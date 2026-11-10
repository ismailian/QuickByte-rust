//! Port of `src/QuickByte.UI/Controls/KeepAwake.cs`.
//!
//! Keeps Windows from falling asleep while downloads are running.
//!
//! A queue woken at 03:00 by [`crate::queue_wake_registration`] is only half a
//! feature if the machine goes back to sleep two minutes later — which is exactly
//! what Windows does after a wake nobody touched the keyboard for. The same idle
//! timer ends an ordinary overnight download an hour in.
//!
//! It uses a power request rather than `SetThreadExecutionState`, because a
//! request carries a reason — `powercfg /requests` names QuickByte and says why,
//! which is the first thing anyone asks when a PC will not sleep — and is not
//! tied to the thread that made it. It only holds off *idle* sleep: closing the
//! lid, the power button and Start > Sleep still work.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Power::{PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest};
use windows::Win32::System::Threading::{POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0};
use windows::core::PWSTR;

use crate::win::{OwnedHandle, wide};

/// `POWER_REQUEST_CONTEXT_VERSION`.
const POWER_REQUEST_CONTEXT_VERSION: u32 = 0;

/// Holds a power request while downloads run (idle sleep only; the lid, the power
/// button and Start > Sleep still work). Released on drop.
pub struct KeepAwake {
    /// The power request handle, created on first use (0 = none).
    request: isize,
    held: bool,
}

impl KeepAwake {
    pub fn new() -> Self {
        Self { request: 0, held: false }
    }

    /// Whether a request is currently held.
    pub fn is_held(&self) -> bool {
        self.held
    }

    /// Takes the request when `downloading`, releases it otherwise. Cheap when
    /// nothing changes. Best-effort: a machine that sleeps is the behaviour there
    /// was before.
    pub fn update(&mut self, downloading: bool) {
        if downloading == self.held {
            return;
        }
        if downloading {
            if self.request == 0 {
                self.request = create_request();
            }
            // SAFETY: the handle is owned by this value.
            if self.request != 0 && unsafe { PowerSetRequest(HANDLE(self.request as _), PowerRequestSystemRequired) }.is_ok() {
                self.held = true;
            }
        } else {
            if self.request != 0 {
                // SAFETY: the handle is owned by this value.
                let _ = unsafe { PowerClearRequest(HANDLE(self.request as _), PowerRequestSystemRequired) };
            }
            self.held = false;
        }
    }
}

fn create_request() -> isize {
    let mut reason = wide("QuickByte is downloading");
    let context = REASON_CONTEXT {
        Version: POWER_REQUEST_CONTEXT_VERSION,
        Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
        Reason: REASON_CONTEXT_0 { SimpleReasonString: PWSTR(reason.as_mut_ptr()) },
    };
    // SAFETY: the context and its string outlive the call; the kernel copies the reason.
    match unsafe { PowerCreateRequest(&context) } {
        Ok(handle) if !handle.is_invalid() => handle.0 as isize,
        _ => 0,
    }
}

impl Default for KeepAwake {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        self.update(false);
        if self.request != 0 {
            drop(OwnedHandle(HANDLE(self.request as _)));
            self.request = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    //! Port of `KeepAwakeTests` (in `QueueWakeRegistrationTests.cs`).

    use super::*;

    #[test]
    fn a_request_is_held_only_while_downloads_run_and_released_on_drop() {
        let mut keep_awake = KeepAwake::new();
        assert!(!keep_awake.is_held());

        keep_awake.update(true);
        assert!(keep_awake.is_held());

        keep_awake.update(true);
        assert!(keep_awake.is_held());

        keep_awake.update(false);
        assert!(!keep_awake.is_held());

        keep_awake.update(true);
        drop(keep_awake);
    }
}
