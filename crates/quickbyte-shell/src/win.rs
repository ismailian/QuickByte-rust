//! Small Win32 helpers shared by the modules of this crate, and the few seams the
//! tests redirect (the Run key path, the running executable's path, the policy
//! key path). Every redirect is a thread-local that only `#[cfg(test)]` code
//! sets, so a test run can never write the user's real `Run` values.

use std::cell::RefCell;
use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::core::{PCWSTR, PWSTR};

/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
pub(crate) const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

thread_local! {
    static RUN_KEY_OVERRIDE: RefCell<Option<String>> = const { RefCell::new(None) };
    static PROCESS_PATH_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// The autostart key both registrars write under. Redirectable for the tests
/// only: the real one is the user's, and a test has no business deleting the
/// entry that starts their QuickByte.
pub(crate) fn run_key_path() -> String {
    RUN_KEY_OVERRIDE.with(|o| o.borrow().clone()).unwrap_or_else(|| RUN_KEY_PATH.to_string())
}

/// `Environment.ProcessPath`: the running executable. Redirectable for the tests,
/// so "is there an agent beside the app?" can be answered both ways without
/// touching the test binary's own folder.
pub(crate) fn process_path() -> Option<PathBuf> {
    if let Some(p) = PROCESS_PATH_OVERRIDE.with(|o| o.borrow().clone()) {
        return Some(p);
    }
    std::env::current_exe().ok().filter(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
pub(crate) fn set_run_key_override(path: Option<String>) {
    RUN_KEY_OVERRIDE.with(|o| *o.borrow_mut() = path);
}

#[cfg(test)]
pub(crate) fn set_process_path_override(path: Option<PathBuf>) {
    PROCESS_PATH_OVERRIDE.with(|o| *o.borrow_mut() = path);
}

/// A NUL-terminated UTF-16 copy of `s`.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `PCWSTR` over a buffer from [`wide`]; the buffer must outlive the call.
pub(crate) fn pcwstr(buffer: &[u16]) -> PCWSTR {
    PCWSTR(buffer.as_ptr())
}

/// Copies `s` into a fixed-size UTF-16 field (NOTIFYICONDATA's), truncated and NUL-terminated.
pub(crate) fn copy_to_fixed(field: &mut [u16], s: &str) {
    let capacity = field.len().saturating_sub(1);
    let mut n = 0;
    for unit in s.encode_utf16().take(capacity) {
        field[n] = unit;
        n += 1;
    }
    for slot in &mut field[n..] {
        *slot = 0;
    }
}

/// `Environment.UserName` — .NET reads `GetUserNameW`, so the pipe and task
/// names built from it match the C# build's exactly.
pub(crate) fn user_name() -> String {
    let mut buffer = [0u16; 257];
    let mut size = buffer.len() as u32;
    // SAFETY: the buffer and its length are valid for the call.
    let ok = unsafe { windows::Win32::System::WindowsProgramming::GetUserNameW(Some(PWSTR(buffer.as_mut_ptr())), &mut size) };
    if ok.is_ok() && size > 1 {
        return String::from_utf16_lossy(&buffer[..size as usize - 1]);
    }
    std::env::var("USERNAME").unwrap_or_default()
}

/// `Environment.SystemDirectory` (`C:\Windows\System32`).
pub(crate) fn system_directory() -> PathBuf {
    let mut buffer = [0u16; 260];
    // SAFETY: the buffer is valid for writes of its length.
    let n = unsafe { windows::Win32::System::SystemInformation::GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    if n > 0 && n < buffer.len() {
        return PathBuf::from(String::from_utf16_lossy(&buffer[..n]));
    }
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    PathBuf::from(root).join("System32")
}

/// The Windows folder (`C:\Windows`), for `explorer.exe` by full path.
pub(crate) fn windows_directory() -> PathBuf {
    let mut buffer = [0u16; 260];
    // SAFETY: the buffer is valid for writes of its length.
    let n = unsafe { windows::Win32::System::SystemInformation::GetWindowsDirectoryW(Some(&mut buffer)) } as usize;
    if n > 0 && n < buffer.len() {
        return PathBuf::from(String::from_utf16_lossy(&buffer[..n]));
    }
    PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()))
}

/// A kernel handle closed on drop.
pub(crate) struct OwnedHandle(pub(crate) HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            // SAFETY: the handle is owned by this value and closed once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Whether a named mutex exists right now (`Mutex.OpenExisting` succeeding).
/// Anything but success — absent, access denied, a malformed name — reads as
/// "not there", as the C# probes did.
pub(crate) fn mutex_exists(name: &str) -> bool {
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
    let name = wide(name);
    // SAFETY: the name buffer outlives the call; the handle is closed at once.
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, pcwstr(&name)) } {
        Ok(handle) => {
            drop(OwnedHandle(handle));
            true
        }
        Err(_) => false,
    }
}

/// Starts a process with no console window, its arguments quoted by std.
pub(crate) fn hidden_command(program: &std::path::Path) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = std::process::Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(test)]
pub(crate) mod test_support {
    //! The Rust shape of `tests/QuickByte.UI.Tests/ScratchRunKey.cs` and
    //! `AgentBesideTheHost.cs`.

    use std::path::{Path, PathBuf};

    use super::*;

    pub(crate) const ROOT: &str = r"Software\QuickByte.Tests";

    /// Registry tests run one at a time: the scratch root is shared, and deleting
    /// it while another thread is creating a key under it would fail that write.
    pub(crate) static REGISTRY_TESTS: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// Deletes `HKCU\Software\QuickByte.Tests` only when it has no subkeys left
    /// (`RegDeleteKeyW` refuses a key with subkeys). Call with [`REGISTRY_TESTS`] held.
    pub(crate) fn remove_root_if_empty() {
        use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteKeyW};
        let root = wide(ROOT);
        // SAFETY: the name buffer outlives the call.
        unsafe {
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, pcwstr(&root));
        }
    }

    /// Points both autostart registrars (on this thread) at a throwaway key under
    /// `HKCU\Software\QuickByte.Tests\<guid>\Run` and deletes it afterwards.
    pub(crate) struct ScratchRunKey {
        pub(crate) path: String,
        parent: String,
        _serial: parking_lot::MutexGuard<'static, ()>,
    }

    impl ScratchRunKey {
        pub(crate) fn new() -> Self {
            let serial = REGISTRY_TESTS.lock();
            let parent = format!(r"{ROOT}\{}", uuid::Uuid::new_v4().simple());
            let path = format!(r"{parent}\Run");
            set_run_key_override(Some(path.clone()));
            Self { path, parent, _serial: serial }
        }

        pub(crate) fn exists(&self) -> bool {
            windows_registry::CURRENT_USER.open(&self.path).is_ok()
        }

        pub(crate) fn value(&self, name: &str) -> Option<String> {
            windows_registry::CURRENT_USER.open(&self.path).ok()?.get_string(name).ok()
        }

        pub(crate) fn set_value(&self, name: &str, value: &str) {
            windows_registry::CURRENT_USER.create(&self.path).unwrap().set_string(name, value).unwrap();
        }

        pub(crate) fn value_names(&self) -> Vec<String> {
            match windows_registry::CURRENT_USER.open(&self.path) {
                Ok(key) => key.values().map(|values| values.map(|(name, _)| name).collect()).unwrap_or_default(),
                Err(_) => Vec::new(),
            }
        }
    }

    impl Drop for ScratchRunKey {
        fn drop(&mut self) {
            set_run_key_override(None);
            let _ = windows_registry::CURRENT_USER.remove_tree(&self.parent);
            remove_root_if_empty();
        }
    }

    /// A fake install folder holding `QuickByte.exe` (an empty file), with or
    /// without the agent beside it, set as this thread's "running executable".
    pub(crate) struct FakeHost {
        pub(crate) folder: PathBuf,
        pub(crate) exe: PathBuf,
    }

    impl FakeHost {
        fn new(agent: Option<&str>) -> Self {
            let folder = std::env::temp_dir().join("QuickByte.UI.Tests").join(uuid::Uuid::new_v4().simple().to_string());
            std::fs::create_dir_all(&folder).unwrap();
            let exe = folder.join("QuickByte.exe");
            std::fs::write(&exe, b"").unwrap();
            if let Some(name) = agent {
                std::fs::write(folder.join(name), b"").unwrap();
            }
            set_process_path_override(Some(exe.clone()));
            Self { folder, exe }
        }

        /// An installed layout: the app and its scheduler side by side.
        pub(crate) fn with_agent() -> Self {
            Self::new(Some(crate::queue_agent_registration::AGENT_FILE_NAME))
        }

        /// A `cargo build` layout: the agent under its cargo name.
        pub(crate) fn with_dev_agent() -> Self {
            Self::new(Some(crate::queue_agent_registration::DEV_AGENT_FILE_NAME))
        }

        /// A build or an install that carries no agent at all.
        pub(crate) fn without_agent() -> Self {
            Self::new(None)
        }

        pub(crate) fn file(&self, name: &str) -> PathBuf {
            self.folder.join(name)
        }

        #[allow(dead_code)]
        pub(crate) fn folder(&self) -> &Path {
            &self.folder
        }
    }

    impl Drop for FakeHost {
        fn drop(&mut self) {
            set_process_path_override(None);
            let _ = std::fs::remove_dir_all(&self.folder);
        }
    }
}
