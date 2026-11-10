//! Port of `MarkOfTheWeb.cs`: tags a finished download as having come from the
//! internet — the `Zone.Identifier` alternate data stream Windows calls the Mark
//! of the Web.
//!
//! Every browser writes it, and a surprising amount of Windows security hangs off
//! it: SmartScreen checks an executable's reputation before running it, Office
//! opens a document in Protected View, PowerShell refuses an unsigned script, and
//! "Unblock" in a file's Properties exists only because of it. The browser
//! extension hands downloads over from Chrome, which means that before this
//! existed, clicking a link in Chrome produced a file with fewer protections than
//! letting Chrome download it would have. A download manager that launders the
//! mark away is a way around every one of those checks.
//!
//! What goes in the stream follows what Chromium writes: the zone, the referring
//! page and the file's address, with any user name and password removed from
//! both. It is text that other programs parse as an INI section, and the referrer
//! is a header the browser extension was sent — so each URL is re-built from a
//! parsed URL, restricted to the web and FTP schemes, and refused outright if
//! anything in it could start a new line.
//!
//! Best-effort, like the rest of the engine's file handling: a FAT32 or exFAT
//! drive and many network shares have no alternate streams at all, and a
//! download that finished is not made to fail over a tag.

use crate::platform::alternate_stream;

pub const STREAM_NAME: &str = "Zone.Identifier";

/// URLZONE_INTERNET. Deliberately fixed rather than mapped: a lower zone would
/// mean fewer warnings, and being wrong in that direction is the expensive one.
pub const INTERNET_ZONE: i32 = 3;

/// Tags `file_path`. Returns whether the stream was written.
pub fn apply(file_path: &str, host_url: Option<&str>, referrer_url: Option<&str>) -> bool {
    // Never create the file: a stream path opened for writing on a file that is
    // not there makes an empty one to hang the stream on.
    if file_path.trim().is_empty() || !std::path::Path::new(file_path).is_file() {
        return false;
    }
    // Best-effort — no alternate streams on this volume.
    alternate_stream::write(file_path, STREAM_NAME, build(host_url, referrer_url).as_bytes()).is_ok()
}

/// The stream's text: CRLF-separated, exactly the shape Windows and the browsers write.
pub fn build(host_url: Option<&str>, referrer_url: Option<&str>) -> String {
    let mut text = String::from("[ZoneTransfer]\r\n");
    text.push_str(&format!("ZoneId={INTERNET_ZONE}\r\n"));
    if let Some(referrer) = sanitize(referrer_url) {
        text.push_str(&format!("ReferrerUrl={referrer}\r\n"));
    }
    if let Some(host) = sanitize(host_url) {
        text.push_str(&format!("HostUrl={host}\r\n"));
    }
    text
}

/// A URL fit to write into the stream, or `None`. The user name and password are
/// removed — the file may be copied anywhere, and the stream goes with it — and
/// anything that is not an absolute http(s) or ftp(s) address is dropped rather
/// than guessed at.
pub fn sanitize(url: Option<&str>) -> Option<String> {
    let url = url?.trim();
    if url.is_empty() {
        return None;
    }
    let mut uri = url::Url::parse(url).ok()?;
    if !matches!(uri.scheme(), "http" | "https" | "ftp" | "ftps") {
        return None;
    }
    // Neither can fail on a URL that has a host, which these schemes require.
    let _ = uri.set_username("");
    let _ = uri.set_password(None);
    let clean = uri.as_str();

    // Belt and braces: an escaped URI cannot contain a raw control character,
    // but this text is parsed line by line by other programs, and a line break
    // smuggled into it would let a referrer write its own ZoneId.
    (!clean.chars().any(char::is_control)).then(|| clean.to_string())
}

#[cfg(test)]
mod tests {
    //! These pin what is written, and that nothing in a captured header can
    //! rewrite it: the referrer is whatever the extension was sent, and the
    //! stream is parsed line by line.

    use super::*;

    #[test]
    fn the_stream_says_internet_and_names_the_page_and_the_file() {
        let text = build(Some("https://cdn.example.com/setup.exe"), Some("https://example.com/downloads"));
        assert_eq!(
            text,
            "[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl=https://example.com/downloads\r\nHostUrl=https://cdn.example.com/setup.exe\r\n"
        );
    }

    #[test]
    fn a_login_in_either_address_is_never_written_down() {
        // The stream travels with the file wherever it is copied.
        let text = build(Some("ftp://alice:hunter2@files.example.com/disk.iso"), Some("https://bob:secret@example.com/"));
        assert!(!text.contains("hunter2"));
        assert!(!text.contains("secret"));
        assert!(text.contains("HostUrl=ftp://files.example.com/disk.iso"));
    }

    #[test]
    fn an_address_that_is_not_a_web_or_ftp_one_is_left_out() {
        for referrer in [
            Some("javascript:alert(1)"),
            Some("file:///C:/Windows/System32/calc.exe"),
            Some("chrome-extension://abcdef/popup.html"),
            Some("not a url"),
            Some(""),
            None,
        ] {
            let text = build(Some("https://example.com/f.exe"), referrer);
            assert!(!text.contains("ReferrerUrl"), "{referrer:?}");
            assert!(text.contains("ZoneId=3"));
        }
    }

    #[test]
    fn a_referrer_cannot_smuggle_in_a_zone_of_its_own() {
        // A captured header carrying a line break, trying to lower the zone the
        // file is judged by.
        let text = build(Some("https://example.com/f.exe"), Some("https://example.com/\r\nZoneId=0\r\n"));
        let lines: Vec<&str> = text.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.iter().filter(|l| l.starts_with("ZoneId=")).count(), 1);
        assert!(lines.contains(&"ZoneId=3"));
    }

    #[test]
    fn unicode_in_an_address_is_escaped_rather_than_written_raw() {
        assert_eq!(
            sanitize(Some("https://example.com/файл.exe")).as_deref(),
            Some("https://example.com/%D1%84%D0%B0%D0%B9%D0%BB.exe")
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_finished_file_gets_the_stream_and_keeps_its_contents() {
        let folder = crate::helpers::disk_space::scratch::TempFolder::new();
        let file = folder.write_file("setup.exe", &[0x4D, 0x5A, 0x90]);

        assert!(apply(&file, Some("https://example.com/setup.exe"), None));
        let stream = std::fs::read_to_string(format!("{file}:{STREAM_NAME}")).unwrap();
        assert!(stream.contains("ZoneId=3"));
        assert_eq!(std::fs::read(&file).unwrap(), [0x4D, 0x5A, 0x90]);
    }

    #[test]
    fn a_file_that_is_not_there_is_not_created_to_hang_the_stream_on() {
        let folder = crate::helpers::disk_space::scratch::TempFolder::new();
        let file = folder.file("never-merged.exe");
        assert!(!apply(&file, Some("https://example.com/f.exe"), None));
        assert!(!std::path::Path::new(&file).exists());
    }
}
