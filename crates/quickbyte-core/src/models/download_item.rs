//! `DownloadItem` — one download, as persisted in `downloads.json`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{DownloadCredentials, Extra, Headers, RemoteFileVersion, RequestOptions, StreamSource, null_as_default};
use crate::enums::{CompletionActions, DownloadCategory, DownloadStatus, PowerAction};
use crate::helpers::file_checksum::ChecksumResult;
use crate::models::DownloadSettings;
use crate::serde_compat::{DateTimeOffset, TimeSpan};

/// One download. Field order and names are the C# property order and names,
/// so a file written here diffs cleanly against one the C# build wrote.
///
/// The `derived_*` fields are the C# computed properties (`FullPath`,
/// `AudioFullPath`, `ProgressPercentage`, `Category`). System.Text.Json wrote
/// them, so they are written here too — a user who rolls back finds the file
/// exactly as the C# build would have left it — but they are never trusted on
/// read: [`DownloadItem::refresh_derived`] recomputes them before every save,
/// and the accessor methods always compute fresh.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct DownloadItem {
    pub id: Uuid,
    #[serde(deserialize_with = "null_as_default")]
    pub url: String,
    #[serde(deserialize_with = "null_as_default")]
    pub file_name: String,
    #[serde(deserialize_with = "null_as_default")]
    pub save_folder: String,
    #[serde(deserialize_with = "null_as_default")]
    pub content_type: String,
    pub total_bytes: i64,
    pub downloaded_bytes: i64,
    pub connections_count: i32,
    pub supports_resume: bool,
    pub credentials: Option<DownloadCredentials>,
    #[serde(deserialize_with = "null_as_default")]
    pub headers: Headers,
    /// This download's own cap in bytes/second; 0 = none. Persisted.
    pub speed_limit_bytes_per_second: i64,
    pub on_completion: CompletionActions,
    #[serde(deserialize_with = "super::lenient_power_action")]
    pub power_action_when_done: PowerAction,
    /// The user's checksum in its canonical `algorithm:hex` form.
    pub expected_checksum: Option<String>,
    pub checksum_result: ChecksumResult,
    pub status: DownloadStatus,
    pub error_message: Option<String>,
    #[serde(serialize_with = "crate::serde_compat::dotnet_double")]
    pub current_speed_bytes_per_second: f64,
    pub estimated_time_remaining: Option<TimeSpan>,
    #[serde(serialize_with = "crate::serde_compat::dotnet_double")]
    pub merge_progress_percentage: f64,
    pub created_at: DateTimeOffset,
    pub completed_at: Option<DateTimeOffset>,
    #[serde(rename = "FullPath")]
    pub derived_full_path: String,
    pub stream: Option<StreamSource>,
    #[serde(rename = "AudioFullPath")]
    pub derived_audio_full_path: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub temp_folder_path: String,
    /// Ranges are only honoured when the cache is bypassed (see the README's
    /// "Segment support is proven" section); every request asks that way.
    pub bypass_cache: bool,
    pub remote_version: Option<RemoteFileVersion>,
    /// The server answers `If-Range` wrongly; stop sending it.
    pub if_range_unsupported: bool,
    #[serde(rename = "ProgressPercentage")]
    #[serde(serialize_with = "crate::serde_compat::dotnet_double")]
    pub derived_progress_percentage: f64,
    #[serde(rename = "Category")]
    pub derived_category: DownloadCategory,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for DownloadItem {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            url: String::new(),
            file_name: String::new(),
            save_folder: String::new(),
            content_type: String::new(),
            total_bytes: 0,
            downloaded_bytes: 0,
            connections_count: DownloadSettings::DEFAULT_CONNECTIONS,
            supports_resume: false,
            credentials: None,
            headers: Headers::new(),
            speed_limit_bytes_per_second: 0,
            on_completion: CompletionActions::NONE,
            power_action_when_done: PowerAction::None,
            expected_checksum: None,
            checksum_result: ChecksumResult::NotChecked,
            status: DownloadStatus::Queued,
            error_message: None,
            current_speed_bytes_per_second: 0.0,
            estimated_time_remaining: None,
            merge_progress_percentage: 0.0,
            created_at: DateTimeOffset::now(),
            completed_at: None,
            derived_full_path: String::new(),
            stream: None,
            derived_audio_full_path: None,
            temp_folder_path: String::new(),
            bypass_cache: false,
            remote_version: None,
            if_range_unsupported: false,
            derived_progress_percentage: 0.0,
            derived_category: DownloadCategory::Queued,
            extra: Extra::new(),
        }
    }
}

impl DownloadItem {
    /// `Path.Combine(SaveFolder, FileName)`.
    pub fn full_path(&self) -> String {
        join_path(&self.save_folder, &self.file_name)
    }

    /// The separate sound track's path, for a stream whose audio is its own file.
    pub fn audio_full_path(&self) -> Option<String> {
        self.stream
            .as_ref()
            .and_then(|s| s.audio_file_name.as_deref())
            .filter(|n| !n.is_empty())
            .map(|n| join_path(&self.save_folder, n))
    }

    pub fn progress_percentage(&self) -> f64 {
        if self.total_bytes <= 0 {
            0.0
        } else {
            (self.downloaded_bytes as f64 / self.total_bytes as f64 * 100.0).min(100.0)
        }
    }

    pub fn category(&self) -> DownloadCategory {
        match self.status {
            DownloadStatus::Queued => DownloadCategory::Queued,
            DownloadStatus::Connecting | DownloadStatus::Downloading | DownloadStatus::Merging | DownloadStatus::Verifying => {
                DownloadCategory::InProgress
            }
            DownloadStatus::Paused => DownloadCategory::Paused,
            DownloadStatus::Completed => DownloadCategory::Completed,
            DownloadStatus::Failed | DownloadStatus::Cancelled => DownloadCategory::Failed,
        }
    }

    /// Recomputes the written-but-never-read computed fields. Called by the
    /// repository right before serialization.
    pub fn refresh_derived(&mut self) {
        self.derived_full_path = self.full_path();
        self.derived_audio_full_path = self.audio_full_path();
        self.derived_progress_percentage = self.progress_percentage();
        self.derived_category = self.category();
        if let Some(v) = self.remote_version.as_mut() {
            v.refresh_derived();
        }
        if let Some(s) = self.stream.as_mut() {
            s.refresh_derived();
        }
    }

    /// The options every request for this download carries.
    pub fn to_request_options(&self) -> RequestOptions {
        RequestOptions {
            credentials: self.credentials.clone(),
            headers: (!self.headers.is_empty()).then(|| self.headers.clone()),
            bypass_cache: self.bypass_cache,
            // Only for a server that has proven it serves ranges. One that has not is
            // fetched on a single connection that expects a 200, and presenting a
            // condition to it would turn that expected answer into a false alarm.
            resume_validator: self
                .remote_version
                .as_ref()
                .filter(|v| self.supports_resume && !self.if_range_unsupported && v.has_validator())
                .cloned(),
        }
    }
}

/// `Path.Combine` for two segments, Windows separators.
pub fn join_path(folder: &str, name: &str) -> String {
    if folder.is_empty() {
        return name.to_string();
    }
    if name.is_empty() {
        return folder.to_string();
    }
    std::path::Path::new(folder).join(name).to_string_lossy().into_owned()
}
