//! QuickByte.Agent.exe — port of `src/QuickByte.Agent` (Program.cs).
//!
//! QuickByte's scheduler agent: a tiny, windowless process whose only job is to
//! be there when QuickByte is not.
//!
//! A queue that starts at 03:00 is only useful if something is watching the
//! clock at 03:00, and the download manager itself may well have been closed
//! hours earlier — a schedule that silently requires the app to be left running
//! is not a schedule. So the agent runs from the user's sign-in, reads the same
//! queues.json QuickByte writes, and when a queue comes due it launches
//! QuickByte and hands it the queue to start.
//!
//! It deliberately does not download anything, does not have a window, and holds
//! no state of its own: everything it knows is in queues.json, so the app
//! remains the only writer and the agent can be killed, restarted or updated
//! without anything being lost. It exits on its own the moment no queue has a
//! schedule left, so a user who never schedules anything never has a background
//! process.
//!
//! **No registry code, on purpose.** Running `QuickByte.Agent.exe` by hand, with
//! or without `--once`, evaluates schedules and exits; it never puts anything in
//! Startup. A process that registered its own autostart could not be switched
//! off by removing the registration. The Run value is the app's business
//! (`quickbyte_shell::queue_agent_registration`).
//!
//! No async runtime either: one thread, one 30-second sleep, ~2 MB resident.

// A windowless process: no console flashes at sign-in.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod agent_log;
mod quickbyte_app;
mod scheduler_loop;
mod single_instance;

use std::process::ExitCode;
use std::sync::Arc;

use quickbyte_core::services::queue_repository::QueueRepository;

use crate::agent_log::AgentLog;
use crate::quickbyte_app::QuickByteApp;
use crate::scheduler_loop::SchedulerLoop;
use crate::single_instance::{MUTEX_NAME, NamedMutex};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let Some(mutex) = NamedMutex::create(&format!("{MUTEX_NAME}{}", quickbyte_core::helpers::app_paths::instance_suffix())) else { return ExitCode::SUCCESS };
    if !mutex.created_new() {
        return ExitCode::SUCCESS;
    }

    let log = Arc::new(AgentLog::in_data_folder());
    let mut scheduler =
        SchedulerLoop::new(Arc::new(QueueRepository::new(None)), Arc::new(QuickByteApp::beside_this_executable()), log.clone(), None);

    if has_switch(&args, "--once") {
        // One evaluation and out: what a person runs by hand to find out why
        // last night's queue did not start. Everything it decides goes to the
        // same log the long-running agent writes.
        scheduler.evaluate_once();
        return ExitCode::SUCCESS;
    }

    // Sign-out ends a windowless process outright; nothing ever sends on this
    // channel, but keeping its sender alive keeps the loop's wait a plain sleep.
    let (_keep_running, cancel) = crossbeam_channel::bounded::<()>(1);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scheduler.run(&cancel)));
    drop(mutex);
    match outcome {
        Ok(_) => ExitCode::SUCCESS,
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown error".into());
            log.write(&format!("agent stopped unexpectedly: {message}"));
            ExitCode::from(1)
        }
    }
}

/// Whether a switch is present, whatever the shell did to it on the way in: the
/// Run key hands this process a quoted command line, and a hand-typed `--Once`
/// means the same thing as `--once`.
fn has_switch(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a.trim().trim_matches('"').eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    //! Port of `tests/QuickByte.Agent.Tests/CommandLineTests.cs`.

    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_once_switch_is_recognised_however_it_arrives() {
        for argument in ["--once", "--ONCE", "--Once", "\"--once\"", "  --once  "] {
            assert!(has_switch(&args(&[argument]), "--once"), "{argument}");
        }
    }

    #[test]
    fn something_that_is_not_the_switch_is_not_the_switch() {
        for argument in ["once", "-once", "--once-more", ""] {
            assert!(!has_switch(&args(&[argument]), "--once"), "{argument}");
        }
    }

    #[test]
    fn a_bare_launch_asks_for_the_service_not_a_single_pass() {
        assert!(!has_switch(&[], "--once"));
    }

    #[test]
    fn the_switch_is_found_wherever_it_sits_on_the_line() {
        assert!(has_switch(&args(&[r"C:\Program Files\QuickByte\QuickByte.Agent.exe", "--once"]), "--once"));
    }
}
