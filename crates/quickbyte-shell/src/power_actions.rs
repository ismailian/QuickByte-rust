//! Port of `src/QuickByte.UI/PowerActions.cs`.
//!
//! Carries out a [`PowerAction`]: sleep, hibernate, shut down. Exiting QuickByte
//! is the one action that is not here — it belongs to the main window, whose
//! exit path pauses the downloads first.
//!
//! Sleep and hibernate are `SetSuspendState` (what WinForms'
//! `Application.SetSuspendState` wraps), and shutting down is Windows' own
//! `shutdown.exe` — which also means a shutdown this starts behaves like any
//! other: it is not forced, so an app with unsaved work still gets to say so.

use std::path::PathBuf;

use quickbyte_core::enums::PowerAction;

use crate::win::{hidden_command, system_directory};

/// How long the countdown in front of an action lasts: 15 s for ExitApp, 60 s
/// for the rest. Long enough to reach the keyboard from across the room for
/// anything that turns the machine off; shorter to close an app.
pub fn countdown_seconds(action: PowerAction) -> u32 {
    if action == PowerAction::ExitApp { 15 } else { 60 }
}

/// The choice as a drop-down shows it, e.g. "Shut down the computer".
pub fn label(action: PowerAction) -> &'static str {
    match action {
        PowerAction::ExitApp => "Exit QuickByte",
        PowerAction::Sleep => "Put the computer to sleep",
        PowerAction::Hibernate => "Hibernate the computer",
        PowerAction::ShutDown => "Shut down the computer",
        PowerAction::None => "Do nothing",
    }
}

/// The countdown's sentence: "QuickByte will … in 42 seconds."
pub fn verb(action: PowerAction) -> &'static str {
    match action {
        PowerAction::ExitApp => "exit",
        PowerAction::Sleep => "put the computer to sleep",
        PowerAction::Hibernate => "hibernate the computer",
        PowerAction::ShutDown => "shut down the computer",
        PowerAction::None => "do nothing",
    }
}

/// The countdown window's "do it now" button text.
pub fn now_button(action: PowerAction) -> &'static str {
    match action {
        PowerAction::ExitApp => "Exit Now",
        PowerAction::Sleep => "Sleep Now",
        PowerAction::Hibernate => "Hibernate Now",
        PowerAction::ShutDown => "Shut Down Now",
        PowerAction::None => "OK",
    }
}

/// `shutdown.exe` by its full path in the system directory, never by name: a
/// name is resolved against the working directory and PATH first, and a download
/// manager's working directory is a very good place to find a file called
/// shutdown.exe that someone else put there.
pub(crate) fn shutdown_command() -> (PathBuf, [&'static str; 3]) {
    (system_directory().join("shutdown.exe"), ["/s", "/t", "0"])
}

/// Sleep / Hibernate (SetSuspendState) or ShutDown (`%SystemRoot%\System32\shutdown.exe`,
/// by full path, no `/f`). ExitApp is the UI's business and returns false here.
///
/// Returns whether Windows accepted it — a machine with hibernation switched off
/// refuses, and saying so beats leaving the user to find a computer still on.
pub fn perform(action: PowerAction) -> bool {
    use windows::Win32::System::Power::SetSuspendState;
    match action {
        // SAFETY: plain calls; force = false, wake events stay enabled.
        PowerAction::Sleep => unsafe { SetSuspendState(false, false, false) },
        PowerAction::Hibernate => unsafe { SetSuspendState(true, false, false) },
        PowerAction::ShutDown => {
            let (program, arguments) = shutdown_command();
            hidden_command(&program).args(arguments).spawn().is_ok()
        }
        PowerAction::ExitApp | PowerAction::None => false,
    }
}

#[cfg(test)]
mod tests {
    //! Port of `tests/QuickByte.UI.Tests/PowerActionsTests.cs`. Nothing here
    //! calls [`perform`].

    use super::*;

    #[test]
    fn shutdown_runs_the_system_copy_of_shutdown_exe_by_its_full_path() {
        let (program, _) = shutdown_command();

        assert_eq!(program, system_directory().join("shutdown.exe"));
        assert!(program.is_absolute());
        assert!(program.is_file(), "{}", program.display());
    }

    #[test]
    fn shutdown_never_forces_apps_with_unsaved_work_closed() {
        let (_, arguments) = shutdown_command();

        assert!(arguments.contains(&"/s"));
        assert!(!arguments.contains(&"/f"));
        assert!(!arguments.contains(&"/p"));
    }

    #[test]
    fn every_action_has_its_own_words_for_the_countdown() {
        let actions: Vec<PowerAction> = PowerAction::ALL.into_iter().filter(|a| *a != PowerAction::None).collect();
        let distinct = |f: fn(PowerAction) -> &'static str| {
            let mut words: Vec<&str> = actions.iter().map(|a| f(*a)).collect();
            words.sort();
            words.dedup();
            words.len()
        };

        assert_eq!(distinct(label), actions.len());
        assert_eq!(distinct(verb), actions.len());
        assert_eq!(distinct(now_button), actions.len());
        assert!(actions.iter().all(|a| label(*a) != label(PowerAction::None)));
    }

    #[test]
    fn the_words_are_the_csharp_builds() {
        assert_eq!(label(PowerAction::ShutDown), "Shut down the computer");
        assert_eq!(verb(PowerAction::Sleep), "put the computer to sleep");
        assert_eq!(now_button(PowerAction::Hibernate), "Hibernate Now");
        assert_eq!(label(PowerAction::None), "Do nothing");
    }

    #[test]
    fn turning_the_computer_off_gives_longer_to_reach_the_keyboard_than_closing_the_app() {
        assert!(countdown_seconds(PowerAction::ShutDown) >= 60);
        assert!(countdown_seconds(PowerAction::Sleep) >= 60);
        assert!(countdown_seconds(PowerAction::ExitApp) > 0);
        assert!(countdown_seconds(PowerAction::ExitApp) < countdown_seconds(PowerAction::ShutDown));
    }
}
