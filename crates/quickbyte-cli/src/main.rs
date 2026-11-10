//! `qb.exe` — port of `src/QuickByte.Cli/Program.cs`.
//!
//! QuickByte from a prompt or a script. It parses the command line, hands the
//! request to the running app over the command pipe, and prints what came back —
//! nothing is downloaded or decided in this process, so a download added here
//! follows exactly the rules of one added in the window.
//!
//! A command that adds work starts QuickByte (in the notification area) when it
//! is not running; one that only reads state reports that it is not, with its own
//! exit code, rather than starting an app to say there is nothing to show.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use quickbyte_core::commands::{command_line, formatter, pipe};

const DONE: u8 = 0;
const REFUSED: u8 = 1;
const USAGE: u8 = 2;
const NOT_RUNNING: u8 = 3;

const APP_EXECUTABLE: &str = "QuickByte.exe";

const FIRST_CONNECT: Duration = Duration::from_millis(400);
/// The app has to load its downloads before it listens; give it time.
const CONNECT_AFTER_START: Duration = Duration::from_secs(30);
const REPLY_TIMEOUT: Duration = Duration::from_secs(3 * 60);

fn main() -> ExitCode {
    // File names are whatever servers call them. Rust writes to a console as
    // UTF-16 and to a pipe or file as UTF-8, so a script reading the output gets
    // them intact rather than through the console's legacy code page.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let working_directory = std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    let parsed = command_line::parse(&args, &working_directory);

    if parsed.show_help {
        println!("{}", command_line::USAGE);
        return ExitCode::from(DONE);
    }
    if parsed.show_version {
        println!("qb {}", quickbyte_core::VERSION);
        return ExitCode::from(DONE);
    }
    let Some(request) = parsed.request.clone().filter(|_| parsed.error.is_none()) else {
        eprintln!("{}", parsed.error.as_deref().unwrap_or("Nothing to do."));
        eprintln!("Run 'qb help' for the commands.");
        return ExitCode::from(USAGE);
    };

    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("qb could not start: {e}");
            return ExitCode::from(REFUSED);
        }
    };

    runtime.block_on(async {
        let mut response = match pipe::send(&request, FIRST_CONNECT, REPLY_TIMEOUT, None).await {
            Ok(response) => response,
            Err(e) => {
                eprintln!("QuickByte could not be reached: {e}");
                return ExitCode::from(NOT_RUNNING);
            }
        };

        if response.is_none() && command_line::starts_quickbyte(&request) {
            if let Err(problem) = start_quickbyte() {
                eprintln!("{problem}");
                return ExitCode::from(NOT_RUNNING);
            }
            response = match pipe::send(&request, CONNECT_AFTER_START, REPLY_TIMEOUT, None).await {
                Ok(response) => response,
                Err(e) => {
                    eprintln!("QuickByte could not be reached: {e}");
                    return ExitCode::from(NOT_RUNNING);
                }
            };
        }

        let Some(response) = response else {
            eprintln!("QuickByte is not running.");
            return ExitCode::from(NOT_RUNNING);
        };

        let output = formatter::format(&request, &response, parsed.json);
        if !output.is_empty() {
            println!("{output}");
        }
        if !response.ok
            && !parsed.json
            && let Some(error) = &response.error
        {
            eprintln!("{error}");
        }
        ExitCode::from(if response.ok { DONE } else { REFUSED })
    })
}

/// The folder this executable sits in.
fn own_folder() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf))
}

/// QuickByte from beside this executable — never from PATH, which could name anything.
fn start_quickbyte() -> Result<(), String> {
    let folder = own_folder().ok_or_else(|| "QuickByte is not running, and qb cannot tell where it is installed.".to_string())?;
    start_quickbyte_in(&folder)
}

fn start_quickbyte_in(folder: &Path) -> Result<(), String> {
    let app = folder.join(APP_EXECUTABLE);
    if !app.is_file() {
        return Err(format!("QuickByte is not running, and {} is not there to start.", app.display()));
    }
    std::process::Command::new(&app)
        .arg("--minimized")
        .current_dir(folder)
        .spawn()
        .map(drop)
        .map_err(|e| format!("QuickByte is not running and could not be started: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_is_looked_for_only_beside_qb() {
        // An empty folder, never the test binary's own: cargo can leave a
        // QuickByte.exe beside it, and a test must never start a real app. The
        // answer names the folder it looked in, not anything on PATH.
        let folder = std::env::temp_dir().join(format!("qb-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let problem = start_quickbyte_in(&folder).unwrap_err();
        let _ = std::fs::remove_dir(&folder);
        assert!(problem.contains(APP_EXECUTABLE), "{problem}");
        assert!(problem.contains(&folder.display().to_string()), "{problem}");
    }
}
