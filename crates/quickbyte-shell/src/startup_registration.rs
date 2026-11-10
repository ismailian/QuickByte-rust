//! Port of `src/QuickByte.UI/StartupRegistration.cs` — Options > System's
//! "Start with Windows".
//!
//! Puts QuickByte in — or takes it out of — the list of programs Windows starts
//! when the user signs in.
//!
//! The entry goes under `HKCU\...\CurrentVersion\Run` rather than the
//! machine-wide `HKLM` hive or a scheduled task: it is a per-user preference, it
//! needs no elevation, and it is the one list Task Manager's Startup tab and
//! Settings > Apps > Startup both show — a user who turns QuickByte off there
//! has to be able to find it.
//!
//! Everything here is best-effort. Group policy, a locked-down profile or a
//! security product can all refuse the write, and none of that is a reason to
//! fail a save or a launch: the checkbox reports what happened and the app
//! carries on. [`sync`] re-asserts the entry on every start so the stored path
//! follows the executable after an update or a move.
//!
//! The preference itself lives in `DownloadSettings`; this is the only place
//! that knows it means a registry value.

use windows_registry::CURRENT_USER;

use crate::win::{process_path, run_key_path};

/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` value name. Also the label Task Manager shows.
pub const VALUE_NAME: &str = "QuickByte";

/// Quoted, because `Run` values are command lines: an unquoted path through
/// `C:\Program Files\...` is read as an attempt to run `C:\Program.exe` with
/// arguments.
fn command_line() -> Option<String> {
    process_path().map(|path| format!("\"{}\"", path.display()))
}

/// Whether the Run value is present and non-empty. An unreadable key reads as
/// "not registered".
pub fn is_enabled() -> bool {
    current_value().is_some_and(|value| !value.is_empty())
}

/// Writes (`"<exe path>"`) or deletes the Run value. Returns the reason when
/// Windows refuses, so the caller can say so instead of silently disagreeing
/// with its own checkbox.
pub fn try_apply(enabled: bool) -> Result<(), String> {
    if quickbyte_core::helpers::app_paths::sandbox().is_some() {
        return Ok(()); // a developer sandbox never touches the real Run key
    }
    let key = CURRENT_USER.create(run_key_path()).map_err(|e| {
        let message = e.message();
        if message.is_empty() { "the Windows startup key could not be opened".to_string() } else { message }
    })?;

    if !enabled {
        // Deleting a value that is not there is the state wanted, not a failure.
        return match key.remove_value(VALUE_NAME) {
            Ok(()) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(e.message()),
        };
    }

    let command = command_line().ok_or_else(|| "the running executable's path is unknown".to_string())?;
    key.set_string(VALUE_NAME, &command).map_err(|e| e.message())
}

/// Brings the registry back in line with the saved preference at startup.
/// Silent by design — nobody asked for it, and the place to be told the write
/// failed is the Options page where the box was ticked.
///
/// It matters on more than a first run: an installed copy that is updated,
/// moved or reinstalled elsewhere leaves a stale path behind, and re-writing the
/// value is what keeps "start with Windows" pointing at the QuickByte that is
/// actually here.
pub fn sync(enabled: bool) {
    if quickbyte_core::helpers::app_paths::sandbox().is_some() {
        return; // a developer sandbox never touches the real Run key
    }
    // Only ever writes when it has to: an unchanged value is a registry write on
    // every launch for nothing, and a delete of a value we never own.
    if !enabled && !is_enabled() {
        return;
    }
    if enabled && current_value() == command_line() {
        return;
    }
    let _ = try_apply(enabled);
}

fn current_value() -> Option<String> {
    CURRENT_USER.open(run_key_path()).ok()?.get_string(VALUE_NAME).ok()
}

pub(crate) fn is_not_found(e: &windows::core::Error) -> bool {
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
    e.code() == ERROR_FILE_NOT_FOUND.to_hresult() || e.code() == ERROR_PATH_NOT_FOUND.to_hresult()
}

#[cfg(test)]
mod tests {
    //! Port of `tests/QuickByte.UI.Tests/StartupRegistrationTests.cs`. Every test
    //! runs against a scratch key — the real one is the user's.

    use super::*;
    use crate::win::test_support::ScratchRunKey;

    fn expected_command() -> String {
        format!("\"{}\"", std::env::current_exe().unwrap().display())
    }

    #[test]
    fn registering_names_this_executable() {
        let key = ScratchRunKey::new();

        assert_eq!(try_apply(true), Ok(()));

        assert_eq!(key.value(VALUE_NAME), Some(expected_command()));
        assert!(is_enabled());
    }

    #[test]
    fn the_path_is_quoted() {
        let key = ScratchRunKey::new();
        try_apply(true).unwrap();

        let value = key.value(VALUE_NAME).unwrap();

        assert!(value.starts_with('"'));
        assert!(value.ends_with('"'));
        assert_eq!(value.trim_matches('"'), std::env::current_exe().unwrap().display().to_string());
    }

    #[test]
    fn the_value_is_the_name_task_manager_shows() {
        assert_eq!(VALUE_NAME, "QuickByte");
    }

    #[test]
    fn deregistering_removes_the_entry() {
        let key = ScratchRunKey::new();
        try_apply(true).unwrap();

        assert_eq!(try_apply(false), Ok(()));

        assert_eq!(key.value(VALUE_NAME), None);
        assert!(!is_enabled());
    }

    #[test]
    fn deregistering_something_that_was_never_registered_is_not_a_failure() {
        let _key = ScratchRunKey::new();

        assert_eq!(try_apply(false), Ok(()));
    }

    #[test]
    fn an_empty_value_does_not_count_as_registered() {
        let key = ScratchRunKey::new();
        key.set_value(VALUE_NAME, "");

        assert!(!is_enabled());
    }

    #[test]
    fn nothing_is_written_to_the_registry_when_there_is_nothing_to_do() {
        // The common case, on every launch of an app the user never asked to
        // start with Windows: it must not create a key, let alone delete a value
        // it does not own.
        let key = ScratchRunKey::new();

        sync(false);

        assert!(!key.exists());
    }

    #[test]
    fn sync_registers_when_the_preference_says_so() {
        let key = ScratchRunKey::new();

        sync(true);

        assert_eq!(key.value(VALUE_NAME), Some(expected_command()));
    }

    #[test]
    fn sync_leaves_exactly_one_entry_however_often_it_runs() {
        let key = ScratchRunKey::new();

        sync(true);
        sync(true);
        sync(true);

        assert_eq!(key.value_names(), vec![VALUE_NAME.to_string()]);
        assert_eq!(key.value(VALUE_NAME), Some(expected_command()));
    }

    #[test]
    fn sync_repoints_an_entry_left_behind_by_an_older_install() {
        let key = ScratchRunKey::new();
        key.set_value(VALUE_NAME, r#""C:\Program Files\QuickByte\QuickByte.exe""#);

        sync(true);

        assert_eq!(key.value(VALUE_NAME), Some(expected_command()));
    }

    #[test]
    fn sync_removes_an_entry_the_user_has_turned_off() {
        let key = ScratchRunKey::new();
        sync(true);

        sync(false);

        assert_eq!(key.value(VALUE_NAME), None);
    }

    #[test]
    fn the_registry_is_what_is_enabled_reports_not_the_saved_preference() {
        let key = ScratchRunKey::new();
        try_apply(true).unwrap();
        assert!(is_enabled());

        key.set_value(VALUE_NAME, "");
        assert!(!is_enabled());
    }

    #[test]
    fn the_real_run_key_is_the_default() {
        assert_eq!(run_key_path(), r"Software\Microsoft\Windows\CurrentVersion\Run");
    }
}
