//! Port of `src/QuickByte.Core/Helpers/HttpRequestDecorator.cs`.
//!
//! Stamps a [`RequestOptions`] onto an outgoing request's headers. One place for
//! it because the metadata probe and every download connection have to present
//! *identical* credentials and headers — a probe that authenticates and
//! connections that don't would resolve a real file size and then fetch eight
//! copies of a login page.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{AUTHORIZATION, CACHE_CONTROL, HeaderMap, HeaderName, HeaderValue, PRAGMA, USER_AGENT};

use crate::helpers::app_user_agent;
use crate::models::{RemoteFileVersion, RequestOptions};
use crate::serde_compat::DateTimeOffset;

/// Applies `options` with the User-Agent in force right now.
pub fn apply(headers: &mut HeaderMap, options: Option<&RequestOptions>) {
    apply_with(headers, options, &app_user_agent::current());
}

/// `user_agent` is a seam, and the only reason this function exists:
/// [`app_user_agent::current`] is process-wide live state, and the tests run in
/// parallel, so a test that configured it would be read by whatever else was
/// mid-assertion.
pub fn apply_with(headers: &mut HeaderMap, options: Option<&RequestOptions>, user_agent: &str) {
    // Before the early return below, and deliberately: the User-Agent is an
    // app-wide setting rather than part of a request's options, and a request
    // made with no options at all still has to carry one.
    apply_user_agent(headers, options, user_agent);

    let Some(options) = options else { return };

    if let Some(credentials) = options.credentials.as_ref().filter(|c| !c.is_empty()) {
        // Sent up front rather than after a 401. Every connection here shares one
        // client on purpose, and presenting Basic pre-emptively also saves a round
        // trip on every one of up to 32 connections. Servers that want Digest,
        // NTLM or Negotiate are answered by `authenticated_http` instead.
        let pair = format!("{}:{}", credentials.user_name, credentials.password);
        if let Ok(value) = HeaderValue::from_str(&format!("Basic {}", STANDARD.encode(pair.as_bytes()))) {
            headers.insert(AUTHORIZATION, value);
        }
    }

    if options.bypass_cache {
        // Both forms, because the intermediary that has to hear it is not
        // necessarily a modern one: Pragma is the HTTP/1.0 spelling and is still
        // the only thing some proxies act on. See `RequestOptions::bypass_cache`
        // for what this works around.
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        headers.insert(PRAGMA, HeaderValue::from_static("no-cache"));
    }

    let Some(captured) = options.headers.as_ref() else { return };
    for (name, value) in captured {
        // Without validation beyond what a header can physically carry: these come
        // from the browser extension, and a Cookie or Referer the browser itself
        // was happy to send must not be rejected here. Range is dropped because
        // this request owns it — a stale captured Range would silently truncate
        // the segment.
        if name.eq_ignore_ascii_case("Range") {
            continue;
        }

        // Conditionals likewise belong to the request that makes them. A captured
        // If-None-Match turns the probe into a 304 with no size, and a stale
        // If-Range would decide for the server which version counts.
        if is_conditional(name) {
            continue;
        }
        if options.has_credentials() && name.eq_ignore_ascii_case("Authorization") {
            continue;
        }

        // Same rule for the pair just written above: a captured
        // "Cache-Control: max-age=0" beside our own no-cache is a header with two
        // directives, and which one an intermediary honours is its business.
        if options.bypass_cache && (name.eq_ignore_ascii_case("Cache-Control") || name.eq_ignore_ascii_case("Pragma")) {
            continue;
        }

        // A request has no content, so a content header on it is not one
        // (HttpClient refused them on request headers, silently).
        if is_content_header(name) {
            continue;
        }

        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.trim().as_bytes()), HeaderValue::from_bytes(value.as_bytes())) {
            headers.append(name, value);
        }
    }
}

/// Writes the User-Agent, unless something more specific owns it.
///
/// A captured one wins over the user's setting, and the setting over
/// QuickByte's own — most specific first. The browser extension's is the most
/// specific there is: it is the string the browser paired with the `Cookie`
/// captured alongside it, and a server that pins a session to the agent it was
/// issued to answers a mismatched pair with the sign-in page the capture existed
/// to avoid. It is written by the header loop, so all this does is stay out of
/// its way — appending ours too would put two agents on one request.
fn apply_user_agent(headers: &mut HeaderMap, options: Option<&RequestOptions>, user_agent: &str) {
    if user_agent.is_empty() {
        return;
    }
    // Already set by the caller, or by an earlier apply on the same request.
    if headers.contains_key(USER_AGENT) {
        return;
    }
    if options.and_then(|o| o.headers.as_ref()).is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case("User-Agent"))) {
        return;
    }
    // As typed: the value's job is to look like whatever a server will accept.
    // `app_user_agent::normalize` has already refused the only thing that is
    // actually unsafe here, a control character.
    if let Ok(value) = HeaderValue::from_bytes(user_agent.as_bytes()) {
        headers.insert(USER_AGENT, value);
    }
}

/// An `If-Range` condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RangeCondition {
    /// A strong entity tag, quotes included.
    EntityTag(String),
    Date(DateTimeOffset),
}

impl RangeCondition {
    /// The header value: the tag as it is, or the date as an HTTP-date.
    pub fn header_value(&self) -> String {
        match self {
            Self::EntityTag(tag) => tag.clone(),
            Self::Date(date) => http_date(date),
        }
    }
}

/// `Tue, 01 Sep 2026 00:00:00 GMT`.
pub fn http_date(date: &DateTimeOffset) -> String {
    date.to_utc().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// The `If-Range` condition for a download's version: its strong ETag, else its
/// Last-Modified date, else nothing.
///
/// A weak ETag yields nothing rather than falling back to the date. RFC 9110
/// forbids a weak tag in `If-Range` — a server must treat it as a mismatch and
/// send the whole file every time — and it forbids the date when the client
/// holds an entity tag for the representation at all.
pub fn if_range_for(version: Option<&RemoteFileVersion>) -> Option<RangeCondition> {
    let version = version?;
    if let Some(tag) = version.etag.as_deref() {
        let quoted = tag.len() >= 2 && tag.starts_with('"') && tag.ends_with('"') && !tag[1..tag.len() - 1].contains('"');
        return (version.has_strong_etag() && quoted).then(|| RangeCondition::EntityTag(tag.to_string()));
    }
    version.last_modified.map(RangeCondition::Date)
}

fn is_conditional(name: &str) -> bool {
    ["If-Range", "If-Match", "If-None-Match", "If-Modified-Since", "If-Unmodified-Since"]
        .iter()
        .any(|c| c.eq_ignore_ascii_case(name))
}

fn is_content_header(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    lower.starts_with("content-") || lower == "transfer-encoding" || lower == "expires" || lower == "last-modified" || lower == "allow"
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, TimeZone};

    use super::*;
    use crate::models::{DownloadCredentials, Headers};

    fn headers_of(pairs: &[(&str, &str)]) -> Headers {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn decorate(options: Option<&RequestOptions>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        apply_with(&mut headers, options, "QuickByte-test");
        headers
    }

    fn values(headers: &HeaderMap, name: &str) -> Vec<String> {
        headers.get_all(name).iter().map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).collect()
    }

    fn alice() -> Option<DownloadCredentials> {
        Some(DownloadCredentials::new("alice", "hunter2"))
    }

    fn basic(pair: &str) -> String {
        format!("Basic {}", STANDARD.encode(pair))
    }

    #[test]
    fn sends_basic_credentials_up_front() {
        let headers = decorate(Some(&RequestOptions { credentials: alice(), ..Default::default() }));
        assert_eq!(values(&headers, "authorization"), vec![basic("alice:hunter2")]);
    }

    #[test]
    fn does_nothing_without_options() {
        assert!(!decorate(None).contains_key(AUTHORIZATION));
    }

    #[test]
    fn ignores_empty_credentials() {
        let headers = decorate(Some(&RequestOptions { credentials: Some(DownloadCredentials::default()), ..Default::default() }));
        assert!(!headers.contains_key(AUTHORIZATION));
    }

    #[test]
    fn carries_the_browser_headers_that_make_a_link_resolve() {
        let options = RequestOptions {
            headers: Some(headers_of(&[
                ("Cookie", "session=abc; theme=dark"),
                ("Referer", "https://example.com/downloads"),
                ("User-Agent", "Mozilla/5.0 (a browser)"),
            ])),
            ..Default::default()
        };
        let headers = decorate(Some(&options));
        assert_eq!(values(&headers, "cookie"), vec!["session=abc; theme=dark"]);
        assert_eq!(values(&headers, "referer"), vec!["https://example.com/downloads"]);
    }

    #[test]
    fn never_lets_a_captured_range_header_through() {
        let mut headers = HeaderMap::new();
        headers.insert("range", HeaderValue::from_static("bytes=100-200"));
        let options = RequestOptions { headers: Some(headers_of(&[("Range", "bytes=0-5"), ("range", "bytes=0-5")])), ..Default::default() };
        apply_with(&mut headers, Some(&options), "x");
        assert_eq!(values(&headers, "range"), vec!["bytes=100-200"]);
    }

    #[test]
    fn drops_a_captured_authorization_when_it_has_credentials_of_its_own() {
        let options = RequestOptions { credentials: alice(), headers: Some(headers_of(&[("Authorization", "Bearer stale-token")])), ..Default::default() };
        assert_eq!(values(&decorate(Some(&options)), "authorization"), vec![basic("alice:hunter2")]);
    }

    #[test]
    fn keeps_a_captured_authorization_when_there_is_nothing_to_replace_it_with() {
        let options = RequestOptions { headers: Some(headers_of(&[("Authorization", "Bearer captured")])), ..Default::default() };
        assert_eq!(values(&decorate(Some(&options)), "authorization"), vec!["Bearer captured"]);
    }

    #[test]
    fn does_not_validate_what_the_browser_was_happy_to_send() {
        let options = RequestOptions { headers: Some(headers_of(&[("Cookie", "weird=\"quoted, value\"; a=b")])), ..Default::default() };
        assert!(decorate(Some(&options)).contains_key("cookie"));
    }

    #[test]
    fn asks_past_the_cache_in_both_spellings_when_told_to() {
        let headers = decorate(Some(&RequestOptions { bypass_cache: true, ..Default::default() }));
        assert_eq!(values(&headers, "cache-control"), vec!["no-cache"]);
        assert_eq!(values(&headers, "pragma"), vec!["no-cache"]);
    }

    #[test]
    fn leaves_the_cache_alone_by_default() {
        let headers = decorate(Some(&RequestOptions::default()));
        assert!(!headers.contains_key(CACHE_CONTROL));
        assert!(!headers.contains_key(PRAGMA));
    }

    #[test]
    fn a_captured_cache_header_does_not_get_to_argue_with_the_bypass() {
        let options = RequestOptions {
            bypass_cache: true,
            headers: Some(headers_of(&[("Cache-Control", "max-age=0"), ("Pragma", "akamai-x-cache-on"), ("Cookie", "session=abc")])),
            ..Default::default()
        };
        let headers = decorate(Some(&options));
        assert_eq!(values(&headers, "cache-control"), vec!["no-cache"]);
        assert_eq!(values(&headers, "pragma"), vec!["no-cache"]);
        assert!(headers.contains_key("cookie"));
    }

    #[test]
    fn a_captured_conditional_header_is_never_sent() {
        for name in ["If-Range", "If-None-Match", "If-Modified-Since", "If-Match", "If-Unmodified-Since"] {
            let options = RequestOptions { headers: Some(headers_of(&[(name, "\"stale\""), ("Cookie", "session=abc")])), ..Default::default() };
            let headers = decorate(Some(&options));
            assert!(!headers.contains_key(name), "{name}");
            assert!(headers.contains_key("cookie"));
        }
    }

    #[test]
    fn a_captured_cache_header_is_still_sent_when_nothing_is_being_bypassed() {
        let options = RequestOptions { headers: Some(headers_of(&[("Cache-Control", "max-age=0")])), ..Default::default() };
        assert_eq!(values(&decorate(Some(&options)), "cache-control"), vec!["max-age=0"]);
    }

    fn date() -> DateTimeOffset {
        DateTimeOffset(FixedOffset::east_opt(0).unwrap().with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap())
    }

    #[test]
    fn if_range_prefers_a_strong_etag() {
        let version = RemoteFileVersion::new(Some("\"v2\"".into()), Some(date()), None);
        assert_eq!(if_range_for(Some(&version)), Some(RangeCondition::EntityTag("\"v2\"".into())));
    }

    #[test]
    fn if_range_falls_back_to_the_date_when_there_is_no_etag() {
        let version = RemoteFileVersion::new(None, Some(date()), None);
        let condition = if_range_for(Some(&version)).unwrap();
        assert_eq!(condition, RangeCondition::Date(date()));
        assert_eq!(condition.header_value(), "Tue, 01 Sep 2026 00:00:00 GMT");
    }

    #[test]
    fn if_range_never_sends_a_weak_etag_or_the_date_beside_it() {
        let version = RemoteFileVersion::new(Some("W/\"v2\"".into()), Some(date()), None);
        assert_eq!(if_range_for(Some(&version)), None);
    }

    #[test]
    fn if_range_asks_nothing_without_a_validator() {
        assert_eq!(if_range_for(None), None);
        assert_eq!(if_range_for(Some(&RemoteFileVersion::new(None, None, Some("example.com".into())))), None);
    }

    fn one_user_agent(headers: &HeaderMap) -> String {
        let all = values(headers, "user-agent");
        assert_eq!(all.len(), 1, "{all:?}");
        all[0].clone()
    }

    #[test]
    fn presents_the_configured_user_agent() {
        let mut headers = HeaderMap::new();
        apply_with(&mut headers, Some(&RequestOptions::none()), "Wget/1.21.4");
        assert_eq!(one_user_agent(&headers), "Wget/1.21.4");
    }

    #[test]
    fn presents_a_user_agent_even_with_no_options_at_all() {
        let mut headers = HeaderMap::new();
        apply_with(&mut headers, None, "Wget/1.21.4");
        assert_eq!(one_user_agent(&headers), "Wget/1.21.4");
    }

    #[test]
    fn a_captured_user_agent_beats_the_configured_one() {
        let options = RequestOptions {
            headers: Some(headers_of(&[("Cookie", "session=abc"), ("User-Agent", "Mozilla/5.0 (a browser)")])),
            ..Default::default()
        };
        let mut headers = HeaderMap::new();
        apply_with(&mut headers, Some(&options), "Wget/1.21.4");
        assert_eq!(one_user_agent(&headers), "Mozilla/5.0 (a browser)");
    }

    #[test]
    fn a_captured_user_agent_wins_whatever_its_header_is_spelled_like() {
        let options = RequestOptions { headers: Some(headers_of(&[("user-agent", "Mozilla/5.0 (a browser)")])), ..Default::default() };
        let mut headers = HeaderMap::new();
        apply_with(&mut headers, Some(&options), "Wget/1.21.4");
        assert_eq!(one_user_agent(&headers), "Mozilla/5.0 (a browser)");
    }

    #[test]
    fn leaves_a_user_agent_the_caller_already_set() {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("Something/1.0"));
        apply_with(&mut headers, Some(&RequestOptions::none()), "Wget/1.21.4");
        assert_eq!(one_user_agent(&headers), "Something/1.0");
    }

    #[test]
    fn a_user_agent_a_strict_parser_would_refuse_is_still_sent() {
        let mut headers = HeaderMap::new();
        apply_with(&mut headers, Some(&RequestOptions::none()), "Mozilla/5.0 (Windows");
        assert_eq!(one_user_agent(&headers), "Mozilla/5.0 (Windows");
    }

    #[test]
    fn the_user_agent_is_sent_alongside_everything_else_a_request_carries() {
        let options = RequestOptions {
            credentials: alice(),
            bypass_cache: true,
            headers: Some(headers_of(&[("Referer", "https://example.com/d")])),
            ..Default::default()
        };
        let mut headers = HeaderMap::new();
        apply_with(&mut headers, Some(&options), "Wget/1.21.4");
        assert_eq!(one_user_agent(&headers), "Wget/1.21.4");
        assert!(values(&headers, "authorization")[0].starts_with("Basic "));
        assert_eq!(values(&headers, "cache-control"), vec!["no-cache"]);
        assert_eq!(values(&headers, "referer"), vec!["https://example.com/d"]);
    }
}
