//! Port of `src/QuickByte.Agent/AgentLog.cs`.
//!
//! A one-line-at-a-time log next to the settings the agent reads
//! (`%APPDATA%\QuickByte\agent.log`).
//!
//! A background process with no window and no console has no other way to
//! explain itself, and "my queue did not start last night" is a question that
//! can only be answered after the fact. It is capped and rewritten rather than
//! rotated: this is a diagnostic aid, not a record, and a scheduler that
//! silently fills a user's profile with logs would be a worse bug than the one
//! it is there to help find.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

const MAX_BYTES: u64 = 64 * 1024;

pub const LOG_FILE_NAME: &str = "agent.log";

/// Where the lines go. The path is a constructor argument so the tests can point
/// it at a scratch file: this log is the one place the agent's decisions are
/// observable, and a test run has no business appending to — or, past the cap,
/// deleting — the user's real agent.log.
pub struct AgentLog {
    path: PathBuf,
    sync: std::sync::Mutex<()>,
}

impl AgentLog {
    pub fn new(path: PathBuf) -> Self {
        Self { path, sync: std::sync::Mutex::new(()) }
    }

    /// `%APPDATA%\QuickByte\agent.log`.
    pub fn in_data_folder() -> Self {
        Self::new(quickbyte_core::helpers::app_paths::data().join(LOG_FILE_NAME))
    }

    #[cfg(test)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// `yyyy-MM-dd HH:mm:ss  message` and a CRLF, as the C# agent wrote it.
    /// Best-effort by definition: a log that can fail is a scheduler that can
    /// miss a run because its disk was full.
    pub fn write(&self, message: &str) {
        let _guard = self.sync.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = self.try_write(message);
    }

    fn try_write(&self, message: &str) -> std::io::Result<()> {
        if let Some(folder) = self.path.parent() {
            fs::create_dir_all(folder)?;
        }
        if fs::metadata(&self.path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
            fs::remove_file(&self.path)?;
        }
        let line = format!("{}  {message}\r\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
        OpenOptions::new().create(true).append(true).open(&self.path)?.write_all(line.as_bytes())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! Port of `tests/QuickByte.Agent.Tests/AgentLogTests.cs` (and `LogFile.cs`).

    use super::*;
    use std::path::Path;

    /// A scratch log file, read back and removed afterwards.
    pub(crate) struct LogFile {
        pub(crate) folder: PathBuf,
        pub(crate) log: std::sync::Arc<AgentLog>,
    }

    impl LogFile {
        pub(crate) fn new() -> Self {
            let folder = std::env::temp_dir().join("QuickByte.Agent.Tests").join(uuid::Uuid::new_v4().simple().to_string());
            fs::create_dir_all(&folder).unwrap();
            let log = std::sync::Arc::new(AgentLog::new(folder.join(LOG_FILE_NAME)));
            Self { folder, log }
        }

        pub(crate) fn path(&self) -> &Path {
            self.log.path()
        }

        pub(crate) fn text(&self) -> String {
            fs::read_to_string(self.path()).unwrap_or_default()
        }

        pub(crate) fn lines(&self) -> Vec<String> {
            self.text().split("\r\n").filter(|l| !l.is_empty()).map(String::from).collect()
        }

        /// How many lines mention `fragment`.
        pub(crate) fn count(&self, fragment: &str) -> usize {
            self.lines().iter().filter(|l| l.contains(fragment)).count()
        }
    }

    impl Drop for LogFile {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.folder);
        }
    }

    #[test]
    fn a_line_carries_the_time_it_happened() {
        let log = LogFile::new();
        log.log.write("scheduler started");

        let lines = log.lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with("scheduler started"));
        assert!(
            chrono::NaiveDateTime::parse_from_str(&lines[0][..19], "%Y-%m-%d %H:%M:%S").is_ok(),
            "no timestamp at the head of: {}",
            lines[0]
        );
        assert_eq!(&lines[0][19..21], "  ");
    }

    #[test]
    fn lines_accumulate_in_the_order_they_were_written() {
        let log = LogFile::new();
        log.log.write("scheduler started");
        log.log.write("started QuickByte for queue 'Nightly'");

        let lines = log.lines();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("scheduler started"));
        assert!(lines[1].ends_with("started QuickByte for queue 'Nightly'"));
        assert!(log.text().ends_with("\r\n"));
    }

    #[test]
    fn a_log_that_is_still_under_its_cap_is_appended_to() {
        let log = LogFile::new();
        fs::write(log.path(), format!("{}\r\n", "x".repeat(60 * 1024))).unwrap();

        log.log.write("last night");

        assert!(log.text().contains("xxxx"));
        assert!(log.lines().last().unwrap().ends_with("last night"));
    }

    #[test]
    fn a_log_past_its_cap_is_started_again_rather_than_rotated() {
        let log = LogFile::new();
        fs::write(log.path(), format!("{}\r\n", "x".repeat(70 * 1024))).unwrap();

        log.log.write("after the cap");

        assert!(!log.text().contains("xxxx"));
        let lines = log.lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with("after the cap"));
        assert_eq!(fs::read_dir(&log.folder).unwrap().count(), 1);
    }

    #[test]
    fn a_log_that_cannot_be_written_is_not_a_crash() {
        // Its folder is a file, so even creating the directory fails.
        let log = LogFile::new();
        let blocker = log.folder.join("blocker");
        fs::write(&blocker, "not a folder").unwrap();
        let unwritable = AgentLog::new(blocker.join(LOG_FILE_NAME));

        unwritable.write("nowhere to go");

        assert_eq!(fs::read_to_string(&blocker).unwrap(), "not a folder");
    }
}
