//! Port of `src/QuickByte.Core/Services/UpdateService.cs`.
//!
//! Reads the release manifest from a fixed endpoint and, when it describes a
//! newer build, downloads that build's installer to a temp folder.
//!
//! Deliberately does *not* go through the segmented download engine: an update
//! is not a user download. It must not appear in the list, must not persist to
//! downloads.json, must not compete for the concurrency gate and must not be
//! throttled by the user's speed limit — and a single-stream copy of an
//! installer is not worth eight sockets.
//!
//! Running the installer is the caller's job. Core has no business deciding
//! when the application is allowed to exit.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use reqwest::header::{CACHE_CONTROL, USER_AGENT};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::{CoreError, Result};
use crate::helpers::{app_user_agent, file_name_helper, product_version, speed_calculator::SpeedCalculator};
use crate::models::{UpdateCheckResult, UpdateDownloadProgress, UpdateManifest};
use crate::net::http_client::{HttpClient, HttpRequest, with_deadline};
use crate::traits::ProgressFn;

/// Where releases are announced. Hardcoded on purpose: an update endpoint that a
/// user — or anything else able to write settings.json — could repoint is a way
/// to make QuickByte download and run an arbitrary executable.
pub const DEFAULT_MANIFEST_URL: &str = "https://quickbyte-app.pages.dev/releases/latest.json";

const BUFFER_SIZE: usize = 81_920;
/// The C# client's `Timeout`: the manifest fetch, and the installer's headers.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The installer's body is bounded by this far longer leash instead.
const INSTALLER_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// A manifest is a few hundred bytes; this is generous.
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const CLEANUP_ATTEMPTS: usize = 5;
const CLEANUP_RETRY_DELAY: Duration = Duration::from_millis(2000);

pub struct UpdateService {
    manifest_url: String,
    download_folder: PathBuf,
    http: Arc<HttpClient>,
    /// The installer this process fetched, if any. Kept for the lifetime of the
    /// process so the cleanup sweep can't delete the file the user is about to be
    /// handed: it is only stale from the *next* launch's point of view.
    installer_this_session: Mutex<Option<PathBuf>>,
}

impl Default for UpdateService {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl UpdateService {
    /// `manifest_url` is overridable only so a release can be staged against a
    /// test endpoint; the app itself always takes the default. `download_folder`
    /// defaults to `%TEMP%\QuickByte\updates`.
    pub fn new(manifest_url: Option<&str>, download_folder: Option<PathBuf>) -> Self {
        Self::with_client(manifest_url, download_folder, HttpClient::shared())
    }

    pub fn with_client(manifest_url: Option<&str>, download_folder: Option<PathBuf>, http: Arc<HttpClient>) -> Self {
        Self {
            manifest_url: manifest_url.filter(|u| !u.trim().is_empty()).unwrap_or(DEFAULT_MANIFEST_URL).to_string(),
            download_folder: download_folder.unwrap_or_else(|| std::env::temp_dir().join("QuickByte").join("updates")),
            http,
            installer_this_session: Mutex::new(None),
        }
    }

    pub fn download_folder(&self) -> &Path {
        &self.download_folder
    }

    /// The two requests here are the only ones in the engine that carry no
    /// download's options, so they stamp the User-Agent themselves — the user's
    /// custom value, for the reason they get the user's proxy: a network that
    /// refuses a client it does not recognise refuses the manifest fetch too, and
    /// an update check that silently reports "no update available" on a network
    /// where every other download works is the failure this avoids.
    fn request(&self, url: Url) -> HttpRequest {
        let mut request = HttpRequest::get(url);
        request.set_header(USER_AGENT, &app_user_agent::current());
        request
    }

    /// Rejects anything that isn't a plain HTTPS URL. This is the file the app
    /// is about to hand to the shell, so "the manifest said so" is not on its own
    /// reason enough to fetch it over a channel that can be rewritten in transit.
    fn resolve_installer_url(manifest: &UpdateManifest) -> Result<Url> {
        match Url::parse(manifest.download_url.trim()) {
            Ok(url) if url.scheme() == "https" => Ok(url),
            _ => Err(CoreError::other("The update download link is not a valid HTTPS URL.")),
        }
    }

    async fn fetch_installer(
        &self,
        url: Url,
        manifest: &UpdateManifest,
        progress: Option<ProgressFn<UpdateDownloadProgress>>,
        cancel: &CancellationToken,
    ) -> Result<String> {
        fs::create_dir_all(&self.download_folder)?;

        // Whatever is already in there belongs to an earlier update and is dead
        // weight the moment this one lands; %TEMP% is nobody's idea of a release
        // archive.
        self.purge_installers();

        let target = self.download_folder.join(format!("QuickByte-{}-Setup.exe", file_name_helper::sanitize_file_name(&manifest.version)));

        // Claimed before the first byte, so a cleanup sweep still retrying in the
        // background walks past this file instead of deleting it.
        *self.installer_this_session.lock() = Some(target.clone());

        let copied = with_deadline(self.copy_to_file(url, &target, manifest.file_size_bytes, progress, cancel), Some(INSTALLER_TIMEOUT), cancel).await;
        let result = match copied {
            Ok(()) => verify_hash(target.clone(), manifest.sha256.clone()).await,
            Err(e) => Err(e),
        };
        if let Err(error) = result {
            // A partial or mismatched installer is worse than none: left behind,
            // it is a plausible-looking executable sitting in a temp folder.
            try_delete(&target);
            return Err(error);
        }
        Ok(target.to_string_lossy().into_owned())
    }

    async fn copy_to_file(
        &self,
        url: Url,
        target: &Path,
        declared_size: i64,
        progress: Option<ProgressFn<UpdateDownloadProgress>>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let mut request = self.request(url);
        request.timeout = Some(REQUEST_TIMEOUT);
        let response = self.http.send(&request, cancel).await?.error_for_status()?;
        let total = response.content_length().unwrap_or(declared_size);

        let mut body = response.into_reader();
        let mut file = std::io::BufWriter::with_capacity(BUFFER_SIZE, fs::File::create(target)?);
        let speed = SpeedCalculator::new(Some(Duration::from_secs(3)));
        let mut buffer = vec![0u8; BUFFER_SIZE];
        let mut received: i64 = 0;
        let mut last_report = Instant::now();

        loop {
            let read = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                read = body.read(&mut buffer) => read?,
            };
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read])?;
            received += read as i64;
            speed.add_sample(received);

            // Throttled for the same reason the merger throttles its reports: one
            // report per buffer floods the UI thread it is trying to inform.
            // 100 ms matches the engine's own sampling interval.
            if let Some(report) = progress.as_ref()
                && last_report.elapsed() >= Duration::from_millis(100)
            {
                last_report = Instant::now();
                report(UpdateDownloadProgress { bytes_received: received, total_bytes: total, speed_bytes_per_second: speed.speed_bytes_per_second() });
            }
        }
        file.flush()?;

        // The throttle above can swallow the last sample, and a bar that stops at
        // 99.4% reads as a stall.
        if let Some(report) = progress.as_ref() {
            report(UpdateDownloadProgress {
                bytes_received: received,
                total_bytes: if total > 0 { total } else { received },
                speed_bytes_per_second: speed.speed_bytes_per_second(),
            });
        }
        Ok(())
    }

    /// Deletes every installer in the update folder bar one this process
    /// downloaded, reporting how many are still there afterwards so the caller
    /// knows whether another attempt is worth making.
    fn purge_installers(&self) -> (usize, usize) {
        let Ok(entries) = fs::read_dir(&self.download_folder) else { return (0, 0) };
        let keep = self.installer_this_session.lock().clone();
        let (mut deleted, mut remaining) = (0, 0);
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if !path.is_file() || !(name.starts_with("quickbyte-") && name.ends_with("-setup.exe")) {
                continue;
            }
            if keep.as_ref().is_some_and(|k| k.to_string_lossy().eq_ignore_ascii_case(&path.to_string_lossy())) {
                continue;
            }
            if try_delete(&path) {
                deleted += 1;
            } else {
                remaining += 1;
            }
        }
        (deleted, remaining)
    }

    fn try_remove_folder_if_empty(&self) {
        if fs::read_dir(&self.download_folder).is_ok_and(|mut entries| entries.next().is_none()) {
            let _ = fs::remove_dir(&self.download_folder);
        }
    }
}

#[async_trait]
impl crate::traits::UpdateService for UpdateService {
    fn manifest_url(&self) -> &str {
        &self.manifest_url
    }

    /// Fetches the release manifest and compares it against `current_version`.
    /// Fails on network or parse failure — a background caller swallows that, a
    /// manual one shows it, and the service shouldn't have to guess which.
    async fn check_for_update(&self, current_version: &str, cancel: &CancellationToken) -> Result<UpdateCheckResult> {
        let url = Url::parse(&self.manifest_url).map_err(|_| CoreError::invalid("The update address is not valid."))?;
        let mut request = self.request(url);
        // A CDN happily serving yesterday's manifest is the one failure mode that
        // looks exactly like "no update available", so ask for a fresh one.
        request.set_header(CACHE_CONTROL, "no-cache");

        let fetch = async {
            let response = self.http.send(&request, cancel).await?.error_for_status()?;
            response.read_at_most(MAX_MANIFEST_BYTES).await?.ok_or_else(|| CoreError::other("The update manifest could not be read."))
        };
        let bytes = with_deadline(fetch, Some(REQUEST_TIMEOUT), cancel).await?;

        let manifest = parse_manifest(&String::from_utf8_lossy(&bytes))?;
        let Some(manifest) = manifest.filter(UpdateManifest::is_usable) else {
            return Ok(UpdateCheckResult::up_to_date(current_version, None));
        };
        Ok(if product_version::is_newer(Some(&manifest.version), Some(current_version)) {
            UpdateCheckResult::available(current_version, manifest)
        } else {
            UpdateCheckResult::up_to_date(current_version, Some(manifest))
        })
    }

    /// Downloads the installer to the update folder and returns its full path,
    /// verifying the manifest's SHA-256 when one is given.
    async fn download_installer(
        &self,
        manifest: &UpdateManifest,
        progress: Option<ProgressFn<UpdateDownloadProgress>>,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let url = Self::resolve_installer_url(manifest)?;
        self.fetch_installer(url, manifest, progress, cancel).await
    }

    /// Deletes installers left in the update folder by earlier runs, and removes
    /// the folder once it is empty.
    ///
    /// Nothing else is in a position to: setup cannot delete the file it is
    /// executing from, and the QuickByte that fetched it has already exited by
    /// then — that is the whole point of exiting. So an update leaves a
    /// multi-megabyte .exe in %TEMP% behind it, and the next launch is the first
    /// moment that file is both finished with and unlocked.
    ///
    /// Best-effort throughout: a locked or vanished file is skipped, not
    /// reported. Anything that survives is picked up by the launch after this one.
    async fn cleanup_downloaded_installers(&self) -> usize {
        let mut deleted = 0;
        for attempt in 0..CLEANUP_ATTEMPTS {
            // The first pass is immediate. The retries exist for the launch setup
            // itself performs: it starts the new QuickByte and only then finishes
            // and releases its own file, so the first delete of that one loses the
            // race by a second or two.
            if attempt > 0 {
                tokio::time::sleep(CLEANUP_RETRY_DELAY).await;
            }
            let (removed, remaining) = self.purge_installers();
            deleted += removed;
            if remaining == 0 {
                break;
            }
        }
        self.try_remove_folder_if_empty();
        deleted
    }
}

/// `true` when the file is gone, whether or not this call is what removed it.
fn try_delete(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

async fn verify_hash(path: PathBuf, expected: Option<String>) -> Result<()> {
    let Some(expected) = expected.filter(|e| !e.trim().is_empty()) else { return Ok(()) };
    let actual = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
        let mut file = fs::File::open(&path)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; BUFFER_SIZE];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(hasher.finalize().iter().map(|b| format!("{b:02X}")).collect())
    })
    .await
    .map_err(|e| CoreError::other(e.to_string()))??;

    if !actual.eq_ignore_ascii_case(expected.replace('-', "").trim()) {
        return Err(CoreError::other("The downloaded installer failed its integrity check and was discarded."));
    }
    Ok(())
}

/// Reads `latest.json` as the C# build did: property names in any case,
/// comments and trailing commas tolerated (the file is written by hand).
pub fn parse_manifest(json: &str) -> Result<Option<UpdateManifest>> {
    let unreadable = || CoreError::other("The update manifest could not be read.");
    let cleaned = strip_comments_and_trailing_commas(json.trim_start_matches('\u{feff}'));
    let value: serde_json::Value = serde_json::from_str(&cleaned).map_err(|_| unreadable())?;
    let object = match value {
        serde_json::Value::Null => return Ok(None),
        serde_json::Value::Object(object) => object,
        _ => return Err(unreadable()),
    };

    let mut canonical = serde_json::Map::new();
    for (key, value) in object {
        let name = match key.to_ascii_lowercase().as_str() {
            "version" => "version",
            "downloadurl" => "downloadUrl",
            "releasenotes" => "releaseNotes",
            "releasedate" => "releaseDate",
            "filesizebytes" => "fileSizeBytes",
            "sha256" => "sha256",
            _ => continue,
        };
        // A null is the property's default, as System.Text.Json read it.
        if !value.is_null() {
            canonical.insert(name.to_string(), value);
        }
    }
    serde_json::from_value(serde_json::Value::Object(canonical)).map(Some).map_err(|_| unreadable())
}

/// Drops `//` and `/* */` comments outside strings, and commas that close an
/// array or object.
fn strip_comments_and_trailing_commas(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut out = String::with_capacity(json.len());
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            ',' => {
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if !matches!(chars.get(j), Some('}') | Some(']')) {
                    out.push(c);
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hand_written_manifest_is_read_in_any_case_with_comments() {
        let manifest = parse_manifest(
            r#"{
                // the release
                "Version": "1.4.0",
                "DOWNLOADURL": "https://example.com/QuickByte-1.4.0-Setup.exe", /* the installer */
                "releaseNotes": null,
                "fileSizeBytes": 7340032,
                "sha256": "abc",
            }"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(manifest.version, "1.4.0");
        assert_eq!(manifest.download_url, "https://example.com/QuickByte-1.4.0-Setup.exe");
        assert_eq!(manifest.release_notes, None);
        assert_eq!(manifest.file_size_bytes, 7_340_032);
        assert_eq!(manifest.sha256.as_deref(), Some("abc"));
    }

    #[test]
    fn a_manifest_that_is_not_json_is_an_error_and_null_is_nothing() {
        assert_eq!(parse_manifest("<html>").unwrap_err().to_string(), "The update manifest could not be read.");
        assert_eq!(parse_manifest("null").unwrap(), None);
        assert_eq!(parse_manifest(r#"{"version": 3}"#).unwrap_err().to_string(), "The update manifest could not be read.");
    }

    #[test]
    fn only_an_https_installer_is_fetched() {
        for url in ["http://example.com/QuickByte-1.4.0-Setup.exe", "file:///C:/evil.exe", "not a url", ""] {
            let manifest = UpdateManifest { version: "1.4.0".into(), download_url: url.into(), ..Default::default() };
            assert_eq!(
                UpdateService::resolve_installer_url(&manifest).unwrap_err().to_string(),
                "The update download link is not a valid HTTPS URL."
            );
        }
        let manifest = UpdateManifest { version: "1.4.0".into(), download_url: "HTTPS://example.com/a.exe".into(), ..Default::default() };
        assert!(UpdateService::resolve_installer_url(&manifest).is_ok());
    }

    #[test]
    fn the_default_endpoint_is_the_website() {
        let service = UpdateService::new(None, Some(std::env::temp_dir().join("qb-unused")));
        assert_eq!(crate::traits::UpdateService::manifest_url(&service), "https://quickbyte-app.pages.dev/releases/latest.json");
        let staged = UpdateService::new(Some("  "), None);
        assert_eq!(crate::traits::UpdateService::manifest_url(&staged), DEFAULT_MANIFEST_URL);
        assert!(staged.download_folder().ends_with("QuickByte\\updates") || staged.download_folder().ends_with("QuickByte/updates"));
    }

    #[tokio::test]
    async fn the_sweep_deletes_old_installers_and_then_the_folder() {
        let folder = std::env::temp_dir().join("QuickByte.Tests").join(uuid::Uuid::new_v4().simple().to_string()).join("updates");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("QuickByte-1.2.0-Setup.exe"), b"old").unwrap();
        fs::write(folder.join("quickbyte-1.3.0-setup.EXE"), b"older").unwrap();
        let service = UpdateService::new(None, Some(folder.clone()));

        let deleted = crate::traits::UpdateService::cleanup_downloaded_installers(&service).await;

        assert_eq!(deleted, 2);
        assert!(!folder.exists());
        let _ = fs::remove_dir_all(folder.parent().unwrap());
    }

    #[tokio::test]
    async fn the_sweep_leaves_what_is_not_an_installer_and_the_one_this_session_fetched() {
        let folder = std::env::temp_dir().join("QuickByte.Tests").join(uuid::Uuid::new_v4().simple().to_string());
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("notes.txt"), b"keep").unwrap();
        fs::write(folder.join("QuickByte-1.4.0-Setup.exe"), b"current").unwrap();
        let service = UpdateService::new(None, Some(folder.clone()));
        *service.installer_this_session.lock() = Some(folder.join("QuickByte-1.4.0-Setup.exe"));

        assert_eq!(crate::traits::UpdateService::cleanup_downloaded_installers(&service).await, 0);
        assert!(folder.join("notes.txt").exists());
        assert!(folder.join("QuickByte-1.4.0-Setup.exe").exists());
        let _ = fs::remove_dir_all(&folder);
    }

    /// A one-file HTTP server on loopback (the HTTPS rule is checked above; the
    /// fetch itself is protocol-agnostic).
    async fn serve_once_per_request(body: &'static [u8]) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let mut head = Vec::new();
                    let mut one = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if stream.read(&mut one).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push(one[0]);
                    }
                    let reply = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = stream.write_all(reply.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                });
            }
        });
        port
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    #[tokio::test]
    async fn an_installer_is_fetched_verified_and_spared_by_the_sweep() {
        const BODY: &[u8] = b"MZ pretend installer";
        let port = serve_once_per_request(BODY).await;
        let folder = std::env::temp_dir().join("QuickByte.Tests").join(uuid::Uuid::new_v4().simple().to_string());
        let http = HttpClient::new(Arc::new(crate::net::app_proxy::AppProxy::new(None)));
        let service = UpdateService::with_client(None, Some(folder.clone()), http);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("QuickByte-1.0.0-Setup.exe"), b"stale").unwrap();

        let manifest = UpdateManifest { version: "1.30.0".into(), sha256: Some(sha256_hex(BODY)), ..Default::default() };
        let reports = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&reports);
        let progress: ProgressFn<UpdateDownloadProgress> = Arc::new(move |p| seen.lock().push(p));
        let url = Url::parse(&format!("http://127.0.0.1:{port}/download")).unwrap();

        let path = service.fetch_installer(url.clone(), &manifest, Some(progress), &CancellationToken::new()).await.unwrap();

        assert!(path.ends_with("QuickByte-1.30.0-Setup.exe"));
        assert_eq!(fs::read(&path).unwrap(), BODY);
        // The earlier installer was purged before this one landed.
        assert!(!folder.join("QuickByte-1.0.0-Setup.exe").exists());
        let last = *reports.lock().last().unwrap();
        assert_eq!((last.bytes_received, last.total_bytes), (BODY.len() as i64, BODY.len() as i64));

        // The file the user is about to be handed survives this session's sweep.
        assert_eq!(crate::traits::UpdateService::cleanup_downloaded_installers(&service).await, 0);
        assert!(Path::new(&path).exists());

        // A mismatched one is deleted, never left looking plausible.
        let wrong = UpdateManifest { version: "1.31.0".into(), sha256: Some("00".into()), ..Default::default() };
        let error = service.fetch_installer(url, &wrong, None, &CancellationToken::new()).await.unwrap_err();
        assert_eq!(error.to_string(), "The downloaded installer failed its integrity check and was discarded.");
        assert!(!folder.join("QuickByte-1.31.0-Setup.exe").exists());
        let _ = fs::remove_dir_all(&folder);
    }

    #[tokio::test]
    async fn a_mismatched_hash_is_refused() {
        let path = std::env::temp_dir().join(format!("qb-hash-{}.bin", uuid::Uuid::new_v4().simple()));
        fs::write(&path, b"hello").unwrap();
        // SHA-256("hello"), with dashes and odd case as people paste it.
        let good = "2CF24DBA-5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824";
        assert!(verify_hash(path.clone(), Some(good.into())).await.is_ok());
        assert!(verify_hash(path.clone(), None).await.is_ok());
        assert_eq!(
            verify_hash(path.clone(), Some("00".into())).await.unwrap_err().to_string(),
            "The downloaded installer failed its integrity check and was discarded."
        );
        let _ = fs::remove_file(&path);
    }
}
