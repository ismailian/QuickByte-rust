//! Port of `src/QuickByte.Core/Services/RemoteFileInfoProvider.cs`.
//!
//! Resolves remote file metadata over HTTP(S). A HEAD request first (cheap),
//! then a ranged GET probe (`bytes=0-0`) which is the only thing that can answer
//! the question the pool actually needs answered: not whether the server says
//! it supports ranges, but whether asking for one gets a range back.
//!
//! A 401 is reported as [`CoreError::AuthenticationRequired`] rather than a
//! generic failure: it is the one error the user can actually do something
//! about, and the Add Download dialog turns it into a credentials prompt instead
//! of a dead end. Only a 401 to the ranged GET counts — a HEAD can be routed
//! somewhere the GET is not, and github.com does exactly that.
//!
//! **Why the probe is not optional.** HEAD used to be allowed to settle range
//! support on its own when it also supplied a size and a name, on the strength
//! of `Accept-Ranges: bytes`. That header is a claim, and the thing making it is
//! frequently a CDN edge rather than the origin — one which, holding the whole
//! file in cache, answers a `Range` request with the whole body and a `200`
//! while passing the origin's `Accept-Ranges` through unchanged. Nothing about
//! that response is an error in HTTP. It just means the file gets split eight
//! ways and seven of those connections receive bytes belonging at offset zero —
//! a download that fails outright, and does so only on the servers that look
//! the most cooperative.
//!
//! **Why a 200 to the probe is not the end of it.** The same contradiction —
//! claims ranges, ignores one — is the signature of a cache in the middle rather
//! than of a server that cannot seek. So the probe is asked a second time with
//! `Cache-Control: no-cache`, which is what makes a shared cache go and ask the
//! origin. When that comes back `206`, ranges work, and
//! [`RemoteFileInfo::bypass_cache`] records that they only work when asked for
//! that way, so every connection asks the same way. (protonvpn.com's download
//! endpoint behaves exactly like this; it is the reason any of this is here.)

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use percent_encoding::percent_decode;
use reqwest::header::RANGE;
use reqwest::{Method, StatusCode};
use tokio_util::sync::CancellationToken;

use crate::error::{CoreError, Result};
use crate::helpers::{file_name_helper, file_version_check};
use crate::models::{RemoteFileInfo, RequestOptions};
use crate::net::http_client::{HttpClient, HttpRequest, HttpResponse, parse_url};
use crate::net::{authenticated_http, request_decorator};
use crate::serde_compat::DateTimeOffset;

/// The probe's budget for each request's headers, as the C# probe client's `Timeout`.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct RemoteFileInfoProvider {
    http: Arc<HttpClient>,
}

impl Default for RemoteFileInfoProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteFileInfoProvider {
    /// The probe on the engine's shared client — it goes the way the
    /// connections will, or it describes a file they then cannot reach.
    pub fn new() -> Self {
        Self { http: HttpClient::shared() }
    }

    /// The seam the tests need, and the only one: everything this exists to get
    /// right is about servers that answer *incorrectly*, which a real server
    /// cannot be asked to do on demand.
    pub fn with_client(http: Arc<HttpClient>) -> Self {
        Self { http }
    }

    fn build(&self, method: Method, url: &url::Url, options: Option<&RequestOptions>, ranged: bool) -> HttpRequest {
        let mut request = HttpRequest::new(method, url.clone());
        if ranged {
            request.set_header(RANGE, "bytes=0-0");
        }
        request_decorator::apply(&mut request.headers, options);
        request.timeout = Some(PROBE_TIMEOUT);
        request
    }

    /// Asks for one byte and reports what came back. Sets
    /// `supports_range_requests` only on a genuine `206`, and escalates past an
    /// intermediary cache once when the response contradicts the server's own
    /// `Accept-Ranges` claim. Returns the file name the server named, if any.
    async fn probe_range_support(
        &self,
        url: &url::Url,
        info: &mut RemoteFileInfo,
        options: Option<&RequestOptions>,
        validators: &mut ValidatorTally,
        cancel: &CancellationToken,
    ) -> Result<Option<String>> {
        let request = self.build(Method::GET, url, options, true);
        // Headers only (which every send here is): a server that ignores the
        // range is about to start sending the entire file, and the status line is
        // all this needs. Dropping the response is what stops it.
        let response = authenticated_http::send(&self.http, &request, options, cancel).await?;

        throw_if_challenged(&response, info, options)?;
        let response = response.error_for_status()?;
        let file_name = absorb(info, &response, validators, true);

        if response.status() == StatusCode::PARTIAL_CONTENT {
            info.supports_range_requests = true;
            return Ok(file_name);
        }
        drop(response);

        // A whole-file answer to a one-byte request. If the server never claimed
        // to do ranges, that is simply the truth and costs one connection. If it
        // did claim it, something between here and the origin is answering from
        // a copy it cannot slice — and that is worth one more request to check.
        // Not re-checked when the caller already asked past the cache: the same
        // request cannot produce a different answer, and the probe would loop.
        if !info.server_claims_range_support || info.bypass_cache {
            return Ok(file_name);
        }

        let bypassing = options.cloned().unwrap_or_default().with_cache_bypass();
        let request = self.build(Method::GET, url, Some(&bypassing), true);
        let revalidated = authenticated_http::send(&self.http, &request, Some(&bypassing), cancel).await?;
        if revalidated.status() != StatusCode::PARTIAL_CONTENT {
            return Ok(file_name);
        }

        info.supports_range_requests = true;
        info.bypass_cache = true;
        Ok(absorb(info, &revalidated, validators, true).or(file_name))
    }
}

#[async_trait]
impl crate::traits::RemoteFileInfoProvider for RemoteFileInfoProvider {
    async fn get_file_info(&self, url: &str, options: &RequestOptions, cancel: &CancellationToken) -> Result<RemoteFileInfo> {
        let options = Some(options);
        let mut info = RemoteFileInfo {
            final_url: url.to_string(),
            // Carried in, not reset: a download that already learned it has to ask
            // past a cache probes that way from the start, and the probe below must
            // not then read its own success as "no bypass needed" and strip the
            // headers off every connection.
            bypass_cache: options.is_some_and(|o| o.bypass_cache),
            ..Default::default()
        };
        let parsed = parse_url(url)?;

        let mut server_file_name = None;
        let mut head_answered_everything = false;
        let mut validators = ValidatorTally::default();

        let head = self.build(Method::HEAD, &parsed, options, false);
        match authenticated_http::send(&self.http, &head, options, cancel).await {
            // A 401 here is deliberately not a credentials prompt. HEAD is not
            // always routed where GET is: github.com sends a HEAD from a signed-in
            // browser (anything carrying logged_in=yes) to a legacy asset host that
            // refuses everyone with a 401, while the same GET goes to one that
            // serves the file. Browsers never send HEAD, so the link works there.
            // The ranged GET below is the request the download will actually make,
            // so it is the one whose challenge counts.
            Ok(response) if response.is_success() => {
                server_file_name = absorb(&mut info, &response, &mut validators, false);
                // Enough to name and size the file, but never enough to decide how
                // many connections fetch it. That is what the probe is for.
                head_answered_everything = info.has_known_size() && server_file_name.is_some();
            }
            Ok(_) => {}
            // Cancellation (a timeout too, as HttpClient's was one) ends the probe.
            Err(e @ (CoreError::Cancelled | CoreError::Timeout)) => return Err(e),
            // Some servers reject HEAD entirely — fall through to the GET probe.
            Err(_) => {}
        }

        // Not short-circuited on a HEAD that answered everything — that is exactly
        // the case this exists to stop trusting.
        match self.probe_range_support(&parsed, &mut info, options, &mut validators, cancel).await {
            Ok(probed_name) => {
                if server_file_name.is_none() {
                    server_file_name = probed_name;
                }
            }
            Err(e)
                if head_answered_everything
                    && !matches!(e, CoreError::Cancelled | CoreError::Timeout | CoreError::AuthenticationRequired { .. }) =>
            {
                // HEAD already answered the questions the dialog asks. A probe that
                // could not run leaves range support unproven — one connection, and
                // a download that works — rather than throwing away a good answer.
                info.supports_range_requests = false;
            }
            Err(e) => return Err(e),
        }

        validators.apply_to(&mut info);
        Ok(finish(info, server_file_name))
    }
}

fn throw_if_challenged(response: &HttpResponse, info: &mut RemoteFileInfo, options: Option<&RequestOptions>) -> Result<()> {
    // The proxy's own challenge is not the file's: answering it with a sign-in
    // prompt would send the user's site password to the proxy.
    if response.status() == StatusCode::PROXY_AUTHENTICATION_REQUIRED {
        return Err(CoreError::ProxyAuthentication(CoreError::PROXY_AUTH_NEEDED.into()));
    }
    if response.status() != StatusCode::UNAUTHORIZED {
        return Ok(());
    }
    let supplied = options.is_some_and(RequestOptions::has_credentials);
    info.requires_authentication = true;
    Err(CoreError::auth(
        if supplied { "The server rejected that user name or password." } else { "This file needs a user name and password." },
        supplied,
    ))
}

/// Copies what a response knows into `info` and returns the file name the
/// server named explicitly, or `None` if it named none.
fn absorb(info: &mut RemoteFileInfo, response: &HttpResponse, validators: &mut ValidatorTally, from_ranged_probe: bool) -> Option<String> {
    // First, before anything derives a name from it: redirects are followed, so
    // the response's address is the one the bytes will really come from. A link
    // like ".../7z2301-x64.exe/download" only reveals its file name here.
    info.final_url = response.url().to_string();

    // Recorded as the claim it is. Only a 206 sets supports_range_requests.
    if response.header_values("accept-ranges").flat_map(|v| v.split(',')).any(|t| t.trim().eq_ignore_ascii_case("bytes")) {
        info.server_claims_range_support = true;
    }

    // A ranged probe's own Content-Length is 1 byte, which must never become the
    // file's size. Only Content-Range carries the total, and a probe the server
    // answered with a full 200 carries it in Content-Length as usual.
    if from_ranged_probe {
        match content_range_total(response) {
            Some(total) if total > 0 => info.content_length = total,
            _ => {
                if response.status() != StatusCode::PARTIAL_CONTENT
                    && let Some(full) = response.content_length().filter(|l| *l > 0)
                {
                    info.content_length = full;
                }
            }
        }
    } else if let Some(length) = response.content_length().filter(|l| *l > 0) {
        info.content_length = length;
    }

    if let Some(media_type) = response.header("content-type").map(|t| t.split(';').next().unwrap_or("").trim()).filter(|t| !t.is_empty()) {
        info.content_type = media_type.to_string();
    }

    validators.add(response);

    // filename* is the RFC 5987 form; it wins because it is the one that can
    // carry non-ASCII correctly.
    let disposition = response.headers().get("content-disposition").map(|v| header_text(v.as_bytes()))?;
    let name = content_disposition_file_name(&disposition)?;
    let name = name.trim().trim_matches('"').to_string();
    (!name.trim().is_empty()).then_some(name)
}

/// A header's text: UTF-8 when it is UTF-8 (what servers send in practice),
/// else Latin-1, byte for byte.
fn header_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// The total length in `Content-Range: bytes 0-0/130079008`.
pub fn content_range_total(response: &HttpResponse) -> Option<i64> {
    let value = response.header("content-range")?;
    let total = value.rsplit('/').next()?.trim();
    total.parse().ok()
}

/// The first byte in `Content-Range: bytes 40-99/100`.
pub fn content_range_from(response: &HttpResponse) -> Option<i64> {
    let value = response.header("content-range")?.trim();
    let spec = value.strip_prefix("bytes").unwrap_or(value).trim();
    let from = spec.split('-').next()?.trim();
    from.parse().ok()
}

/// The file name a `Content-Disposition` names: `filename*` (RFC 5987, decoded)
/// over `filename`.
pub fn content_disposition_file_name(value: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for part in split_params(value).into_iter().skip(1) {
        let Some((name, raw)) = part.split_once('=') else { continue };
        let name = name.trim().to_ascii_lowercase();
        let raw = raw.trim();
        match name.as_str() {
            "filename*" => extended = decode_ext_value(raw),
            "filename" => plain = Some(decode_encoded_word(&unquote(raw))),
            _ => {}
        }
    }
    extended.or(plain).filter(|n| !n.is_empty())
}

/// Splits on `;` outside quotes.
fn split_params(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in value.chars() {
        match c {
            _ if escaped => {
                current.push(c);
                escaped = false;
            }
            '\\' if quoted => {
                current.push(c);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            ';' if !quoted => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    parts.push(current);
    parts
}

fn unquote(raw: &str) -> String {
    let Some(inner) = raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) else { return raw.to_string() };
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `UTF-8''na%C3%AFve.txt` → `naïve.txt`.
fn decode_ext_value(raw: &str) -> Option<String> {
    let raw = unquote(raw);
    let mut pieces = raw.splitn(3, '\'');
    let charset = pieces.next()?.trim().to_ascii_lowercase();
    let _language = pieces.next()?;
    let encoded = pieces.next()?;
    let bytes: Vec<u8> = percent_decode(encoded.as_bytes()).collect();
    Some(match charset.as_str() {
        "iso-8859-1" | "latin1" => bytes.iter().map(|&b| b as char).collect(),
        _ => String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// A MIME encoded-word (`=?UTF-8?B?...?=`), which .NET's parser decoded in
/// `filename` too.
fn decode_encoded_word(value: &str) -> String {
    use base64::Engine;
    let trimmed = value.trim();
    let Some(inner) = trimmed.strip_prefix("=?").and_then(|r| r.strip_suffix("?=")) else { return value.to_string() };
    let mut parts = inner.splitn(3, '?');
    let (Some(_charset), Some(encoding), Some(text)) = (parts.next(), parts.next(), parts.next()) else { return value.to_string() };
    let bytes = match encoding.to_ascii_uppercase().as_str() {
        "B" => base64::engine::general_purpose::STANDARD.decode(text).ok(),
        "Q" => {
            let replaced = text.replace('_', " ");
            Some(percent_decode(replaced.replace('=', "%").as_bytes()).collect())
        }
        _ => None,
    };
    bytes.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_else(|| value.to_string())
}

/// An HTTP-date (`Tue, 01 Sep 2026 08:30:00 GMT`).
pub fn parse_http_date(value: &str) -> Option<DateTimeOffset> {
    let value = value.trim();
    if let Ok(date) = chrono::DateTime::parse_from_rfc2822(value) {
        return Some(DateTimeOffset(date));
    }
    // RFC 850 and asctime, the two obsolete forms HTTP still accepts.
    for format in ["%A, %d-%b-%y %H:%M:%S GMT", "%a %b %e %H:%M:%S %Y"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(value, format) {
            return Some(DateTimeOffset::from_utc(naive.and_utc()));
        }
    }
    None
}

/// An entity tag as `ETag` carries it — `"x"` or `W/"x"` — or nothing.
fn entity_tag(value: &str) -> Option<String> {
    let value = value.trim();
    let opaque = value.strip_prefix("W/").unwrap_or(value);
    (opaque.len() >= 2 && opaque.starts_with('"') && opaque.ends_with('"')).then(|| value.to_string())
}

/// Settles the file name: what the server said, else what the *final* URL says,
/// and in either case with an extension inferred from the content type if it
/// still has none.
fn finish(mut info: RemoteFileInfo, server_file_name: Option<String>) -> RemoteFileInfo {
    let name = match server_file_name {
        Some(name) => file_name_helper::sanitize_file_name(&name),
        None => file_name_helper::file_name_from_url(&info.final_url),
    };
    info.file_name = file_name_helper::ensure_extension(&name, Some(&info.content_type));
    info
}

/// Collects the validators every response of one probe carried, and keeps only
/// those they all agreed on.
///
/// A resume compares these with what the next probe says, and presents them as
/// `If-Range`. An ETag that differs between a HEAD and a GET made a second apart
/// — minted per request, or by two edges that each hash something of their own
/// — would fail both of those every single time, and read as a file that changed
/// while nobody was looking. Dropping it costs the protection on that one
/// server; keeping it costs the download.
#[derive(Default)]
struct ValidatorTally {
    etag: Option<String>,
    etag_disagreed: bool,
    last_modified: Option<DateTimeOffset>,
    last_modified_disagreed: bool,
}

impl ValidatorTally {
    fn add(&mut self, response: &HttpResponse) {
        // As sent: the W/ prefix is what says the tag may not be used in If-Range.
        if let Some(etag) = response.header("etag").and_then(entity_tag) {
            match &self.etag {
                None => self.etag = Some(etag),
                Some(known) if !file_version_check::same_opaque_tag(known, &etag) => self.etag_disagreed = true,
                Some(_) => {}
            }
        }
        if let Some(modified) = response.header("last-modified").and_then(parse_http_date) {
            match self.last_modified {
                None => self.last_modified = Some(modified),
                Some(known) if known.0 != modified.0 => self.last_modified_disagreed = true,
                Some(_) => {}
            }
        }
    }

    fn apply_to(self, info: &mut RemoteFileInfo) {
        info.etag = if self.etag_disagreed { None } else { self.etag };
        info.last_modified = if self.last_modified_disagreed { None } else { self.last_modified };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disposition_names_the_file_in_either_form() {
        assert_eq!(content_disposition_file_name("attachment; filename=\"setup.exe\"").as_deref(), Some("setup.exe"));
        assert_eq!(content_disposition_file_name("attachment; filename=plain.zip").as_deref(), Some("plain.zip"));
        assert_eq!(
            content_disposition_file_name("attachment; filename=\"fallback.txt\"; filename*=UTF-8''na%C3%AFve.txt").as_deref(),
            Some("naïve.txt")
        );
        assert_eq!(content_disposition_file_name("attachment; filename=\"a;b.txt\"").as_deref(), Some("a;b.txt"));
        assert_eq!(content_disposition_file_name("inline").as_deref(), None);
    }

    #[test]
    fn an_encoded_word_is_decoded() {
        assert_eq!(content_disposition_file_name("attachment; filename=\"=?UTF-8?B?w6l0w6kuZG9j?=\"").as_deref(), Some("été.doc"));
    }

    #[test]
    fn http_dates_are_read_in_all_three_forms() {
        let expected = "2026-09-01T08:30:00+00:00";
        assert_eq!(parse_http_date("Tue, 01 Sep 2026 08:30:00 GMT").unwrap().to_string(), expected);
        assert_eq!(parse_http_date("Tuesday, 01-Sep-26 08:30:00 GMT").unwrap().to_string(), expected);
        assert_eq!(parse_http_date("Tue Sep  1 08:30:00 2026").unwrap().to_string(), expected);
    }

    #[test]
    fn only_a_quoted_tag_is_a_validator() {
        assert_eq!(entity_tag("\"v1\"").as_deref(), Some("\"v1\""));
        assert_eq!(entity_tag("W/\"v1\"").as_deref(), Some("W/\"v1\""));
        assert_eq!(entity_tag("v1"), None);
    }
}
