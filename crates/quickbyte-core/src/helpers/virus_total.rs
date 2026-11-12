//! The rules of the VirusTotal check that need no network: what a vendor tally
//! means, which downloads count as private, and what may be sent.
//!
//! **What is sent, and when** (the contract Options > VirusTotal describes):
//!
//! - Nothing at all unless the user turned the check on and gave a key.
//! - A download's *address*, with any user name and password stripped out.
//! - A finished file's *SHA-256*. A hash reveals nothing about a file VirusTotal
//!   has not already seen, so it is sent for every download, private or not.
//! - The *file itself*, only when VirusTotal has never seen it and it is no
//!   larger than the upload limit (at most 25 MB).
//!
//! A *private* download — one that signed in, or carries the browser's cookies
//! or an `Authorization` header — is never sent by address or by upload unless
//! the user opted in to that separately. Its login, cookies and headers are
//! never sent anywhere but the server they belong to, opted in or not.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;

use crate::models::{DownloadItem, DownloadSettings, VirusTotalVerdict, header_get};

/// The page for a file's report on virustotal.com.
pub fn file_link(sha256: &str) -> String {
    format!("https://www.virustotal.com/gui/file/{}", sha256.to_ascii_lowercase())
}

/// The page for an address's report, from the id VirusTotal gave it.
pub fn url_link(object_id: &str) -> String {
    format!("https://www.virustotal.com/gui/url/{object_id}")
}

/// What the vendors said, as VirusTotal counts it (`last_analysis_stats` / `stats`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ScanStats {
    pub malicious: i32,
    pub suspicious: i32,
    pub harmless: i32,
    pub undetected: i32,
    pub timeout: i32,
}

impl ScanStats {
    /// Vendors that gave an answer. One that timed out did not.
    pub fn engines(&self) -> i32 {
        self.malicious + self.suspicious + self.harmless + self.undetected
    }
}

/// A finished lookup or analysis: the tally, and the id VirusTotal files it
/// under (for the link to its page), when it said.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanResult {
    pub stats: ScanStats,
    pub object_id: Option<String>,
}

/// `threshold` malicious votes make a file malicious. Fewer — but at least one
/// malicious or suspicious vote — make it suspicious: one vendor's alarm is
/// worth hearing about, and is often wrong. A tally nobody voted in says
/// nothing either way.
pub fn classify(stats: &ScanStats, threshold: i32) -> VirusTotalVerdict {
    if stats.malicious >= threshold.max(1) {
        VirusTotalVerdict::Malicious
    } else if stats.malicious > 0 || stats.suspicious > 0 {
        VirusTotalVerdict::Suspicious
    } else if stats.engines() > 0 {
        VirusTotalVerdict::Clean
    } else {
        VirusTotalVerdict::Unknown
    }
}

/// VirusTotal's identifier for an address: its unpadded URL-safe base64.
pub fn url_id(url: &str) -> String {
    URL_SAFE_NO_PAD.encode(url.as_bytes())
}

/// The address as it may be sent: a web or FTP address, without a user name,
/// password or fragment. `None` for anything else.
pub fn shareable_url(url: &str) -> Option<String> {
    let mut parsed = url::Url::parse(url.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https" | "ftp") || parsed.host_str().is_none() {
        return None;
    }
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_fragment(None);
    Some(parsed.into())
}

/// Whether the download came from somewhere only this user can reach: it signs
/// in, carries cookies or an `Authorization` header, or has a login in its address.
pub fn is_private(item: &DownloadItem) -> bool {
    let signs_in = item.credentials.as_ref().is_some_and(|c| !c.is_empty());
    let carries = |name: &str| header_get(&item.headers, name).is_some_and(|v| !v.trim().is_empty());
    let login_in_address = url::Url::parse(item.url.trim()).is_ok_and(|u| !u.username().is_empty() || u.password().is_some());
    signs_in || carries("Cookie") || carries("Authorization") || login_in_address
}

/// Whether the download may be sent by address or by upload.
pub fn may_share(item: &DownloadItem, settings: &DownloadSettings) -> bool {
    settings.virus_total_upload_private_files || !is_private(item)
}

/// A VirusTotal API key: 64 hexadecimal digits. Used to recognise a key stored
/// (or typed through `qb config`) in the clear.
pub fn looks_like_api_key(text: &str) -> bool {
    let text = text.trim();
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DownloadCredentials, Headers};

    fn stats(malicious: i32, suspicious: i32, harmless: i32, undetected: i32) -> ScanStats {
        ScanStats { malicious, suspicious, harmless, undetected, timeout: 0 }
    }

    #[test]
    fn enough_malicious_votes_outweigh_any_number_of_suspicious_ones() {
        assert_eq!(classify(&stats(3, 9, 50, 10), 3), VirusTotalVerdict::Malicious);
        assert_eq!(classify(&stats(1, 9, 50, 10), 1), VirusTotalVerdict::Malicious);
        assert_eq!(classify(&stats(0, 1, 50, 10), 3), VirusTotalVerdict::Suspicious);
        assert_eq!(classify(&stats(0, 0, 50, 10), 3), VirusTotalVerdict::Clean);
        assert_eq!(classify(&stats(0, 0, 0, 1), 3), VirusTotalVerdict::Clean);
    }

    #[test]
    fn malicious_votes_below_the_threshold_make_it_suspicious_not_clean() {
        assert_eq!(classify(&stats(2, 0, 60, 8), 3), VirusTotalVerdict::Suspicious);
        assert_eq!(classify(&stats(1, 0, 60, 8), 0), VirusTotalVerdict::Malicious, "a threshold below 1 is 1");
    }

    #[test]
    fn a_tally_nobody_voted_in_says_nothing() {
        let timed_out = ScanStats { timeout: 70, ..Default::default() };
        assert_eq!(classify(&timed_out, 3), VirusTotalVerdict::Unknown);
        assert_eq!(timed_out.engines(), 0);
    }

    #[test]
    fn the_tally_reads_from_virus_totals_own_shape() {
        let parsed: ScanStats = serde_json::from_str(
            r#"{"harmless":61,"malicious":3,"suspicious":1,"undetected":9,"timeout":0,"type-unsupported":4}"#,
        )
        .unwrap();
        assert_eq!(parsed, stats(3, 1, 61, 9));
        assert_eq!(parsed.engines(), 74);
    }

    #[test]
    fn an_address_id_is_unpadded_url_safe_base64() {
        // The example from VirusTotal's own documentation.
        assert_eq!(url_id("http://www.somedomain.com/this/is/my/url"), "aHR0cDovL3d3dy5zb21lZG9tYWluLmNvbS90aGlzL2lzL215L3VybA");
        assert!(!url_id("https://a.example/?q=1>").contains(['=', '+', '/']));
    }

    #[test]
    fn a_login_in_the_address_is_never_shared() {
        assert_eq!(
            shareable_url("https://me:secret@files.example.com/a.zip?x=1#part").as_deref(),
            Some("https://files.example.com/a.zip?x=1")
        );
        assert_eq!(shareable_url("ftp://anonymous@ftp.example.org/pub/x.iso").as_deref(), Some("ftp://ftp.example.org/pub/x.iso"));
        assert_eq!(shareable_url("file:///C:/secret.txt"), None);
        assert_eq!(shareable_url("not a url"), None);
    }

    #[test]
    fn a_download_that_signs_in_or_carries_cookies_is_private() {
        let public = DownloadItem { url: "https://example.com/a.zip".into(), ..Default::default() };
        assert!(!is_private(&public));

        let signed_in = DownloadItem { credentials: Some(DownloadCredentials::new("me", "pw")), ..public.clone() };
        assert!(is_private(&signed_in));

        let mut headers = Headers::new();
        headers.insert("cookie".into(), "session=1".into());
        assert!(is_private(&DownloadItem { headers, ..public.clone() }));

        let mut headers = Headers::new();
        headers.insert("Referer".into(), "https://example.com/".into());
        assert!(!is_private(&DownloadItem { headers, ..public.clone() }), "a referrer is not a secret");

        assert!(is_private(&DownloadItem { url: "https://me:pw@example.com/a.zip".into(), ..public }));
    }

    #[test]
    fn a_private_download_is_shared_only_when_the_user_opted_in() {
        let item = DownloadItem { credentials: Some(DownloadCredentials::new("me", "pw")), ..Default::default() };
        let mut settings = DownloadSettings::default();
        assert!(!may_share(&item, &settings));
        settings.virus_total_upload_private_files = true;
        assert!(may_share(&item, &settings));
        assert!(may_share(&DownloadItem::default(), &DownloadSettings::default()));
    }

    #[test]
    fn an_api_key_is_sixty_four_hex_digits() {
        assert!(looks_like_api_key(&"0123456789abcdefABCDEF0123456789".repeat(2)));
        assert!(!looks_like_api_key("abc"));
        assert!(!looks_like_api_key(&"g".repeat(64)));
    }
}
