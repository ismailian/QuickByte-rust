//! The agent's own single-instance mutex (`Local\QuickByte.QueueAgent`).

/// One agent per signed-in user — "Local\" scopes it to the logon session,
/// matching QuickByte's own single-instance mutex. Two agents would launch the
/// app twice for one schedule. The same name is written down again in
/// `quickbyte_shell::queue_agent_registration::AGENT_MUTEX_NAME`, which is how
/// the app tells whether the agent is running.
pub const MUTEX_NAME: &str = r"Local\QuickByte.QueueAgent";

/// A named mutex, created unowned: existence is the signal, so there is no
/// ownership to release on the wrong thread and nothing to abandon if the
/// process is killed. Closed on drop.
pub struct NamedMutex {
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
    created_new: bool,
}

impl NamedMutex {
    #[cfg(windows)]
    pub fn create(name: &str) -> Option<NamedMutex> {
        use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
        use windows::Win32::System::Threading::CreateMutexW;
        use windows::core::PCWSTR;

        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: the name buffer outlives the call; the last error is read
        // straight after the call that set it.
        unsafe {
            let handle = CreateMutexW(None, false, PCWSTR(wide.as_ptr())).ok()?;
            let created_new = GetLastError() != ERROR_ALREADY_EXISTS;
            Some(NamedMutex { handle, created_new })
        }
    }

    #[cfg(not(windows))]
    pub fn create(_name: &str) -> Option<NamedMutex> {
        Some(NamedMutex { created_new: true })
    }

    /// False when another process already held this name.
    pub fn created_new(&self) -> bool {
        self.created_new
    }
}

impl Drop for NamedMutex {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: the handle is owned by this value and closed once.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_holds_the_name_the_app_watches_for() {
        assert_eq!(MUTEX_NAME, r"Local\QuickByte.QueueAgent");
    }

    #[test]
    fn a_second_holder_of_the_same_name_is_told_so() {
        let name = format!(r"Local\QuickByte.Agent.Tests.{}", uuid::Uuid::new_v4().simple());
        let first = NamedMutex::create(&name).unwrap();
        let second = NamedMutex::create(&name).unwrap();

        assert!(first.created_new());
        assert!(!second.created_new());
    }
}
