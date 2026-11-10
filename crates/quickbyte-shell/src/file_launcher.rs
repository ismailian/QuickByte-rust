//! Port of `src/QuickByte.UI/Controls/FileLauncher.cs`.
//!
//! Shell hand-offs (open a downloaded file, reveal it in Explorer) shared by the
//! main window, the details window and the completion window. Every call is
//! best-effort: a missing file or a shell association the user hasn't set is not
//! worth an error dialog in a download manager.

use std::os::windows::process::CommandExt;
use std::path::Path;

use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONASTERISK, SW_SHOWNORMAL};
use windows::core::PCWSTR;

use crate::win::{pcwstr, wide, windows_directory};

/// `Process.Start(new ProcessStartInfo(target) { UseShellExecute = true })`:
/// the default verb, as Explorer's double-click would.
fn shell_execute(target: &str) -> bool {
    let file = wide(target);
    // SAFETY: the string buffer outlives the call; null verb, parameters and directory.
    let result = unsafe { ShellExecuteW(None, PCWSTR::null(), pcwstr(&file), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    // ShellExecute's documented success test: a value greater than 32.
    result.0 as isize > 32
}

/// Opens a finished file with its default program.
pub fn open_file(full_path: &str) -> bool {
    if full_path.trim().is_empty() || !Path::new(full_path).is_file() {
        return false;
    }
    shell_execute(full_path)
}

/// Runs the downloaded installer (ShellExecute, so UAC can prompt).
///
/// Hands a downloaded installer to the shell. Going through the shell is
/// load-bearing rather than incidental here: it is what lets Windows raise the
/// elevation prompt setup needs.
///
/// Unlike the rest of this module this one reports whether it worked, because
/// the caller closes QuickByte on the strength of it — and a user who dismisses
/// the UAC prompt must not be left with neither the old app nor the new one.
pub fn run_installer(installer_path: &str) -> bool {
    if installer_path.trim().is_empty() || !Path::new(installer_path).is_file() {
        return false;
    }
    shell_execute(installer_path)
}

/// Opens an http/https page in the default browser. Anything else is refused.
///
/// The address comes from a captured `Referer` header, which is whatever the
/// browser extension was sent, and handing the shell a `file:` or
/// custom-protocol URL from there would run whatever it names.
pub fn open_web_page(url: &str) -> bool {
    let Ok(uri) = url::Url::parse(url) else { return false };
    if !matches!(uri.scheme(), "http" | "https") || uri.host_str().is_none_or(str::is_empty) {
        return false;
    }
    shell_execute(uri.as_str())
}

/// Selects the file in Explorer, or opens `fallback_folder` when the file is gone.
/// Neither existing means nothing to show — opening some other folder instead
/// would be worse than nothing.
pub fn reveal_in_explorer(full_path: &str, fallback_folder: &str) {
    let explorer = windows_directory().join("explorer.exe");
    if !full_path.is_empty() && Path::new(full_path).is_file() {
        let _ = std::process::Command::new(&explorer).raw_arg(format!("/select,\"{full_path}\"")).spawn();
    } else if !fallback_folder.is_empty() && Path::new(fallback_folder).is_dir() {
        let _ = std::process::Command::new(&explorer).raw_arg(format!("\"{fallback_folder}\"")).spawn();
    }
}

/// Opens a folder in Explorer.
pub fn open_folder(folder: &str) -> bool {
    if folder.trim().is_empty() || !Path::new(folder).is_dir() {
        return false;
    }
    shell_execute(folder)
}

/// The system notification sound (`SystemSounds.Asterisk`).
pub fn play_notification_sound() {
    // SAFETY: a plain call.
    unsafe {
        let _ = MessageBeep(MB_ICONASTERISK);
    }
}

#[cfg(test)]
mod tests {
    //! Port of `tests/QuickByte.UI.Tests/FileLauncherTests.cs`. Only the paths
    //! that don't start anything are exercised: a test that opened a file would
    //! launch whatever the user has associated with it.

    use super::*;

    fn missing(name: &str) -> String {
        std::env::temp_dir()
            .join("QuickByte.UI.Tests")
            .join(uuid::Uuid::new_v4().simple().to_string())
            .join(name)
            .display()
            .to_string()
    }

    #[test]
    fn opening_a_file_that_is_gone_does_nothing_at_all() {
        assert!(!open_file(&missing("gone.zip")));
    }

    #[test]
    fn opening_a_path_that_is_not_one_does_nothing() {
        assert!(!open_file(""));
        assert!(!open_file("   "));
    }

    #[test]
    fn a_web_page_that_is_not_a_web_page_is_never_handed_to_the_shell() {
        for url in ["file:///C:/Windows/System32/calc.exe", "javascript:alert(1)", "ms-settings:privacy", r"C:\Windows\notepad.exe", "not a url", ""] {
            assert!(!open_web_page(url), "{url}");
        }
    }

    #[test]
    fn revealing_something_that_is_gone_does_not_open_a_window() {
        reveal_in_explorer(&missing("gone.zip"), &missing("no-folder"));
    }

    #[test]
    fn an_installer_that_is_not_there_reports_failure_rather_than_swallowing_it() {
        assert!(!run_installer(&missing("QuickByte-9.9.9-x64.exe")));
    }

    #[test]
    fn a_nonsense_installer_path_is_a_false_not_an_exception() {
        for path in ["", "   ", r"Z:\nowhere\setup.exe"] {
            assert!(!run_installer(path), "{path}");
        }
    }

    #[test]
    fn a_folder_that_is_not_there_is_not_opened() {
        assert!(!open_folder(&missing("no-folder")));
        assert!(!open_folder(""));
    }
}
