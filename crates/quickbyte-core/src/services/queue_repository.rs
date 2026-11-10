//! Port of `src/QuickByte.Core/Services/QueueRepository.cs`.
//!
//! Stores the queue list as JSON under `%APPDATA%\QuickByte\queues.json`.
//!
//! Two things separate it from the download repository, and both come from the
//! same fact: a second process (the scheduler agent) reads this file while the
//! app writes it.
//!
//! Writes go to a temporary file that is then moved over the real one
//! ([`atomic_file`]), so a reader either sees the whole previous file or the
//! whole new one — never the empty window a truncating write leaves behind,
//! which the agent would read as "this user has no scheduled queues" and act on.
//!
//! Reads open the file shared for reading and writing (Rust's default on
//! Windows) and retry briefly, because a share violation against a writer that
//! holds the file for a millisecond is not a reason to report no queues.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use parking_lot::Mutex;

use crate::helpers::{app_paths, atomic_file};
use crate::models::DownloadQueue;
use crate::serde_compat::strip_bom;

const READ_ATTEMPTS: u32 = 3;
const READ_RETRY_DELAY: Duration = Duration::from_millis(60);

/// The file name inside the data folder.
pub const QUEUES_FILE_NAME: &str = "queues.json";

pub struct QueueRepository {
    queues_file_path: PathBuf,
    file_lock: Mutex<()>,
}

impl Default for QueueRepository {
    fn default() -> Self {
        Self::new(None)
    }
}

impl QueueRepository {
    /// `app_data_folder` defaults to `%APPDATA%\QuickByte`; tests pass a scratch folder.
    pub fn new(app_data_folder: Option<&Path>) -> Self {
        let folder = app_data_folder.unwrap_or_else(|| app_paths::data());
        // Best-effort, as in C#: a folder that cannot be created surfaces on the first save.
        let _ = fs::create_dir_all(folder);
        Self { queues_file_path: folder.join(QUEUES_FILE_NAME), file_lock: Mutex::new(()) }
    }

    /// The full path of `queues.json`.
    pub fn path(&self) -> &Path {
        &self.queues_file_path
    }

    fn read_once(&self) -> Result<Vec<DownloadQueue>, ReadFailure> {
        let bytes = match fs::read(&self.queues_file_path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(ReadFailure::Busy),
        };
        // `null` reads as an empty list, as `Deserialize(...) ?? new List()` did.
        match serde_json::from_slice::<Option<Vec<DownloadQueue>>>(strip_bom(&bytes)) {
            Ok(queues) => Ok(queues.unwrap_or_default()),
            Err(_) => Err(ReadFailure::Malformed),
        }
    }
}

enum ReadFailure {
    /// The file is there but could not be opened or read: another process holds it.
    Busy,
    /// The file was read but is not a queue list.
    Malformed,
}

impl crate::traits::QueueRepository for QueueRepository {
    fn load_all(&self) -> Vec<DownloadQueue> {
        self.try_load_all().unwrap_or_default()
    }

    fn try_load_all(&self) -> Option<Vec<DownloadQueue>> {
        let _guard = self.file_lock.lock();
        for attempt in 0..READ_ATTEMPTS {
            match self.read_once() {
                Ok(queues) => return Some(queues),
                // Malformed rather than busy. Retrying will not help, and a
                // corrupt file must not stop the app from starting — the same
                // call the download list makes.
                Err(ReadFailure::Malformed) => return Some(Vec::new()),
                Err(ReadFailure::Busy) => {
                    if attempt + 1 < READ_ATTEMPTS {
                        std::thread::sleep(READ_RETRY_DELAY);
                    }
                }
            }
        }
        // Unreadable is not the same as empty: "try again", never "no queues".
        None
    }

    fn save_all(&self, queues: &[DownloadQueue]) {
        let _guard = self.file_lock.lock();
        // Write-then-rename: this is the step that has to be atomic, so a reader
        // is never looking at a half-written queue list. Best-effort, as the
        // trait has no error channel; the next change saves again.
        let _ = atomic_file::write_json(&self.queues_file_path, &queues);
    }
}

#[cfg(test)]
mod tests {
    //! The queue half of `tests/QuickByte.Core.Tests/Services/PersistenceTests.cs`.

    use super::*;
    use crate::enums::{PowerAction, ScheduleDays};
    use crate::models::QueueSchedule;
    use crate::serde_compat::TimeSpan;
    use crate::traits::QueueRepository as _;
    use chrono::NaiveDate;
    use uuid::Uuid;

    struct TempFolder(PathBuf);

    impl TempFolder {
        fn new() -> Self {
            let path = std::env::temp_dir().join("QuickByte.Tests").join(Uuid::new_v4().simple().to_string());
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempFolder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_empty_profile_loads_an_empty_list() {
        let folder = TempFolder::new();

        assert!(QueueRepository::new(Some(&folder.0)).load_all().is_empty());
        let queues = QueueRepository::new(Some(&folder.0)).try_load_all();
        assert_eq!(queues, Some(Vec::new()));
    }

    #[test]
    fn a_saved_queue_comes_back_whole() {
        let folder = TempFolder::new();
        let repository = QueueRepository::new(Some(&folder.0));
        let mut queue = DownloadQueue {
            name: "Nightly".into(),
            concurrent_downloads: 4,
            speed_limit_bytes_per_second: 500_000,
            when_finished: PowerAction::Hibernate,
            schedule: QueueSchedule {
                enabled: true,
                days: ScheduleDays::WEEKDAYS,
                start_time: TimeSpan::from_hours(2),
                stop_at_enabled: true,
                stop_time: TimeSpan::from_hours(6),
                ..QueueSchedule::default()
            },
            ..DownloadQueue::default()
        };
        queue.item_ids.push(Uuid::new_v4());

        repository.save_all(std::slice::from_ref(&queue));
        let restored = repository.load_all().into_iter().next().unwrap();

        assert_eq!(restored.name, "Nightly");
        assert_eq!(restored.concurrent_downloads, 4);
        assert_eq!(restored.speed_limit_bytes_per_second, 500_000);
        assert_eq!(restored.when_finished, PowerAction::Hibernate);
        assert_eq!(restored.schedule.days, ScheduleDays::WEEKDAYS);
        assert_eq!(restored.schedule.start_time, TimeSpan::from_hours(2));
        assert!(restored.schedule.stop_at_enabled);
        assert_eq!(restored.item_ids.len(), 1);
        assert_eq!(restored, queue);
    }

    #[test]
    fn a_malformed_file_is_not_a_reason_to_retry_or_to_fail() {
        // Malformed rather than busy: retrying will not help, and a corrupt file
        // must not stop the app from starting.
        let folder = TempFolder::new();
        fs::write(folder.file("queues.json"), "[[[ nope").unwrap();

        assert_eq!(QueueRepository::new(Some(&folder.0)).try_load_all(), Some(Vec::new()));
    }

    #[test]
    fn a_file_that_cannot_be_read_is_try_again_not_no_queues() {
        // A folder where the file should be: opening it fails with an I/O error,
        // which is what a busy file looks like, and must not read as "no queues".
        let folder = TempFolder::new();
        fs::create_dir_all(folder.file("queues.json")).unwrap();

        let repository = QueueRepository::new(Some(&folder.0));
        assert_eq!(repository.try_load_all(), None);
        assert!(repository.load_all().is_empty());
    }

    #[test]
    fn a_bom_and_a_null_list_are_both_read() {
        let folder = TempFolder::new();
        fs::write(folder.file("queues.json"), b"\xEF\xBB\xBFnull").unwrap();

        assert_eq!(QueueRepository::new(Some(&folder.0)).try_load_all(), Some(Vec::new()));
    }

    #[test]
    fn a_save_leaves_no_temporary_file_behind() {
        // Writes go via a temp file that is moved over the real one, so a reader
        // never sees the empty window a truncating write leaves behind — which
        // the agent would read as "this user has no scheduled queues".
        let folder = TempFolder::new();
        let repository = QueueRepository::new(Some(&folder.0));

        repository.save_all(&[DownloadQueue::default()]);

        assert!(folder.file("queues.json").exists());
        assert!(!folder.file("queues.json.tmp").exists());
    }

    #[test]
    fn a_reader_can_open_the_file_while_a_writer_holds_it() {
        let folder = TempFolder::new();
        let repository = QueueRepository::new(Some(&folder.0));
        repository.save_all(&[DownloadQueue { name: "Nightly".into(), ..DownloadQueue::default() }]);

        // The agent is a second process reading this file while the app writes it.
        let _held_open = fs::File::open(folder.file("queues.json")).unwrap();

        let queues = repository.try_load_all().unwrap();
        assert_eq!(queues.len(), 1);
        assert_eq!(queues[0].name, "Nightly");
    }

    #[test]
    fn the_same_file_read_twice_gives_the_same_verdict() {
        // The app and the agent have to agree about whether a run is owed.
        let folder = TempFolder::new();
        let when = NaiveDate::from_ymd_opt(2026, 8, 26).unwrap().and_hms_opt(2, 30, 0).unwrap();
        let queue = DownloadQueue {
            schedule: QueueSchedule {
                enabled: true,
                days: ScheduleDays::EVERY_DAY,
                start_time: TimeSpan::from_hours(2),
                ..QueueSchedule::default()
            },
            ..DownloadQueue::default()
        };

        QueueRepository::new(Some(&folder.0)).save_all(&[queue]);

        let first = QueueRepository::new(Some(&folder.0)).load_all().remove(0).is_due(when);
        let second = QueueRepository::new(Some(&folder.0)).load_all().remove(0).is_due(when);
        assert_eq!(first, second);
        assert!(first);
    }

    #[test]
    fn saving_writes_the_pascal_case_file_the_csharp_build_reads() {
        let folder = TempFolder::new();
        QueueRepository::new(Some(&folder.0)).save_all(&[DownloadQueue { name: "Nightly".into(), ..DownloadQueue::default() }]);

        let text = fs::read_to_string(folder.file("queues.json")).unwrap();
        assert!(text.starts_with("[\n  {\n    \"Id\": "), "{text}");
        assert!(text.contains("\"Name\": \"Nightly\""));
        assert!(text.contains("\"StartTime\": \"02:00:00\""));
    }
}
