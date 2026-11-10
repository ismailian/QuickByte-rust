//! Port of `src/QuickByte.UI/ZoneInformationPolicy.cs`.
//!
//! Whether Windows is configured to keep zone information on downloaded files —
//! the Attachment Manager policy "Do not preserve zone information in file
//! attachments", which is `SaveZoneInformation` under
//! `...\CurrentVersion\Policies\Attachments`.
//!
//! QuickByte marks every finished download as coming from the internet
//! (Core's `MarkOfTheWeb`), the way the browsers do. An administrator who turned
//! that off for the browsers has turned it off for this too, and Chrome honours
//! the same policy.
//!
//! Read here rather than in Core because it is a registry value, and the
//! registry belongs to this crate like every other Windows concept; Core is
//! handed the answer. Read once per launch, like the rest of the snapshotted
//! configuration.

use std::cell::RefCell;

use windows_registry::{CURRENT_USER, Key, LOCAL_MACHINE, Type};

const POLICY_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Attachments";
pub(crate) const VALUE_NAME: &str = "SaveZoneInformation";

thread_local! {
    /// Redirected only by the tests, to a scratch key instead of the real policy.
    static POLICY_KEY_OVERRIDE: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn policy_key_path() -> String {
    POLICY_KEY_OVERRIDE.with(|o| o.borrow().clone()).unwrap_or_else(|| POLICY_KEY_PATH.to_string())
}

/// False when the Attachment Manager policy "Do not preserve zone information"
/// (`SaveZoneInformation = 1`) is set, machine policy first.
///
/// True unless a policy says not to preserve zone information. The machine
/// policy wins over the user's, as it does for any Group Policy setting.
/// Anything unreadable, or any value other than the two the policy defines,
/// reads as "preserve": the safe default is the one with the warnings.
pub fn preserves_zone_information() -> bool {
    read(LOCAL_MACHINE).or_else(|| read(CURRENT_USER)).unwrap_or(true)
}

fn read(hive: &Key) -> Option<bool> {
    let key = hive.open(policy_key_path()).ok()?;
    if key.get_type(VALUE_NAME).ok()? != Type::U32 {
        return None;
    }
    match key.get_u32(VALUE_NAME).ok()? {
        1 => Some(false), // "Do not preserve zone information"
        2 => Some(true),  // "Preserve zone information"
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    //! Port of `tests/QuickByte.UI.Tests/ZoneInformationPolicyTests.cs`. Read
    //! from a scratch key under `HKCU\Software\QuickByte.Tests` — the machine
    //! half of the lookup then finds nothing at that path, which is also what
    //! exercises the fallback to the user's policy.

    use super::*;
    use crate::win::test_support::{REGISTRY_TESTS, ROOT, remove_root_if_empty};

    struct ScratchPolicy {
        parent: String,
        path: String,
        _serial: parking_lot::MutexGuard<'static, ()>,
    }

    impl ScratchPolicy {
        fn new() -> Self {
            let serial = REGISTRY_TESTS.lock();
            let parent = format!(r"{ROOT}\{}", uuid::Uuid::new_v4().simple());
            let path = format!(r"{parent}\Attachments");
            POLICY_KEY_OVERRIDE.with(|o| *o.borrow_mut() = Some(path.clone()));
            Self { parent, path, _serial: serial }
        }
        fn set(&self, value: u32) {
            CURRENT_USER.create(&self.path).unwrap().set_u32(VALUE_NAME, value).unwrap();
        }
    }

    impl Drop for ScratchPolicy {
        fn drop(&mut self) {
            POLICY_KEY_OVERRIDE.with(|o| *o.borrow_mut() = None);
            let _ = CURRENT_USER.remove_tree(&self.parent);
            remove_root_if_empty();
        }
    }

    #[test]
    fn without_a_policy_downloads_are_marked() {
        let _policy = ScratchPolicy::new();
        assert!(preserves_zone_information());
    }

    #[test]
    fn the_policy_that_turns_zone_information_off_turns_it_off_here_too() {
        let policy = ScratchPolicy::new();
        policy.set(1);
        assert!(!preserves_zone_information());
    }

    #[test]
    fn the_policy_that_asks_for_zone_information_keeps_it_on() {
        let policy = ScratchPolicy::new();
        policy.set(2);
        assert!(preserves_zone_information());
    }

    #[test]
    fn a_value_the_policy_does_not_define_is_read_as_the_safe_default() {
        let policy = ScratchPolicy::new();
        policy.set(7);
        assert!(preserves_zone_information());

        CURRENT_USER.create(&policy.path).unwrap().set_string(VALUE_NAME, "1").unwrap();
        assert!(preserves_zone_information());
    }
}
