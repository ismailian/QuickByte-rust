//! `IAppLauncher` and `QuickByteApp` from `src/QuickByte.Agent/SchedulerLoop.cs`.
//!
//! Starting the download manager, as the loop needs to see it: is it already up,
//! and can it be launched for a queue. A trait over what would otherwise be two
//! free calls, because both are things the loop has to decide *without* doing
//! them — whether a due queue is launched, stood down on, or left for the next
//! tick is the agent's entire behaviour, and it cannot be checked by starting
//! processes.

use std::path::{Path, PathBuf};
use std::process::Command;

use uuid::Uuid;

pub trait AppLauncher: Send + Sync {
    /// Whether QuickByte is already running, and so will start the queue itself.
    fn is_running(&self) -> bool;

    /// Starts QuickByte on the given queue, reporting why if it could not.
    fn try_launch_for_queue(&self, queue_id: Uuid) -> Result<(), String>;
}

/// Finding and starting the download manager itself. Both halves deliberately
/// avoid the registry: the agent is installed beside QuickByte.exe and can
/// simply look next to itself, and a scheduler that reads a path out of the Run
/// key would start whatever that key happened to point at.
pub struct QuickByteApp {
    folder: PathBuf,
}

pub const EXECUTABLE_NAME: &str = "QuickByte.exe";

/// The mutex QuickByte's `SingleInstance` holds for as long as it runs. Opening
/// it is the cheapest possible "is the app up?", and it is the same name a
/// second launch would collide with.
pub const APP_MUTEX_NAME: &str = r"Local\QuickByte.SingleInstance";

impl QuickByteApp {
    /// QuickByte in `folder` (the agent's own folder in production).
    pub fn new(folder: PathBuf) -> Self {
        Self { folder }
    }

    /// The folder this executable runs from (`AppContext.BaseDirectory`).
    pub fn beside_this_executable() -> Self {
        let folder = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
        Self::new(folder)
    }

    #[cfg(test)]
    pub fn folder(&self) -> &Path {
        &self.folder
    }
}

impl AppLauncher for QuickByteApp {
    fn is_running(&self) -> bool {
        is_mutex_present(&format!("{APP_MUTEX_NAME}{}", quickbyte_core::helpers::app_paths::instance_suffix()))
    }

    fn try_launch_for_queue(&self, queue_id: Uuid) -> Result<(), String> {
        let executable = self.folder.join(EXECUTABLE_NAME);
        if !executable.is_file() {
            return Err(format!("{} does not exist", executable.display()));
        }
        // The child handle is dropped, not waited on: QuickByte outlives this tick.
        build_command(&executable, &self.folder, queue_id).spawn().map(|_| ()).map_err(|e| e.to_string())
    }
}

/// The command line QuickByte is started with. `--minimized` is not optional
/// here: this launch was asked for by a clock, not by a person, and a window
/// appearing over whatever the user is doing at 03:00 — or at sign-in — is not
/// what a schedule promised.
///
/// The switch and the id's format are what the app's `find_queue_id` reads on
/// the other side, so this shape is a contract between two processes rather
/// than a detail of this one — which is why it is built where it can be
/// inspected without a launch. A plain process start (no shell), with the
/// arguments kept apart so a path with a space in it is quoted correctly.
pub fn build_command(executable: &Path, working_directory: &Path, queue_id: Uuid) -> Command {
    let mut command = Command::new(executable);
    command.arg("--run-queue").arg(queue_id.hyphenated().to_string()).arg("--minimized").current_dir(working_directory);
    command
}

/// Whether a named mutex exists at this moment. Named apart from `is_running`
/// so the probe can be exercised against a mutex a caller owns, rather than
/// against whatever happens to be running on the machine.
///
/// Anything but "it is there" — absent, an access denial from a stricter
/// session, a malformed name — reads as absent: a launch that turns out to be
/// redundant hands itself over and exits, which is exactly what a second launch
/// is for.
#[cfg(windows)]
pub fn is_mutex_present(name: &str) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
    use windows::core::PCWSTR;

    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: the name buffer outlives the call; the handle is closed at once.
    unsafe {
        match OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(wide.as_ptr())) {
            Ok(handle) => {
                let _ = CloseHandle(handle);
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(not(windows))]
pub fn is_mutex_present(_name: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    //! Port of `tests/QuickByte.Agent.Tests/QuickByteAppTests.cs`. Nothing here
    //! starts anything: the command line is inspected where it is built.

    use super::*;
    use crate::single_instance::NamedMutex;

    #[test]
    fn a_named_mutex_is_present_only_while_something_holds_it() {
        // A name of this test's own, so the answer does not depend on whether
        // QuickByte happens to be running on the machine running the tests.
        let name = format!(r"Local\QuickByte.Agent.Tests.{}", Uuid::new_v4().simple());

        assert!(!is_mutex_present(&name));
        {
            let mutex = NamedMutex::create(&name).expect("created");
            assert!(mutex.created_new());
            assert!(is_mutex_present(&name));
        }
        assert!(!is_mutex_present(&name));
    }

    #[test]
    fn a_malformed_mutex_name_reads_as_not_running_rather_than_throwing() {
        assert!(!is_mutex_present(&"x".repeat(1024)));
        assert!(!is_mutex_present(r"Local\a\b"));
    }

    #[test]
    fn the_app_is_looked_for_under_the_name_it_holds_itself() {
        assert_eq!(APP_MUTEX_NAME, r"Local\QuickByte.SingleInstance");
    }

    #[test]
    fn quickbyte_is_looked_for_beside_the_agent_and_its_absence_reported() {
        let folder = std::env::temp_dir().join("QuickByte.Agent.Tests").join(Uuid::new_v4().simple().to_string());
        std::fs::create_dir_all(&folder).unwrap();
        let app = QuickByteApp::new(folder.clone());

        let error = app.try_launch_for_queue(Uuid::new_v4()).unwrap_err();

        assert!(error.contains(EXECUTABLE_NAME), "{error}");
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn the_agents_own_folder_is_where_it_looks() {
        let app = QuickByteApp::beside_this_executable();
        assert_eq!(app.folder(), std::env::current_exe().unwrap().parent().unwrap());
    }

    #[test]
    fn the_launch_carries_the_queue_and_the_switches_quickbyte_reads() {
        let queue_id = Uuid::new_v4();

        let command = build_command(Path::new(r"C:\Program Files\QuickByte\QuickByte.exe"), Path::new(r"C:\Program Files\QuickByte"), queue_id);

        let args: Vec<String> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, vec!["--run-queue".to_string(), queue_id.hyphenated().to_string(), "--minimized".to_string()]);
        assert_eq!(Uuid::parse_str(&args[1]).unwrap(), queue_id);
    }

    #[test]
    fn the_launch_is_a_plain_process_start_with_its_arguments_kept_apart() {
        let command = build_command(Path::new(r"C:\Program Files\QuickByte\QuickByte.exe"), Path::new(r"C:\Program Files\QuickByte"), Uuid::new_v4());

        assert_eq!(command.get_program(), r"C:\Program Files\QuickByte\QuickByte.exe");
        assert_eq!(command.get_current_dir(), Some(Path::new(r"C:\Program Files\QuickByte")));
        assert_eq!(command.get_args().count(), 3);
    }
}
