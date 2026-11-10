//! `DownloadSettings` — everything in Options, persisted as `settings.json`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Extra, ProxySettings, null_as_default};
use crate::enums::AppTheme;
use crate::helpers::app_paths;
use crate::helpers::file_category::{self, FileCategory};
use crate::models::join_path;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct DownloadSettings {
    pub default_connections_count: i32,
    pub dynamic_segmentation: bool,
    pub max_retries: i32,
    pub retry_delay_milliseconds: i32,
    pub custom_user_agent: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub default_download_folder: String,
    pub sort_into_category_folders: bool,
    #[serde(deserialize_with = "null_as_default")]
    pub proxy: ProxySettings,
    #[serde(deserialize_with = "null_as_default")]
    pub category_folders: BTreeMap<FileCategory, String>,
    /// Derived from the data folder; never persisted (see the README: the chunk
    /// folder is not a setting).
    #[serde(skip)]
    pub temp_folder: String,
    pub progress_update_interval_milliseconds: i32,
    pub stream_buffer_size_bytes: i32,
    pub max_concurrent_downloads: i32,
    pub global_speed_limit_bytes_per_second: i64,
    pub browser_integration_enabled: bool,
    pub browser_integration_port: i32,
    #[serde(deserialize_with = "null_as_default")]
    pub browser_integration_token: String,
    pub start_with_windows: bool,
    pub start_minimized: bool,
    pub keep_awake_while_downloading: bool,
    pub auto_open_details_window: bool,
    pub show_completion_window: bool,
    pub appearance: AppTheme,
    pub monitor_clipboard: bool,
    #[serde(deserialize_with = "null_as_default")]
    pub clipboard_file_types: String,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for DownloadSettings {
    fn default() -> Self {
        Self {
            default_connections_count: Self::DEFAULT_CONNECTIONS,
            dynamic_segmentation: true,
            max_retries: 3,
            retry_delay_milliseconds: 1500,
            custom_user_agent: None,
            default_download_folder: default_download_folder(),
            sort_into_category_folders: false,
            proxy: ProxySettings::default(),
            category_folders: BTreeMap::new(),
            temp_folder: app_paths::temp().to_string_lossy().into_owned(),
            progress_update_interval_milliseconds: 100,
            stream_buffer_size_bytes: 81_920,
            max_concurrent_downloads: 3,
            global_speed_limit_bytes_per_second: 0,
            browser_integration_enabled: true,
            browser_integration_port: Self::DEFAULT_BROWSER_INTEGRATION_PORT,
            browser_integration_token: String::new(),
            start_with_windows: false,
            start_minimized: false,
            keep_awake_while_downloading: true,
            auto_open_details_window: true,
            show_completion_window: true,
            appearance: AppTheme::System,
            monitor_clipboard: false,
            clipboard_file_types: crate::helpers::clipboard_links::DEFAULT_FILE_TYPES.to_string(),
            extra: Extra::new(),
        }
    }
}

/// `%USERPROFILE%\Downloads\QuickByte`.
pub fn default_download_folder() -> String {
    let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
    join_path(&join_path(&profile, "Downloads"), "QuickByte")
}

impl DownloadSettings {
    pub const MIN_CONNECTIONS: i32 = 1;
    pub const MAX_CONNECTIONS: i32 = 32;
    pub const DEFAULT_CONNECTIONS: i32 = 8;
    pub const CONNECTION_CHOICES: [i32; 7] = [1, 2, 4, 8, 16, 24, 32];
    pub const DEFAULT_BROWSER_INTEGRATION_PORT: i32 = 9614;
    pub const MIN_BUFFER_SIZE_BYTES: i32 = 4 * 1024;
    pub const MAX_BUFFER_SIZE_BYTES: i32 = 8 * 1024 * 1024;

    /// The folder a file of this name goes to: its category folder when
    /// sorting is on, else the default folder.
    pub fn folder_for(&self, file_name: &str) -> String {
        if !self.sort_into_category_folders {
            return self.default_download_folder.clone();
        }
        self.folder_for_category(file_category::get_category(file_name))
    }

    /// The user's chosen folder for a category, if it is a full path; else the default one.
    /// A relative path — which only a hand-edited settings.json could produce — is ignored
    /// rather than resolved against wherever the app started.
    pub fn folder_for_category(&self, category: FileCategory) -> String {
        match self.category_folders.get(&category) {
            Some(chosen) if !chosen.trim().is_empty() && is_path_fully_qualified(chosen) => chosen.clone(),
            _ => self.default_category_folder(category),
        }
    }

    pub fn default_category_folder(&self, category: FileCategory) -> String {
        if category == FileCategory::Other {
            self.default_download_folder.clone()
        } else {
            join_path(&self.default_download_folder, category.name())
        }
    }

    pub fn clamp_connections(&self, requested: i32) -> i32 {
        requested.clamp(Self::MIN_CONNECTIONS, Self::MAX_CONNECTIONS)
    }

    /// The largest offered choice not above `requested` (at least 1).
    pub fn nearest_connection_choice(requested: i32) -> i32 {
        let mut best = Self::CONNECTION_CHOICES[0];
        for choice in Self::CONNECTION_CHOICES {
            if choice > requested {
                break;
            }
            best = choice;
        }
        best
    }

    pub fn clamp_buffer_size(&self) -> usize {
        self.stream_buffer_size_bytes.clamp(Self::MIN_BUFFER_SIZE_BYTES, Self::MAX_BUFFER_SIZE_BYTES) as usize
    }

    /// The progress sampling interval, clamped to 50–2000 ms.
    pub fn progress_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.progress_update_interval_milliseconds.clamp(50, 2000) as u64)
    }
}

/// `Path.IsPathFullyQualified` on Windows: `C:\…`, `C:/…` or a UNC `\\server\share`.
pub fn is_path_fully_qualified(path: &str) -> bool {
    let b = path.as_bytes();
    if b.len() >= 2 && (b[0] == b'\\' || b[0] == b'/') && (b[1] == b'\\' || b[1] == b'/') {
        return true;
    }
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}
