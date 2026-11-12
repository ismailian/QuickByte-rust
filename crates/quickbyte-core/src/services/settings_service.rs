//! Port of `SettingsService.cs`: loads and saves [`DownloadSettings`] as JSON
//! under `%APPDATA%\QuickByte\settings.json` and announces every change, so every
//! open window immediately reflects new defaults.
//!
//! It is also the one place that decides where the chunk folder is.
//! `DownloadSettings::temp_folder` is not persisted; it is stamped onto every
//! settings object that passes through here as `temp\` beside settings.json
//! itself. See `helpers::app_paths` for why it moved there, and note that
//! stamping on `save` as well as `load` is what keeps it right: the Options
//! window builds a *fresh* `DownloadSettings` and has no field for it.
//!
//! `default_download_folder` is left alone — that one is a real setting, and
//! where a user keeps their files is their business.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::events::{EventHub, EventStream};
use crate::helpers::settings_keys::{self, SettingsKeyError};
use crate::helpers::{app_paths, atomic_file};
use crate::models::DownloadSettings;
use crate::traits;

pub const FILE_NAME: &str = "settings.json";

pub struct SettingsService {
    data_folder: PathBuf,
    settings_file: PathBuf,
    current: RwLock<Arc<DownloadSettings>>,
    /// Read, change and write happen under this: two read-modify-writes at once —
    /// the browser bridge minting its token while a window saves a preference —
    /// would otherwise each start from the same copy, and the second write would
    /// undo the first.
    write_lock: Mutex<()>,
    hub: EventHub<Arc<DownloadSettings>>,
}

impl SettingsService {
    /// A service over `app_data_folder`, or `%APPDATA%\QuickByte` when `None`.
    /// Tests hand it a scratch directory, and the chunk folder follows it there.
    pub fn new(app_data_folder: Option<&Path>) -> Self {
        let data_folder = app_data_folder.map(Path::to_path_buf).unwrap_or_else(|| app_paths::data().to_path_buf());
        let _ = std::fs::create_dir_all(&data_folder);
        let settings_file = data_folder.join(FILE_NAME);
        let service = Self {
            data_folder,
            settings_file,
            current: RwLock::new(Arc::new(DownloadSettings::default())),
            write_lock: Mutex::new(()),
            hub: EventHub::default(),
        };
        let mut initial = DownloadSettings::default();
        service.apply_chunk_folder(&mut initial);
        *service.current.write() = Arc::new(initial);
        service
    }

    /// The hub `subscribe` draws from, for attaching a [`crate::events::Dispatcher`].
    pub fn events(&self) -> &EventHub<Arc<DownloadSettings>> {
        &self.hub
    }

    pub fn data_folder(&self) -> &Path {
        &self.data_folder
    }

    pub fn settings_file(&self) -> &Path {
        &self.settings_file
    }

    /// Like `save`, but says whether the file could be written. The settings are
    /// current and announced either way: a disk that refuses the write should not
    /// also make the app ignore what the user just chose.
    pub fn try_save(&self, settings: DownloadSettings) -> std::io::Result<()> {
        let (saved, written) = {
            let _guard = self.write_lock.lock();
            self.commit(settings)
        };
        // Announced outside the lock: a subscriber that waited on another thread
        // which was itself saving would otherwise deadlock.
        self.hub.publish(saved);
        written
    }

    fn commit(&self, mut settings: DownloadSettings) -> (Arc<DownloadSettings>, std::io::Result<()>) {
        self.apply_chunk_folder(&mut settings);
        let settings = Arc::new(settings);
        *self.current.write() = settings.clone();
        let written = atomic_file::write_json(&self.settings_file, &*settings);
        self.ensure_folders_exist(&settings);
        (settings, written)
    }

    /// Points the chunk folder at this service's data folder. Tests hand the
    /// constructor a scratch directory precisely so the chunks follow it there
    /// instead of the real profile.
    fn apply_chunk_folder(&self, settings: &mut DownloadSettings) {
        settings.temp_folder = app_paths::temp_in(&self.data_folder).to_string_lossy().into_owned();
        // An older build persisted the chunk folder; C# ignored the key on read and
        // dropped it on the next write, and so does this rather than carry it along.
        settings.extra.remove("TempFolder");
    }

    /// Best-effort: a folder that cannot be created is a failed download later,
    /// with a message naming the path — not a reason to refuse to start or to
    /// lose the settings the user just saved.
    fn ensure_folders_exist(&self, settings: &DownloadSettings) {
        if !settings.default_download_folder.trim().is_empty() && self.may_create(&settings.default_download_folder) {
            let _ = std::fs::create_dir_all(&settings.default_download_folder);
        }
        let _ = std::fs::create_dir_all(&settings.temp_folder);
    }

    /// A test never creates folders outside its own scratch directory — the
    /// defaults name the real `Downloads\QuickByte`.
    #[cfg(test)]
    fn may_create(&self, folder: &str) -> bool {
        Path::new(folder).starts_with(&self.data_folder)
    }

    #[cfg(not(test))]
    fn may_create(&self, _folder: &str) -> bool {
        true
    }
}

impl traits::SettingsService for SettingsService {
    fn current(&self) -> Arc<DownloadSettings> {
        self.current.read().clone()
    }

    fn subscribe(&self) -> EventStream<Arc<DownloadSettings>> {
        self.hub.subscribe()
    }

    fn subscribe_fn(&self, callback: crate::events::EventCallback<Arc<DownloadSettings>>) {
        self.hub.subscribe_fn(callback);
    }

    fn load(&self) {
        let mut loaded = if self.settings_file.exists() {
            // Corrupt or unreadable settings — fall back to defaults rather than crash startup.
            atomic_file::read_json::<DownloadSettings>(&self.settings_file).unwrap_or_default()
        } else {
            (*self.current()).clone()
        };
        self.apply_chunk_folder(&mut loaded);
        self.ensure_folders_exist(&loaded);
        *self.current.write() = Arc::new(loaded);
    }

    /// Replaces every setting with `settings`. Start from `create_copy`, never
    /// from a `DownloadSettings::default()`, or every field left unset goes back
    /// to its default. A write that fails leaves the settings current for this
    /// session (see [`SettingsService::try_save`]).
    fn save(&self, settings: DownloadSettings) {
        let _ = self.try_save(settings);
    }

    /// Read, change and write under one lock (see `write_lock`).
    fn save_many(&self, changes: &[(&str, &str)]) -> Result<(), SettingsKeyError> {
        let saved = {
            let _guard = self.write_lock.lock();
            let updated = settings_keys::with(&self.current(), changes)?;
            self.commit(updated).0
        };
        self.hub.publish(saved);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! The loader promises a corrupt or unreadable file falls back to defaults
    //! rather than failing startup — the difference between an app that forgets
    //! a setting and one that will not open.

    use std::collections::BTreeMap;

    use super::*;
    use crate::helpers::disk_space::scratch::TempFolder;
    use crate::helpers::file_category::FileCategory;
    use crate::traits::SettingsService as _;

    fn service(folder: &TempFolder) -> SettingsService {
        let service = SettingsService::new(Some(folder.path()));
        service.load();
        service
    }

    /// A service over a scratch profile whose download folder is the scratch folder too.
    fn seeded(folder: &TempFolder) -> SettingsService {
        let service = service(folder);
        service.save(DownloadSettings {
            default_download_folder: folder.path_str(),
            stream_buffer_size_bytes: 4321,
            ..Default::default()
        });
        service
    }

    #[test]
    fn an_empty_profile_loads_the_defaults() {
        let folder = TempFolder::new();
        let service = service(&folder);
        assert_eq!(service.current().default_connections_count, 8);
        assert_eq!(service.current().max_concurrent_downloads, 3);
    }

    #[test]
    fn category_folders_survive_a_restart() {
        let folder = TempFolder::new();
        service(&folder).save(DownloadSettings {
            default_download_folder: folder.path_str(),
            sort_into_category_folders: true,
            category_folders: BTreeMap::from([(FileCategory::Video, r"D:\Media\Videos".to_string())]),
            ..Default::default()
        });

        let reopened = service(&folder);
        assert!(reopened.current().sort_into_category_folders);
        assert_eq!(reopened.current().category_folders[&FileCategory::Video], r"D:\Media\Videos");
        // Stored by name, so reordering the enum can never move a folder.
        assert!(std::fs::read_to_string(folder.path().join(FILE_NAME)).unwrap().contains("\"Video\""));
    }

    #[test]
    fn settings_round_trip() {
        let folder = TempFolder::new();
        service(&folder).save(DownloadSettings {
            default_connections_count: 16,
            global_speed_limit_bytes_per_second: 250_000,
            start_minimized: true,
            default_download_folder: folder.path_str(),
            ..Default::default()
        });

        let reopened = service(&folder);
        let current = reopened.current();
        assert_eq!(current.default_connections_count, 16);
        assert_eq!(current.global_speed_limit_bytes_per_second, 250_000);
        assert!(current.start_minimized);
        assert_eq!(current.default_download_folder, folder.path_str());
    }

    #[test]
    fn a_corrupt_settings_file_falls_back_to_defaults_rather_than_crashing_startup() {
        let folder = TempFolder::new();
        folder.write_file(FILE_NAME, b"not json");
        assert_eq!(service(&folder).current().default_connections_count, 8);
    }

    #[test]
    fn a_settings_file_with_a_byte_order_mark_still_loads() {
        let folder = TempFolder::new();
        folder.write_file(FILE_NAME, b"\xEF\xBB\xBF{\"MaxRetries\":7}");
        assert_eq!(service(&folder).current().max_retries, 7);
    }

    #[test]
    fn saving_announces_the_new_settings() {
        // The two live subscribers are the global speed limit and the browser
        // bridge; a limit you have to restart the app to apply is not a limit.
        let folder = TempFolder::new();
        let service = service(&folder);
        let events = service.subscribe();

        service.save(DownloadSettings {
            global_speed_limit_bytes_per_second: 999,
            default_download_folder: folder.path_str(),
            ..Default::default()
        });

        let announced = events.try_recv().unwrap();
        assert_eq!(announced.global_speed_limit_bytes_per_second, 999);
        assert!(Arc::ptr_eq(&announced, &service.current()));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn load_creates_the_folders_a_download_is_about_to_need() {
        let folder = TempFolder::new();
        let downloads = folder.path().join("finished");
        let service = SettingsService::new(Some(folder.path()));
        service.save(DownloadSettings { default_download_folder: downloads.to_string_lossy().into_owned(), ..Default::default() });
        std::fs::remove_dir_all(&downloads).unwrap();
        service.load();

        assert!(downloads.is_dir());
        assert!(folder.path().join("temp").is_dir());
    }

    #[test]
    fn the_chunk_folder_follows_the_data_folder_and_the_download_folder_does_not() {
        // The chunk folder is derived, not configured: Options has no field for
        // it, and a settings.json written by an older build still names %TEMP%,
        // where every disk cleaner on the machine sweeps it. Stamping it here is
        // what carries an existing install over. The download folder in the same
        // file is a real setting and has to come back untouched.
        let folder = TempFolder::new();
        folder.write_file(FILE_NAME, br#"{"DefaultDownloadFolder":"D:\\keep-me","TempFolder":"C:\\Windows\\Temp\\QuickByte"}"#);

        let service = service(&folder);
        assert_eq!(service.current().temp_folder, folder.file("temp"));
        assert_eq!(service.current().default_download_folder, r"D:\keep-me");

        // And the stale key is not carried into the next write.
        service.save(service.create_copy());
        assert!(!std::fs::read_to_string(folder.path().join(FILE_NAME)).unwrap().contains("TempFolder"));
    }

    #[test]
    fn save_stamps_the_chunk_folder_onto_the_fresh_settings_the_options_dialog_builds() {
        // The Options window builds a *new* DownloadSettings on every save and
        // does not carry the chunk folder forward — so the service has to.
        let folder = TempFolder::new();
        let service = service(&folder);
        service.save(DownloadSettings { default_connections_count: 16, ..Default::default() });
        assert_eq!(service.current().temp_folder, folder.file("temp"));
    }

    #[test]
    fn unknown_settings_from_a_newer_build_survive_a_save() {
        let folder = TempFolder::new();
        folder.write_file(FILE_NAME, br#"{"MaxRetries":2,"SomeFutureSetting":"abc","AnotherOne":true}"#);
        let service = service(&folder);
        service.save_value("MaxRetries", "5").unwrap();
        let text = std::fs::read_to_string(folder.path().join(FILE_NAME)).unwrap();
        assert!(text.contains("\"SomeFutureSetting\": \"abc\""), "{text}");
        assert!(text.contains("\"MaxRetries\": 5"), "{text}");
    }

    #[cfg(windows)]
    #[test]
    fn the_csharp_builds_virus_total_settings_carry_over_and_the_key_is_then_stored_encrypted() {
        // 1.26.2 wrote these names. Whether it stored the key encrypted or not,
        // a key that reads back as a VirusTotal key is kept — and from the next
        // save on it is never in the file in the clear.
        let key = "0123456789abcdef".repeat(4);
        let folder = TempFolder::new();
        folder.write_file(FILE_NAME, format!(r#"{{"VirusTotalEnabled":true,"ProtectedVirusTotalApiKey":"{key}"}}"#).as_bytes());

        let loaded = service(&folder);
        assert!(loaded.current().virus_total_enabled);
        assert_eq!(loaded.current().virus_total_api_key, key);

        loaded.save(loaded.create_copy());
        let text = std::fs::read_to_string(folder.path().join(FILE_NAME)).unwrap();
        assert!(!text.contains(&key), "{text}");
        assert_eq!(service(&folder).current().virus_total_api_key, key);
    }

    // ------------------------------------------------- by name (SettingsKeysTests) --

    #[test]
    fn saving_one_setting_by_name_persists_it_and_keeps_the_rest() {
        let folder = TempFolder::new();
        seeded(&folder).save_value("MaxConcurrentDownloads", "5").unwrap();

        let reopened = service(&folder);
        assert_eq!(reopened.get("MaxConcurrentDownloads").unwrap(), "5");
        assert_eq!(reopened.current().stream_buffer_size_bytes, 4321);
        assert_eq!(reopened.current().default_download_folder, folder.path_str());
    }

    #[test]
    fn saving_many_is_one_write_and_one_announcement() {
        let folder = TempFolder::new();
        let service = seeded(&folder);
        let events = service.subscribe();

        service.save_many(&[("MaxRetries", "1"), ("RetryDelayMilliseconds", "250")]).unwrap();

        let only = events.try_recv().unwrap();
        assert!(events.try_recv().is_err());
        assert!(Arc::ptr_eq(&only, &service.current()));
        assert_eq!(only.max_retries, 1);
        assert_eq!(only.retry_delay_milliseconds, 250);
    }

    #[test]
    fn a_batch_with_one_bad_value_saves_nothing() {
        let folder = TempFolder::new();
        let service = seeded(&folder);
        let before = service.current();
        let events = service.subscribe();

        let result = service.save_many(&[("MaxRetries", "1"), ("DynamicSegmentation", "maybe")]);
        assert!(matches!(result, Err(SettingsKeyError::InvalidValue(_))));
        assert!(Arc::ptr_eq(&before, &service.current()));
        assert_eq!(service.current().max_retries, 3);
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn a_saved_change_never_touches_the_instance_a_running_download_holds() {
        let folder = TempFolder::new();
        let service = seeded(&folder);
        let held_by_a_download = service.current();

        service.save_value("MaxRetries", "10").unwrap();

        assert_eq!(held_by_a_download.max_retries, 3);
        assert_eq!(service.current().max_retries, 10);
    }

    #[test]
    fn the_chunk_folder_is_still_derived_after_a_save_by_name() {
        let folder = TempFolder::new();
        let service = seeded(&folder);
        service.save_value("MaxRetries", "4").unwrap();
        assert_eq!(service.current().temp_folder, app_paths::temp_in(folder.path()).to_string_lossy());
    }

    #[test]
    fn get_all_describes_the_current_settings() {
        let folder = TempFolder::new();
        let all = seeded(&folder).get_all();
        assert_eq!(all.get("StreamBufferSizeBytes"), Some("4321"));
        assert_eq!(all.get("defaultdownloadfolder"), Some(folder.path_str().as_str()));
    }

    #[test]
    fn an_unknown_key_by_name_is_refused() {
        let folder = TempFolder::new();
        let service = seeded(&folder);
        assert!(matches!(service.save_value("NoSuchSetting", "1"), Err(SettingsKeyError::UnknownKey(_))));
        assert!(matches!(service.get("TempFolder"), Err(SettingsKeyError::UnknownKey(_))));
    }
}
