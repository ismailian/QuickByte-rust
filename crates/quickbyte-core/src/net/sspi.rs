//! NTLM and Negotiate through Windows SSPI — what `SocketsHttpHandler` did for
//! the C# build once its `Credentials` held the login.
//!
//! Both are a handshake bound to one TCP connection: the client offers a token,
//! the server answers with a challenge token, the client answers that. SSPI
//! (`AcquireCredentialsHandleW` + `InitializeSecurityContextW`) produces each
//! token from the login typed for the server; [`crate::net::authenticated_http`]
//! carries them over a connection of its own. `DOMAIN\user` is split into its
//! domain and user; `user@domain` is passed whole, which is how Windows reads it.

#[cfg(not(windows))]
use crate::error::CoreError;
use crate::error::Result;

/// A Windows login: user, domain (possibly empty), password.
#[derive(Clone, PartialEq, Eq)]
pub struct WindowsLogin {
    pub user: String,
    pub domain: String,
    pub password: String,
}

impl std::fmt::Debug for WindowsLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsLogin").field("user", &self.user).field("domain", &self.domain).finish_non_exhaustive()
    }
}

impl WindowsLogin {
    /// `DOMAIN\user` is split into its domain and user for NTLM and Negotiate;
    /// `user@domain` is passed whole, which is how Windows reads it.
    pub fn from_typed(user_name: &str, password: &str) -> Self {
        match user_name.find('\\') {
            Some(slash) if slash > 0 && slash < user_name.len() - 1 => Self {
                user: user_name[slash + 1..].to_string(),
                domain: user_name[..slash].to_string(),
                password: password.to_string(),
            },
            _ => Self { user: user_name.to_string(), domain: String::new(), password: password.to_string() },
        }
    }
}

/// One NTLM or Negotiate handshake in progress.
pub struct SspiContext {
    #[cfg(windows)]
    inner: windows_impl::Context,
}

impl SspiContext {
    /// Starts a handshake with `package` ("NTLM" or "Negotiate") for `login`,
    /// aimed at the service principal `target` (`HTTP/host`).
    pub fn new(package: &str, login: &WindowsLogin, target: &str) -> Result<Self> {
        #[cfg(windows)]
        {
            Ok(Self { inner: windows_impl::Context::new(package, login, target)? })
        }
        #[cfg(not(windows))]
        {
            let _ = (package, login, target);
            Err(CoreError::other("NTLM and Negotiate need Windows."))
        }
    }

    /// The next token to send, given the server's last one (`None` to begin).
    /// The flag says whether the handshake is complete on this side.
    pub fn step(&mut self, input: Option<&[u8]>) -> Result<(Vec<u8>, bool)> {
        #[cfg(windows)]
        {
            self.inner.step(input)
        }
        #[cfg(not(windows))]
        {
            let _ = input;
            Err(CoreError::other("NTLM and Negotiate need Windows."))
        }
    }
}

#[cfg(windows)]
mod windows_impl {
    use windows::Win32::Foundation::{SEC_E_OK, SEC_I_COMPLETE_AND_CONTINUE, SEC_I_COMPLETE_NEEDED};
    use windows::Win32::Security::Authentication::Identity::{
        AcquireCredentialsHandleW, CompleteAuthToken, DeleteSecurityContext, FreeContextBuffer, FreeCredentialsHandle,
        ISC_REQ_ALLOCATE_MEMORY, ISC_REQ_CONNECTION, InitializeSecurityContextW, SECBUFFER_TOKEN, SECBUFFER_VERSION,
        SECPKG_CRED_OUTBOUND, SECURITY_NATIVE_DREP, SecBuffer, SecBufferDesc,
    };
    use windows::Win32::Security::Credentials::SecHandle;
    use windows::core::PCWSTR;

    use super::WindowsLogin;
    use crate::error::{CoreError, Result};

    /// `SEC_WINNT_AUTH_IDENTITY_W`: lengths in characters, without the terminator.
    #[repr(C)]
    struct AuthIdentity {
        user: *mut u16,
        user_length: u32,
        domain: *mut u16,
        domain_length: u32,
        password: *mut u16,
        password_length: u32,
        flags: u32,
    }
    const SEC_WINNT_AUTH_IDENTITY_UNICODE: u32 = 2;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub struct Context {
        credentials: SecHandle,
        context: Option<SecHandle>,
        target: Vec<u16>,
    }

    // SAFETY: SSPI handles are opaque values that may be used from any thread,
    // one call at a time; a context is only ever driven by the task that owns it.
    unsafe impl Send for Context {}

    impl Context {
        pub fn new(package: &str, login: &WindowsLogin, target: &str) -> Result<Self> {
            let mut user = wide(&login.user);
            let mut domain = wide(&login.domain);
            let mut password = wide(&login.password);
            let identity = AuthIdentity {
                user: user.as_mut_ptr(),
                user_length: (user.len() - 1) as u32,
                domain: domain.as_mut_ptr(),
                domain_length: (domain.len() - 1) as u32,
                password: password.as_mut_ptr(),
                password_length: (password.len() - 1) as u32,
                flags: SEC_WINNT_AUTH_IDENTITY_UNICODE,
            };
            let package = wide(package);
            let mut credentials = SecHandle::default();
            // SAFETY: every pointer in `identity` and `package` borrows a live
            // buffer for the duration of the call, which copies what it needs.
            let result = unsafe {
                AcquireCredentialsHandleW(
                    PCWSTR::null(),
                    PCWSTR(package.as_ptr()),
                    SECPKG_CRED_OUTBOUND,
                    None,
                    Some(&identity as *const AuthIdentity as *const core::ffi::c_void),
                    None,
                    None,
                    &mut credentials,
                    None,
                )
            };
            password.iter_mut().for_each(|c| *c = 0);
            result.map_err(|e| CoreError::other(format!("Windows could not prepare the sign-in: {}", e.message())))?;
            Ok(Self { credentials, context: None, target: wide(target) })
        }

        pub fn step(&mut self, input: Option<&[u8]>) -> Result<(Vec<u8>, bool)> {
            let mut input_bytes = input.map(<[u8]>::to_vec).unwrap_or_default();
            let mut in_buffer = SecBuffer {
                cbBuffer: input_bytes.len() as u32,
                BufferType: SECBUFFER_TOKEN,
                pvBuffer: input_bytes.as_mut_ptr() as *mut _,
            };
            let in_desc = SecBufferDesc { ulVersion: SECBUFFER_VERSION, cBuffers: 1, pBuffers: &mut in_buffer };
            let mut out_buffer = SecBuffer { cbBuffer: 0, BufferType: SECBUFFER_TOKEN, pvBuffer: std::ptr::null_mut() };
            let mut out_desc = SecBufferDesc { ulVersion: SECBUFFER_VERSION, cBuffers: 1, pBuffers: &mut out_buffer };
            let mut new_context = self.context.unwrap_or_default();
            let mut attributes = 0u32;

            // SAFETY: the buffers above outlive the call; SSPI allocates the
            // output token (ISC_REQ_ALLOCATE_MEMORY), released with
            // FreeContextBuffer below.
            let status = unsafe {
                InitializeSecurityContextW(
                    Some(&self.credentials),
                    self.context.as_ref().map(|c| c as *const SecHandle),
                    Some(self.target.as_ptr()),
                    ISC_REQ_ALLOCATE_MEMORY | ISC_REQ_CONNECTION,
                    0,
                    SECURITY_NATIVE_DREP,
                    input.map(|_| &in_desc as *const SecBufferDesc),
                    0,
                    Some(&mut new_context),
                    Some(&mut out_desc),
                    &mut attributes,
                    None,
                )
            };
            if status.is_err() {
                return Err(CoreError::other(format!("Windows could not sign in: {}", windows::core::Error::from(status).message())));
            }
            self.context = Some(new_context);

            if status == SEC_I_COMPLETE_NEEDED || status == SEC_I_COMPLETE_AND_CONTINUE {
                // SAFETY: the context was just established and the descriptor is live.
                unsafe {
                    let _ = CompleteAuthToken(&new_context, &out_desc);
                }
            }

            let token = if out_buffer.pvBuffer.is_null() || out_buffer.cbBuffer == 0 {
                Vec::new()
            } else {
                // SAFETY: SSPI wrote `cbBuffer` bytes at `pvBuffer`, ours to copy then free.
                unsafe {
                    let bytes = std::slice::from_raw_parts(out_buffer.pvBuffer as *const u8, out_buffer.cbBuffer as usize).to_vec();
                    let _ = FreeContextBuffer(out_buffer.pvBuffer);
                    bytes
                }
            };
            let done = status == SEC_E_OK || status == SEC_I_COMPLETE_NEEDED;
            Ok((token, done))
        }
    }

    impl Drop for Context {
        fn drop(&mut self) {
            // SAFETY: both handles came from SSPI and are released exactly once.
            unsafe {
                if let Some(context) = self.context.take() {
                    let _ = DeleteSecurityContext(&context);
                }
                let _ = FreeCredentialsHandle(&self.credentials);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windows_login_is_split_into_domain_and_user() {
        for (typed, user, domain) in [
            ("CORP\\erin", "erin", "CORP"),
            ("erin@corp.example.com", "erin@corp.example.com", ""),
            ("erin", "erin", ""),
            ("\\erin", "\\erin", ""),
        ] {
            let login = WindowsLogin::from_typed(typed, "pw");
            assert_eq!((login.user.as_str(), login.domain.as_str(), login.password.as_str()), (user, domain, "pw"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn ntlm_opens_with_its_negotiate_message() {
        let mut context = SspiContext::new("NTLM", &WindowsLogin::from_typed("CORP\\erin", "pw"), "HTTP/127.0.0.1").unwrap();
        let (token, done) = context.step(None).unwrap();
        assert!(!done);
        // "NTLMSSP\0" then message type 1.
        assert_eq!(&token[..12], b"NTLMSSP\0\x01\0\0\0");
    }
}
