//! Port of `src/QuickByte.UI/QueueAgentRegistration.cs` — the scheduler agent's
//! Run value, and starting the agent.
//!
//! Keeps QuickByte's scheduler agent (`QuickByte.Agent.exe`) installed and
//! running for exactly as long as there is a schedule for it to watch.
//!
//! The agent is the answer to the obvious hole in queue scheduling: a queue set
//! to start at 03:00 cannot start itself if the download manager was closed at
//! midnight. The agent is a separate process that outlives QuickByte, starts
//! with the user's session, and launches QuickByte when a queue comes due.
//!
//! It is registered the same way QuickByte itself is — a value under
//! `HKCU\...\CurrentVersion\Run`. That is a per-user, no-elevation autostart at
//! sign-in rather than a machine-wide Windows service at boot, and the choice is
//! deliberate: a download manager that needs an administrator prompt (or a
//! service running as SYSTEM, downloading into a user's profile, with that
//! user's cookies) to schedule a queue is not a download manager anyone should
//! install. Sign-in is also the earliest moment a per-user download queue means
//! anything.
//!
//! Both directions are best-effort and silent, matching
//! [`crate::startup_registration`]: group policy or a security product may
//! refuse the write, and a queue window is not the place to learn about it.

use std::path::PathBuf;

use windows_registry::CURRENT_USER;

use crate::win::{mutex_exists, process_path, run_key_path};

/// Value name under the Run key, and the label Task Manager shows.
pub const VALUE_NAME: &str = "QuickByteScheduler";
pub const AGENT_FILE_NAME: &str = "QuickByte.Agent.exe";
/// The cargo build's name for the same binary; accepted so a dev build finds its agent.
pub const DEV_AGENT_FILE_NAME: &str = "quickbyte-agent.exe";
/// Held by the agent for its whole life — see its `main.rs`.
pub const AGENT_MUTEX_NAME: &str = r"Local\QuickByte.QueueAgent";

/// The agent as installed beside this executable. Resolved from the running
/// app's own folder so an update, or a copy run from somewhere else, uses the
/// agent that shipped with it rather than one left behind by a previous install.
/// The shipped name wins; the cargo name (`quickbyte-agent.exe`) is accepted so
/// a `cargo run` build finds the agent it was built with.
pub fn executable_path() -> Option<PathBuf> {
    let folder = process_path()?.parent()?.to_path_buf();
    if folder.as_os_str().is_empty() {
        return None;
    }
    [AGENT_FILE_NAME, DEV_AGENT_FILE_NAME].into_iter().map(|name| folder.join(name)).find(|path| path.is_file())
}

/// False when QuickByte is running from a build or an install that does not
/// include the agent. Scheduling still works while the app is open — the
/// in-process scheduler is not the agent's job — so this is a reason to say
/// less in the queue window, not a reason to fail.
pub fn is_available() -> bool {
    executable_path().is_some()
}

/// Whether the agent's mutex exists.
pub fn is_running() -> bool {
    mutex_exists(&format!("{AGENT_MUTEX_NAME}{}", quickbyte_core::helpers::app_paths::instance_suffix()))
}

/// Brings the agent in line with whether anything is scheduled: registered and
/// running when it is, deregistered when it is not.
///
/// Nothing here kills a running agent. It exits on its own once the queue file
/// holds no schedules — which it reads from the same file this app just wrote —
/// and letting it notice beats reaching into another process to end it.
pub fn sync(schedule_exists: bool) {
    if quickbyte_core::helpers::app_paths::sandbox().is_some() {
        return; // a developer sandbox registers and starts no agent
    }
    register(schedule_exists);
    if schedule_exists {
        ensure_running();
    }
}

/// Starts the agent now, so a schedule set this afternoon is watched this
/// afternoon rather than from the next sign-in. Does nothing if one is already
/// running — the agent is single-instance itself, so a duplicate launch would
/// exit immediately anyway; this only avoids the process creation.
pub fn ensure_running() {
    if is_running() {
        return;
    }
    let Some(path) = executable_path() else { return };
    let mut command = std::process::Command::new(&path);
    if let Some(folder) = path.parent() {
        command.current_dir(folder);
    }
    // Best-effort: the in-app scheduler still covers every queue for as long as
    // this window is open. The child handle is dropped, not waited on.
    let _ = command.spawn();
}

/// The registry half of [`sync`], on its own — crate-visible so it can be
/// tested without the other half, which starts a process.
pub(crate) fn register(enabled: bool) {
    let Ok(key) = CURRENT_USER.create(run_key_path()) else { return };

    if !enabled {
        let _ = key.remove_value(VALUE_NAME);
        return;
    }

    let Some(path) = executable_path() else { return };

    // Quoted: Run values are command lines, and an unquoted path through
    // "Program Files" is read as C:\Program.exe with arguments.
    let command = format!("\"{}\"", path.display());
    if key.get_string(VALUE_NAME).ok().as_deref() == Some(command.as_str()) {
        return;
    }
    let _ = key.set_string(VALUE_NAME, &command);
}

#[cfg(test)]
mod tests {
    //! Port of `tests/QuickByte.UI.Tests/QueueAgentRegistrationTests.cs`. `sync`
    //! itself is deliberately not called: its other half starts a process.

    use super::*;
    use crate::startup_registration;
    use crate::win::test_support::{FakeHost, ScratchRunKey};

    #[test]
    fn the_agent_is_looked_for_beside_the_running_executable() {
        let host = FakeHost::with_agent();

        assert_eq!(executable_path(), Some(host.file(AGENT_FILE_NAME)));
        assert!(is_available());
    }

    #[test]
    fn a_dev_build_finds_the_agent_under_its_cargo_name() {
        let host = FakeHost::with_dev_agent();

        assert_eq!(executable_path(), Some(host.file(DEV_AGENT_FILE_NAME)));
    }

    #[test]
    fn the_shipped_name_wins_over_the_cargo_name() {
        let host = FakeHost::with_agent();
        std::fs::write(host.file(DEV_AGENT_FILE_NAME), b"").unwrap();

        assert_eq!(executable_path(), Some(host.file(AGENT_FILE_NAME)));
    }

    #[test]
    fn the_agent_that_shipped_with_this_copy_is_the_one_registered() {
        let key = ScratchRunKey::new();
        let _host = FakeHost::with_agent();

        register(true);

        assert_eq!(key.value(VALUE_NAME), Some(format!("\"{}\"", executable_path().unwrap().display())));
    }

    #[test]
    fn no_agent_beside_the_app_means_no_entry_that_would_start_one() {
        let key = ScratchRunKey::new();
        let _host = FakeHost::without_agent();

        assert_eq!(executable_path(), None);
        assert!(!is_available());

        register(true);

        assert_eq!(key.value(VALUE_NAME), None);
    }

    #[test]
    fn a_registered_path_is_quoted() {
        let key = ScratchRunKey::new();
        let _host = FakeHost::with_agent();
        key.set_value(VALUE_NAME, r"C:\Program Files\QuickByte\QuickByte.Agent.exe");

        register(true);

        let value = key.value(VALUE_NAME).unwrap();
        assert!(value.starts_with('"'));
        assert!(value.ends_with('"'));
        assert_eq!(value.trim_matches('"'), executable_path().unwrap().display().to_string());
    }

    #[test]
    fn deregistering_removes_whatever_was_there() {
        let key = ScratchRunKey::new();
        key.set_value(VALUE_NAME, r#""C:\somewhere\QuickByte.Agent.exe""#);

        register(false);

        assert_eq!(key.value(VALUE_NAME), None);
    }

    #[test]
    fn deregistering_when_nothing_is_registered_is_harmless() {
        let key = ScratchRunKey::new();

        register(false);

        assert!(key.value_names().is_empty());
    }

    #[test]
    fn the_apps_entry_is_not_touched_by_the_agents() {
        let _key = ScratchRunKey::new();
        startup_registration::try_apply(true).unwrap();

        register(false);

        assert!(startup_registration::is_enabled());
    }

    #[test]
    fn the_agent_is_named_and_watched_for_exactly_as_the_agent_names_itself() {
        assert_eq!(AGENT_FILE_NAME, "QuickByte.Agent.exe");
        assert_eq!(AGENT_MUTEX_NAME, r"Local\QuickByte.QueueAgent");
    }

    #[test]
    fn the_scheduler_has_its_own_row_in_task_manager() {
        assert_eq!(VALUE_NAME, "QuickByteScheduler");
        assert_ne!(VALUE_NAME, startup_registration::VALUE_NAME);
    }
}
