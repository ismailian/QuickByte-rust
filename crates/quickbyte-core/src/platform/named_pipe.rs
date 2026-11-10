//! Current-user-only named pipes — what `PipeOptions.CurrentUserOnly` gave the
//! C# `CommandPipe`.
//!
//! Both ends hold the line:
//!
//! * **The server** is created with a security descriptor whose owner is the
//!   current user and whose protected DACL grants access to the current user's
//!   SID and nobody else, and it refuses remote clients. Another account on the
//!   machine cannot open it.
//! * **The client** checks, before it says anything, that the pipe it reached is
//!   owned by the current user. Another account that created a pipe of the same
//!   name first (pipe names are machine-wide) cannot impersonate QuickByte to `qb`.

use std::io;
use std::time::Duration;

#[cfg(windows)]
pub use imp::*;

/// `\\.\pipe\<name>`.
pub fn pipe_path(name: &str) -> String {
    format!(r"\\.\pipe\{name}")
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions};
    use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
    };
    use windows::Win32::Security::{
        EqualSid, GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::{PCWSTR, PWSTR};

    use super::*;

    /// ERROR_PIPE_BUSY: every instance is serving someone; try again shortly.
    const ERROR_PIPE_BUSY: i32 = 231;

    /// `Environment.UserName` — .NET reads `GetUserNameW`, so pipe names built
    /// from it match the C# build's exactly.
    pub fn current_user_name() -> String {
        let mut buffer = [0u16; 257];
        let mut size = buffer.len() as u32;
        // SAFETY: the buffer and its length are valid for the call.
        let ok = unsafe { windows::Win32::System::WindowsProgramming::GetUserNameW(Some(PWSTR(buffer.as_mut_ptr())), &mut size) };
        if ok.is_ok() && size > 1 {
            return String::from_utf16_lossy(&buffer[..size as usize - 1]);
        }
        std::env::var("USERNAME").unwrap_or_default()
    }

    /// The current process's user SID, as bytes holding a `TOKEN_USER`.
    struct UserSid {
        buffer: Vec<u64>,
    }

    impl UserSid {
        fn current() -> io::Result<Self> {
            // SAFETY: the pseudo handle needs no closing; the token handle is
            // closed below; the buffer is sized by the first call and 8-aligned.
            unsafe {
                let mut token = HANDLE::default();
                OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(io::Error::other)?;
                let mut needed = 0u32;
                let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
                let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
                let result = GetTokenInformation(token, TokenUser, Some(buffer.as_mut_ptr().cast()), (buffer.len() * 8) as u32, &mut needed);
                let _ = CloseHandle(token);
                result.map_err(io::Error::other)?;
                Ok(Self { buffer })
            }
        }

        fn sid(&self) -> PSID {
            // SAFETY: the buffer holds a TOKEN_USER written by GetTokenInformation.
            unsafe { (*(self.buffer.as_ptr() as *const TOKEN_USER)).User.Sid }
        }

        fn to_sddl(&self) -> io::Result<String> {
            // SAFETY: the SID is valid while self lives; the string is freed with LocalFree.
            unsafe {
                let mut text = PWSTR::null();
                ConvertSidToStringSidW(self.sid(), &mut text).map_err(io::Error::other)?;
                let value = text.to_string().map_err(io::Error::other);
                let _ = LocalFree(Some(HLOCAL(text.0.cast())));
                value
            }
        }
    }

    /// Creates one instance of the pipe `name`, open only to the current user and
    /// to local clients. Byte mode, one instance at a time.
    pub fn create_server(name: &str) -> io::Result<NamedPipeServer> {
        let sid = UserSid::current()?.to_sddl()?;
        // Owner: this user. DACL, protected from inheritance: full access for
        // this user and no one else.
        let sddl: Vec<u16> = format!("O:{sid}D:P(A;;GA;;;{sid})").encode_utf16().chain(Some(0)).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the SDDL string is NUL-terminated; the descriptor it returns is
        // freed with LocalFree once the pipe exists (CreateNamedPipe copies it).
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut descriptor, None)
                .map_err(io::Error::other)?;
            let mut attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: false.into(),
            };
            let server = ServerOptions::new()
                .access_inbound(true)
                .access_outbound(true)
                .pipe_mode(PipeMode::Byte)
                .reject_remote_clients(true)
                .max_instances(1)
                .create_with_security_attributes_raw(pipe_path(name), &mut attributes as *mut SECURITY_ATTRIBUTES as *mut c_void);
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
            server
        }
    }

    /// Opens the pipe `name`, waiting up to `timeout` for a server to listen.
    /// `Ok(None)` when nobody did. Refuses (`PermissionDenied`) a pipe that is not
    /// owned by the current user.
    pub async fn connect(name: &str, timeout: Duration) -> io::Result<Option<NamedPipeClient>> {
        let deadline = tokio::time::Instant::now() + timeout;
        let path = pipe_path(name);
        let client = loop {
            match ClientOptions::new().open(&path) {
                Ok(client) => break client,
                Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Ok(None);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        };
        if !owned_by_current_user(client.as_raw_handle()) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "The pipe is not owned by this user."));
        }
        Ok(Some(client))
    }

    /// Whether the kernel object behind `handle` is owned by the current user.
    pub fn owned_by_current_user(handle: std::os::windows::io::RawHandle) -> bool {
        let Ok(user) = UserSid::current() else { return false };
        let mut owner = PSID::default();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the owner SID points into the descriptor, which is freed after
        // the comparison.
        unsafe {
            let status = GetSecurityInfo(
                HANDLE(handle),
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                Some(&mut owner),
                None,
                None,
                None,
                Some(&mut descriptor),
            );
            if status != ERROR_SUCCESS {
                return false;
            }
            let same = !owner.0.is_null() && EqualSid(owner, user.sid()).is_ok();
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
            same
        }
    }
}

#[cfg(not(windows))]
pub fn current_user_name() -> String {
    std::env::var("USER").unwrap_or_default()
}

#[cfg(not(windows))]
pub async fn connect(_name: &str, _timeout: Duration) -> io::Result<Option<()>> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Named pipes are a Windows feature."))
}

#[cfg(all(test, windows))]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn unique() -> String {
        format!("QuickByte.Test.Pipe.{}", uuid::Uuid::new_v4().simple())
    }

    #[tokio::test]
    async fn a_pipe_of_this_user_is_reached_and_carries_bytes_both_ways() {
        let name = unique();
        let server = create_server(&name).unwrap();
        let serving = tokio::spawn(async move {
            server.connect().await.unwrap();
            let mut server = server;
            let mut buffer = [0u8; 4];
            server.read_exact(&mut buffer).await.unwrap();
            server.write_all(b"pong").await.unwrap();
            buffer
        });

        let mut client = connect(&name, Duration::from_secs(5)).await.unwrap().expect("the server listens");
        client.write_all(b"ping").await.unwrap();
        let mut reply = [0u8; 4];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"pong");
        assert_eq!(&serving.await.unwrap(), b"ping");
    }

    #[tokio::test]
    async fn nobody_listening_is_none_within_the_timeout() {
        let started = std::time::Instant::now();
        assert!(connect(&unique(), Duration::from_millis(100)).await.unwrap().is_none());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn the_server_is_owned_by_this_user() {
        let name = unique();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = runtime.enter();
        let server = create_server(&name).unwrap();
        assert!(owned_by_current_user(std::os::windows::io::AsRawHandle::as_raw_handle(&server)));
    }

    #[test]
    fn the_user_name_is_known() {
        assert!(!current_user_name().is_empty());
    }
}
