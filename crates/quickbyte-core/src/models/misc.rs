//! The smaller models: credentials, request options, remote file info and
//! version, stream source, connection snapshot, requests, and the updater's
//! manifest/result types.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use uuid::Uuid;

use super::{Extra, null_as_default};
use crate::enums::ConnectionStatus;
use crate::helpers::file_checksum::ExpectedChecksum;
use crate::helpers::secret_protector;
use crate::serde_compat::DateTimeOffset;

/// HTTP headers as persisted on a download (`Dictionary<string,string>`).
/// Lookups that must ignore case go through [`header_get`].
pub type Headers = BTreeMap<String, String>;

/// Case-insensitive header lookup.
pub fn header_get<'a>(headers: &'a Headers, name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

// ---------------------------------------------------------------------------
// DownloadCredentials
// ---------------------------------------------------------------------------

/// A login a download presents to its server. The password lives only in
/// memory; what is persisted is `ProtectedPassword`, DPAPI-encrypted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "CredentialsWire", into = "CredentialsWire")]
pub struct DownloadCredentials {
    pub user_name: String,
    /// Never serialized in the clear (`[JsonIgnore]` in C#).
    pub password: String,
    pub extra: Extra,
}

impl DownloadCredentials {
    pub fn new(user_name: impl Into<String>, password: impl Into<String>) -> Self {
        Self { user_name: user_name.into(), password: password.into(), extra: Extra::new() }
    }
    pub fn is_empty(&self) -> bool {
        self.user_name.is_empty() && self.password.is_empty()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CredentialsWire {
    #[serde(default, deserialize_with = "null_as_default")]
    user_name: String,
    #[serde(default)]
    protected_password: Option<String>,
    #[serde(flatten)]
    extra: Extra,
}

impl From<CredentialsWire> for DownloadCredentials {
    fn from(w: CredentialsWire) -> Self {
        Self {
            user_name: w.user_name,
            password: secret_protector::unprotect(w.protected_password.as_deref()),
            extra: w.extra,
        }
    }
}

impl From<DownloadCredentials> for CredentialsWire {
    fn from(c: DownloadCredentials) -> Self {
        Self { protected_password: secret_protector::protect(&c.password), user_name: c.user_name, extra: c.extra }
    }
}

// ---------------------------------------------------------------------------
// RemoteFileVersion / RemoteFileInfo
// ---------------------------------------------------------------------------

/// Which version of a file a download's chunks came from: the server's
/// validators plus the host that issued them (validators are only comparable
/// from the host that minted them).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct RemoteFileVersion {
    #[serde(rename = "ETag")]
    pub etag: Option<String>,
    pub last_modified: Option<DateTimeOffset>,
    pub host: Option<String>,
    /// Computed; written for the C# build's benefit, recomputed on save.
    #[serde(rename = "HasValidator")]
    pub derived_has_validator: bool,
    #[serde(rename = "HasStrongETag")]
    pub derived_has_strong_etag: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl RemoteFileVersion {
    pub fn new(etag: Option<String>, last_modified: Option<DateTimeOffset>, host: Option<String>) -> Self {
        let mut v = Self { etag, last_modified, host, ..Default::default() };
        v.refresh_derived();
        v
    }
    pub fn has_validator(&self) -> bool {
        self.etag.is_some() || self.last_modified.is_some()
    }
    pub fn has_strong_etag(&self) -> bool {
        self.etag.as_deref().is_some_and(|e| !e.starts_with("W/"))
    }
    pub fn refresh_derived(&mut self) {
        self.derived_has_validator = self.has_validator();
        self.derived_has_strong_etag = self.has_strong_etag();
    }
}

/// What a probe learned about a remote file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteFileInfo {
    pub file_name: String,
    pub content_length: i64,
    pub content_type: String,
    /// Proven: only a `206` from the offset that was asked for sets this.
    pub supports_range_requests: bool,
    /// What `Accept-Ranges` claimed, which is not proof.
    pub server_claims_range_support: bool,
    /// Ranges are honoured only when the request bypasses the cache.
    pub bypass_cache: bool,
    pub etag: Option<String>,
    pub last_modified: Option<DateTimeOffset>,
    pub final_url: String,
    pub requires_authentication: bool,
}

impl Default for RemoteFileInfo {
    fn default() -> Self {
        Self {
            file_name: "download".into(),
            content_length: 0,
            content_type: "application/octet-stream".into(),
            supports_range_requests: false,
            server_claims_range_support: false,
            bypass_cache: false,
            etag: None,
            last_modified: None,
            final_url: String::new(),
            requires_authentication: false,
        }
    }
}

impl RemoteFileInfo {
    pub fn has_known_size(&self) -> bool {
        self.content_length > 0
    }
    /// The version this probe describes, with the host (`authority`) of the final URL.
    pub fn version(&self) -> RemoteFileVersion {
        let host = url::Url::parse(&self.final_url).ok().and_then(|u| authority(&u));
        RemoteFileVersion::new(self.etag.clone(), self.last_modified, host)
    }
}

/// `Uri.Authority`: host plus the port when it is not the scheme's default.
pub fn authority(u: &url::Url) -> Option<String> {
    let host = u.host_str()?;
    Some(match u.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

// ---------------------------------------------------------------------------
// RequestOptions
// ---------------------------------------------------------------------------

/// Everything about a request beyond its URL: login, captured headers, cache
/// bypass, and the `If-Range` validator.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestOptions {
    pub credentials: Option<DownloadCredentials>,
    pub headers: Option<Headers>,
    pub bypass_cache: bool,
    pub resume_validator: Option<RemoteFileVersion>,
}

impl RequestOptions {
    pub fn none() -> Self {
        Self::default()
    }
    pub fn has_credentials(&self) -> bool {
        self.credentials.as_ref().is_some_and(|c| !c.is_empty())
    }
    pub fn has_headers(&self) -> bool {
        self.headers.as_ref().is_some_and(|h| !h.is_empty())
    }
    pub fn with_cache_bypass(&self) -> Self {
        Self { bypass_cache: true, ..self.clone() }
    }
}

// ---------------------------------------------------------------------------
// StreamSource
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum StreamFormat {
    #[default]
    Hls = 0,
    Dash = 1,
}

/// Which quality of a streaming video a download fetches. Persisted on the item.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct StreamSource {
    pub format: StreamFormat,
    pub main_playlist_url: Option<String>,
    pub audio_playlist_url: Option<String>,
    pub main_representation: Option<String>,
    pub audio_representation: Option<String>,
    pub audio_extension: Option<String>,
    pub audio_file_name: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub quality: String,
    #[serde(rename = "HasSeparateAudio")]
    pub derived_has_separate_audio: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl StreamSource {
    pub fn has_separate_audio(&self) -> bool {
        match self.format {
            StreamFormat::Hls => self.audio_playlist_url.is_some(),
            StreamFormat::Dash => self.audio_representation.is_some(),
        }
    }
    pub fn refresh_derived(&mut self) {
        self.derived_has_separate_audio = self.has_separate_audio();
    }
}

// ---------------------------------------------------------------------------
// ConnectionInfo
// ---------------------------------------------------------------------------

/// An immutable snapshot of one connection, for the details window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub connection_id: i32,
    pub range_start: i64,
    pub range_end: i64,
    pub bytes_downloaded: i64,
    pub status: ConnectionStatus,
    pub retry_count: i32,
    pub last_error: Option<String>,
}

impl ConnectionInfo {
    pub fn total_bytes(&self) -> i64 {
        self.range_end - self.range_start + 1
    }
    pub fn remaining_bytes(&self) -> i64 {
        (self.total_bytes() - self.bytes_downloaded).max(0)
    }
    pub fn progress_percentage(&self) -> f64 {
        let total = self.total_bytes();
        if total <= 0 { 0.0 } else { self.bytes_downloaded as f64 / total as f64 * 100.0 }
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// What the Add Download window (or `qb add`, or a batch) asks the manager to create.
#[derive(Clone, Debug)]
pub struct DownloadRequest {
    pub url: String,
    pub file_info: RemoteFileInfo,
    pub save_folder: String,
    pub file_name: String,
    pub connections_count: i32,
    pub credentials: Option<DownloadCredentials>,
    pub headers: Option<Headers>,
    /// False files the download without starting it (e.g. into a queue).
    pub start_immediately: bool,
    pub expected_checksum: Option<ExpectedChecksum>,
    pub stream: Option<StreamSource>,
}

impl DownloadRequest {
    pub fn new(url: impl Into<String>, file_info: RemoteFileInfo, save_folder: impl Into<String>, file_name: impl Into<String>, connections_count: i32) -> Self {
        Self {
            url: url.into(),
            file_info,
            save_folder: save_folder.into(),
            file_name: file_name.into(),
            connections_count,
            credentials: None,
            headers: None,
            start_immediately: true,
            expected_checksum: None,
            stream: None,
        }
    }
}

/// A new address for an existing download (Refresh Download Address).
#[derive(Clone, Debug)]
pub struct AddressChange {
    pub url: String,
    pub file_info: RemoteFileInfo,
    pub credentials: Option<DownloadCredentials>,
    pub headers: Option<Headers>,
    /// Throw the chunks away and start over.
    pub discard_progress: bool,
}

/// A download the browser extension handed over.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapturedDownload {
    pub url: String,
    pub file_name: Option<String>,
    pub total_bytes: i64,
    pub mime_type: Option<String>,
    pub referrer: Option<String>,
    pub user_agent: Option<String>,
    pub cookie: Option<String>,
}

impl CapturedDownload {
    /// The headers the browser would have sent: cookie, referrer, user agent.
    pub fn to_headers(&self) -> Headers {
        let mut headers = Headers::new();
        let mut put = |name: &str, value: &Option<String>| {
            if let Some(v) = value.as_deref().filter(|v| !v.trim().is_empty()) {
                headers.insert(name.to_string(), v.to_string());
            }
        };
        put("Cookie", &self.cookie);
        put("Referer", &self.referrer);
        put("User-Agent", &self.user_agent);
        headers
    }
}

// ---------------------------------------------------------------------------
// Updater
// ---------------------------------------------------------------------------

/// `latest.json` on the update endpoint. Only `version` and `downloadUrl` are required.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateManifest {
    #[serde(alias = "Version")]
    pub version: String,
    #[serde(rename = "downloadUrl", alias = "DownloadUrl", alias = "downloadurl")]
    pub download_url: String,
    #[serde(rename = "releaseNotes", alias = "ReleaseNotes")]
    pub release_notes: Option<String>,
    #[serde(rename = "releaseDate", alias = "ReleaseDate")]
    pub release_date: Option<DateTimeOffset>,
    #[serde(rename = "fileSizeBytes", alias = "FileSizeBytes")]
    pub file_size_bytes: i64,
    #[serde(alias = "Sha256", alias = "SHA256")]
    pub sha256: Option<String>,
}

impl UpdateManifest {
    pub fn is_usable(&self) -> bool {
        !self.version.trim().is_empty() && !self.download_url.trim().is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateCheckResult {
    pub update_available: bool,
    pub current_version: String,
    pub manifest: Option<UpdateManifest>,
}

impl UpdateCheckResult {
    pub fn up_to_date(current_version: impl Into<String>, manifest: Option<UpdateManifest>) -> Self {
        Self { update_available: false, current_version: current_version.into(), manifest }
    }
    pub fn available(current_version: impl Into<String>, manifest: UpdateManifest) -> Self {
        Self { update_available: true, current_version: current_version.into(), manifest: Some(manifest) }
    }
    pub fn latest_version(&self) -> &str {
        self.manifest.as_ref().map(|m| m.version.as_str()).unwrap_or(&self.current_version)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UpdateDownloadProgress {
    pub bytes_received: i64,
    pub total_bytes: i64,
    pub speed_bytes_per_second: f64,
}

impl UpdateDownloadProgress {
    pub fn percentage(&self) -> Option<f64> {
        (self.total_bytes > 0).then(|| (self.bytes_received as f64 * 100.0 / self.total_bytes as f64).clamp(0.0, 100.0))
    }
}

/// Convenience: a fresh random id, `Guid.NewGuid()`.
pub fn new_id() -> Uuid {
    Uuid::new_v4()
}
