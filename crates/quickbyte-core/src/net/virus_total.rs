//! VirusTotal's API (v3) over the engine's shared HTTP client — the
//! [`VirusTotalApi`] the background check talks through.
//!
//! **Pacing.** The free API allows four requests a minute per key, and a check
//! takes several (look the address up, submit it, poll the analysis, look the
//! file up…), for every download. So every request waits for its turn —
//! [`PUBLIC_API_INTERVAL`] after the one before, across all downloads — and a
//! `429` (quota used up) is waited out and asked again a couple of times before
//! it counts as a failure. The check runs in the background; slow is fine,
//! a key locked out for the day is not.
//!
//! **What goes over the wire** is the key (as `x-apikey`), and then only an
//! address, a SHA-256 or a file — never a download's login, cookies or
//! headers. Deciding *which* of those may be sent is `helpers::virus_total`'s
//! business, not this module's.

use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Method;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::{CoreError, Result};
use crate::helpers::virus_total::{ScanResult, ScanStats, url_id};
use crate::net::http_client::{HttpClient, map_reqwest_error, with_deadline};
use crate::traits::VirusTotalApi;

pub const API_BASE: &str = "https://www.virustotal.com/api/v3";

/// The free API's four requests a minute.
pub const PUBLIC_API_INTERVAL: Duration = Duration::from_secs(15);

/// How long a `429` is waited out before asking again, and how many times.
const QUOTA_BACKOFF: Duration = Duration::from_secs(60);
const QUOTA_RETRIES: u32 = 2;

/// For a lookup; an upload of up to 25 MB on a slow line gets longer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// A file every VirusTotal account can look up: the empty one. Checking a key
/// against it costs one request and needs nothing of the user's.
const EMPTY_FILE_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

pub const KEY_REJECTED: &str = "VirusTotal did not accept the API key. Check it in Options > VirusTotal.";
pub const QUOTA_USED: &str = "The VirusTotal API key's request quota is used up for now.";

pub struct VirusTotalClient {
    http: Arc<HttpClient>,
    base: String,
    min_interval: Duration,
    quota_backoff: Duration,
    /// When the next request may go. Held while waiting, so requests go out one
    /// at a time and in the order they asked.
    next_slot: tokio::sync::Mutex<Option<tokio::time::Instant>>,
}

impl VirusTotalClient {
    /// The real API over the shared client, paced for the free quota.
    pub fn new() -> Self {
        Self::with(API_BASE, HttpClient::shared(), PUBLIC_API_INTERVAL, QUOTA_BACKOFF)
    }

    /// Another endpoint, client and pacing — for tests against a local server.
    pub fn with(base: &str, http: Arc<HttpClient>, min_interval: Duration, quota_backoff: Duration) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            min_interval,
            quota_backoff,
            next_slot: tokio::sync::Mutex::new(None),
        }
    }

    async fn wait_turn(&self, cancel: &CancellationToken) -> Result<()> {
        let mut slot = self.next_slot.lock().await;
        if let Some(at) = *slot {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                _ = tokio::time::sleep_until(at) => {}
            }
        }
        *slot = Some(tokio::time::Instant::now() + self.min_interval);
        Ok(())
    }

    /// One call, paced, with a `429` waited out. Returns the status and body.
    async fn call(
        &self,
        api_key: &str,
        method: Method,
        path: &str,
        body: Option<(&str, Vec<u8>)>,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(u16, Vec<u8>)> {
        let url = format!("{}{path}", self.base);
        let mut attempt = 0;
        loop {
            self.wait_turn(cancel).await?;
            let mut builder = self
                .http
                .reqwest()
                .request(method.clone(), &url)
                .header("x-apikey", api_key.trim())
                .header(ACCEPT, "application/json");
            if let Some((content_type, bytes)) = &body {
                builder = builder.header(CONTENT_TYPE, *content_type).body(bytes.clone());
            }
            let proxy = self.http.proxy();
            let exchange = async {
                let response = builder.send().await.map_err(|e| map_reqwest_error(&e, proxy))?;
                let status = response.status().as_u16();
                let bytes = response.bytes().await.map_err(|e| map_reqwest_error(&e, proxy))?;
                Ok((status, bytes.to_vec()))
            };
            let (status, bytes) = with_deadline(exchange, Some(timeout), cancel).await?;
            if status == 429 && attempt < QUOTA_RETRIES {
                attempt += 1;
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                    _ = tokio::time::sleep(self.quota_backoff) => {}
                }
                continue;
            }
            return Ok((status, bytes));
        }
    }

    /// A lookup: the object on a 200, `None` on a 404, an error otherwise.
    async fn lookup(&self, api_key: &str, path: &str, cancel: &CancellationToken) -> Result<Option<Value>> {
        let (status, body) = self.call(api_key, Method::GET, path, None, REQUEST_TIMEOUT, cancel).await?;
        match status {
            200 => parse(&body).map(Some),
            404 => Ok(None),
            _ => Err(error_for(status, &body)),
        }
    }

    /// A submission: the analysis id it was filed under.
    async fn submit(&self, api_key: &str, path: &str, body: (&str, Vec<u8>), timeout: Duration, cancel: &CancellationToken) -> Result<String> {
        let (status, answer) = self.call(api_key, Method::POST, path, Some(body), timeout, cancel).await?;
        if status != 200 {
            return Err(error_for(status, &answer));
        }
        parse(&answer)?
            .pointer("/data/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| CoreError::other("VirusTotal's answer had no analysis id."))
    }
}

impl Default for VirusTotalClient {
    fn default() -> Self {
        Self::new()
    }
}

fn parse(body: &[u8]) -> Result<Value> {
    serde_json::from_slice(body).map_err(|e| CoreError::other(format!("VirusTotal's answer could not be read: {e}")))
}

fn stats_at(value: &Value, pointer: &str) -> ScanStats {
    value.pointer(pointer).cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

fn string_at(value: &Value, pointer: &str) -> Option<String> {
    value.pointer(pointer).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

/// The report an address or file lookup returns.
fn report_of(value: &Value) -> ScanResult {
    ScanResult { stats: stats_at(value, "/data/attributes/last_analysis_stats"), object_id: string_at(value, "/data/id") }
}

/// VirusTotal's own words for a refusal, where it gave any.
fn error_for(status: u16, body: &[u8]) -> CoreError {
    let said = serde_json::from_slice::<Value>(body).ok().and_then(|v| string_at(&v, "/error/message"));
    match status {
        401 => CoreError::auth(KEY_REJECTED, true),
        429 => CoreError::http(Some(429), QUOTA_USED),
        _ => CoreError::http(
            Some(status),
            match said {
                Some(message) => format!("VirusTotal answered {status}: {message}"),
                None => format!("VirusTotal answered {status}."),
            },
        ),
    }
}

/// `multipart/form-data` with one part, `file`. Built by hand: it is the only
/// multipart request the engine makes.
fn multipart(file_name: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = format!("----QuickByte{}", uuid::Uuid::new_v4().simple());
    let safe_name: String = file_name.chars().map(|c| if c == '"' || c == '\\' || c.is_control() { '_' } else { c }).collect();
    let mut body = Vec::with_capacity(bytes.len() + 256);
    let _ = write!(
        body,
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{safe_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    );
    body.extend_from_slice(bytes);
    let _ = write!(body, "\r\n--{boundary}--\r\n");
    (format!("multipart/form-data; boundary={boundary}"), body)
}

#[async_trait]
impl VirusTotalApi for VirusTotalClient {
    async fn url_report(&self, api_key: &str, url: &str, cancel: &CancellationToken) -> Result<Option<ScanResult>> {
        Ok(self.lookup(api_key, &format!("/urls/{}", url_id(url)), cancel).await?.as_ref().map(report_of))
    }

    async fn submit_url(&self, api_key: &str, url: &str, cancel: &CancellationToken) -> Result<String> {
        let form = url::form_urlencoded::Serializer::new(String::new()).append_pair("url", url).finish();
        self.submit(api_key, "/urls", ("application/x-www-form-urlencoded", form.into_bytes()), REQUEST_TIMEOUT, cancel).await
    }

    async fn analysis(&self, api_key: &str, analysis_id: &str, cancel: &CancellationToken) -> Result<Option<ScanResult>> {
        let path = format!("/analyses/{}", percent_encoding::utf8_percent_encode(analysis_id, percent_encoding::NON_ALPHANUMERIC));
        let Some(value) = self.lookup(api_key, &path, cancel).await? else {
            return Err(CoreError::other("VirusTotal no longer has the analysis it started."));
        };
        if value.pointer("/data/attributes/status").and_then(Value::as_str) != Some("completed") {
            return Ok(None);
        }
        Ok(Some(ScanResult {
            stats: stats_at(&value, "/data/attributes/stats"),
            object_id: string_at(&value, "/meta/url_info/id").or_else(|| string_at(&value, "/meta/file_info/sha256")),
        }))
    }

    async fn file_report(&self, api_key: &str, sha256: &str, cancel: &CancellationToken) -> Result<Option<ScanResult>> {
        Ok(self.lookup(api_key, &format!("/files/{}", sha256.to_ascii_lowercase()), cancel).await?.as_ref().map(report_of))
    }

    async fn upload_file(&self, api_key: &str, path: &str, cancel: &CancellationToken) -> Result<String> {
        let bytes = tokio::fs::read(path).await?;
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
        let (content_type, body) = multipart(&name, &bytes);
        self.submit(api_key, "/files", (content_type.as_str(), body), UPLOAD_TIMEOUT, cancel).await
    }

    async fn check_key(&self, api_key: &str, cancel: &CancellationToken) -> Result<()> {
        if api_key.trim().is_empty() {
            return Err(CoreError::invalid("Enter a VirusTotal API key."));
        }
        self.lookup(api_key, &format!("/files/{EMPTY_FILE_SHA256}"), cancel).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_upload_is_one_file_part_with_a_safe_name() {
        let (content_type, body) = multipart("a \"b\".exe", b"MZ");
        let boundary = content_type.strip_prefix("multipart/form-data; boundary=").unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with(&format!("--{boundary}\r\n")));
        assert!(text.contains("name=\"file\"; filename=\"a _b_.exe\""), "{text}");
        assert!(text.ends_with(&format!("\r\nMZ\r\n--{boundary}--\r\n")));
    }

    #[test]
    fn a_refusal_says_what_virus_total_said() {
        assert!(matches!(error_for(401, b""), CoreError::AuthenticationRequired { .. }));
        assert_eq!(error_for(429, b"").to_string(), QUOTA_USED);
        let said = br#"{"error":{"code":"ForbiddenError","message":"You are not allowed to perform the requested operation"}}"#;
        assert_eq!(error_for(403, said).to_string(), "VirusTotal answered 403: You are not allowed to perform the requested operation");
        assert_eq!(error_for(500, b"<html>").to_string(), "VirusTotal answered 500.");
    }

    #[test]
    fn a_report_reads_its_tally_and_id() {
        let value: Value = serde_json::from_str(
            r#"{"data":{"id":"abc","attributes":{"last_analysis_stats":{"malicious":2,"harmless":60,"undetected":8}}}}"#,
        )
        .unwrap();
        let report = report_of(&value);
        assert_eq!(report.object_id.as_deref(), Some("abc"));
        assert_eq!(report.stats.malicious, 2);
        assert_eq!(report.stats.engines(), 70);
    }
}
