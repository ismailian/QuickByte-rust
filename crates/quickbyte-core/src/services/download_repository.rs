//! Port of `DownloadRepository.cs`: persists the download list as JSON under
//! `%APPDATA%\QuickByte\downloads.json` so the In Progress / Completed / Failed
//! lists survive app restarts.
//!
//! One improvement over the C#: saves are atomic (`helpers::atomic_file`). The
//! C# wrote the file in place, and a crash mid-write left it truncated — and the
//! whole download list with it.

use std::path::{Path, PathBuf};

use parking_lot::Mutex;

use crate::helpers::{app_paths, atomic_file};
use crate::models::DownloadItem;
use crate::traits;

pub const FILE_NAME: &str = "downloads.json";

pub struct DownloadRepository {
    file: PathBuf,
    file_lock: Mutex<()>,
}

impl DownloadRepository {
    /// A repository in `app_data_folder`, or `%APPDATA%\QuickByte` when `None`.
    pub fn new(app_data_folder: Option<&Path>) -> Self {
        let folder = app_data_folder.map(Path::to_path_buf).unwrap_or_else(|| app_paths::data().to_path_buf());
        let _ = std::fs::create_dir_all(&folder);
        Self { file: folder.join(FILE_NAME), file_lock: Mutex::new(()) }
    }

    pub fn file_path(&self) -> &Path {
        &self.file
    }

    /// Like `save_all`, but says whether the file could be written.
    pub fn try_save_all(&self, items: &[DownloadItem]) -> std::io::Result<()> {
        // The computed fields are written for the C# build's benefit (a user can
        // roll back) and never trusted on read, so they are made true right here.
        let items: Vec<DownloadItem> = items
            .iter()
            .cloned()
            .map(|mut item| {
                item.refresh_derived();
                item
            })
            .collect();
        let _guard = self.file_lock.lock();
        atomic_file::write_json(&self.file, &items)
    }
}

impl traits::DownloadRepository for DownloadRepository {
    /// The saved list; empty when there is none, or when it cannot be read — an
    /// app that forgets its download list is better than one that will not open.
    fn load_all(&self) -> Vec<DownloadItem> {
        let _guard = self.file_lock.lock();
        if !self.file.exists() {
            return Vec::new();
        }
        atomic_file::read_json(&self.file).unwrap_or_default()
    }

    fn save_all(&self, items: &[DownloadItem]) {
        let _ = self.try_save_all(items);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::{CompletionActions, DownloadStatus, PowerAction};
    use crate::helpers::disk_space::scratch::TempFolder;
    use crate::models::{DownloadCredentials, RemoteFileVersion};
    use crate::serde_compat::DateTimeOffset;
    use crate::traits::DownloadRepository as _;

    fn repository(folder: &TempFolder) -> DownloadRepository {
        DownloadRepository::new(Some(folder.path()))
    }

    #[test]
    fn an_empty_profile_loads_an_empty_list() {
        let folder = TempFolder::new();
        assert!(repository(&folder).load_all().is_empty());
    }

    #[test]
    fn a_saved_list_comes_back() {
        let folder = TempFolder::new();
        let repository = repository(&folder);
        let item = DownloadItem {
            url: "https://example.com/a.bin".into(),
            file_name: "a.bin".into(),
            save_folder: r"C:\Downloads".into(),
            total_bytes: 1234,
            downloaded_bytes: 567,
            status: DownloadStatus::Paused,
            ..Default::default()
        };

        repository.save_all(std::slice::from_ref(&item));
        let loaded = repository.load_all();

        assert_eq!(loaded.len(), 1);
        let restored = &loaded[0];
        assert_eq!(restored.id, item.id);
        assert_eq!(restored.file_name, "a.bin");
        assert_eq!(restored.total_bytes, 1234);
        assert_eq!(restored.status, DownloadStatus::Paused);
    }

    #[test]
    fn the_computed_fields_are_written_fresh_for_a_rollback() {
        let folder = TempFolder::new();
        let repository = repository(&folder);
        let item = DownloadItem {
            file_name: "a.bin".into(),
            save_folder: r"C:\Downloads".into(),
            total_bytes: 200,
            downloaded_bytes: 50,
            status: DownloadStatus::Paused,
            ..Default::default()
        };
        repository.save_all(&[item]);

        let text = std::fs::read_to_string(folder.path().join(FILE_NAME)).unwrap();
        assert!(text.contains(r#""FullPath": "C:\\Downloads\\a.bin""#), "{text}");
        assert!(text.contains(r#""ProgressPercentage": 25"#), "{text}");
        assert!(text.contains(r#""Category": 5"#), "{text}");
        assert!(!text.starts_with('\u{FEFF}'));
    }

    #[test]
    fn the_captured_headers_survive_a_restart() {
        // A session cookie that resolved the link at capture time is what lets a
        // resume three days later fetch the same bytes rather than a login page.
        let folder = TempFolder::new();
        let repository = repository(&folder);
        let mut item = DownloadItem::default();
        item.headers.insert("Cookie".into(), "session=abc".into());
        repository.save_all(&[item]);
        assert_eq!(repository.load_all()[0].headers["Cookie"], "session=abc");
    }

    #[test]
    fn the_version_the_chunks_belong_to_survives_a_restart() {
        // The resume it protects is usually the one after a restart: that is when
        // the probe compares the server's file with the one on disk.
        let folder = TempFolder::new();
        let repository = repository(&folder);
        let modified: DateTimeOffset = "2026-09-01T08:30:00+00:00".parse().unwrap();
        let version = RemoteFileVersion::new(Some("W/\"v1\"".into()), Some(modified), Some("example.com".into()));
        repository.save_all(&[DownloadItem { remote_version: Some(version.clone()), if_range_unsupported: true, ..Default::default() }]);

        let restored = repository.load_all().remove(0);
        assert_eq!(restored.remote_version, Some(version));
        assert!(restored.if_range_unsupported);
    }

    #[cfg(windows)]
    #[test]
    fn a_password_is_never_written_to_the_file_in_the_clear() {
        let folder = TempFolder::new();
        let repository = repository(&folder);
        repository.save_all(&[DownloadItem { credentials: Some(DownloadCredentials::new("alice", "hunter2")), ..Default::default() }]);

        let on_disk = std::fs::read_to_string(folder.path().join(FILE_NAME)).unwrap();
        assert!(!on_disk.contains("hunter2"));
        assert_eq!(repository.load_all()[0].credentials.as_ref().unwrap().password, "hunter2");
    }

    #[test]
    fn what_a_download_does_when_it_finishes_survives_a_restart() {
        let folder = TempFolder::new();
        let repository = repository(&folder);
        repository.save_all(&[DownloadItem {
            on_completion: CompletionActions::OPEN_FOLDER,
            power_action_when_done: PowerAction::ShutDown,
            ..Default::default()
        }]);
        let restored = repository.load_all().remove(0);
        assert_eq!(restored.on_completion, CompletionActions::OPEN_FOLDER);
        assert_eq!(restored.power_action_when_done, PowerAction::ShutDown);
    }

    #[test]
    fn a_corrupt_file_loads_as_empty_rather_than_failing() {
        let folder = TempFolder::new();
        folder.write_file(FILE_NAME, b"{ this is not json");
        assert!(repository(&folder).load_all().is_empty());
    }

    #[test]
    fn saving_replaces_the_whole_list() {
        let folder = TempFolder::new();
        let repository = repository(&folder);
        repository.save_all(&[DownloadItem::default(), DownloadItem::default()]);
        repository.save_all(&[DownloadItem::default()]);
        assert_eq!(repository.load_all().len(), 1);
        assert!(!folder.path().join("downloads.json.tmp").exists());
    }

    #[test]
    fn a_download_from_a_newer_build_keeps_what_this_one_does_not_know() {
        let folder = TempFolder::new();
        folder.write_file(FILE_NAME, br#"[{"Id":"d8d7a993-496a-4bfa-85e5-2d9e20ecf2eb","FileName":"a.exe","VirusTotal":{"Verdict":"clean"}}]"#);
        let repository = repository(&folder);
        let items = repository.load_all();
        repository.save_all(&items);
        let text = std::fs::read_to_string(folder.path().join(FILE_NAME)).unwrap();
        assert!(text.contains("\"VirusTotal\""), "{text}");
        assert_eq!(repository.load_all()[0].extra, items[0].extra);
    }
}
