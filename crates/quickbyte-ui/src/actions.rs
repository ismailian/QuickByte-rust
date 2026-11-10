//! The Core helpers the windows use, in one place.
//!
//! Most of these now *are* Core (`helpers::download_actions`,
//! `connection_actions`, `download_search`, `completion_plan`, `file_checksum`,
//! `clipboard_links`, `url_credentials`, `user_path`, `batch_links`,
//! `wake_times`, `settings_keys`): the functions below are re-exports or
//! one-line wrappers, and the few that take a [`Shell`] only add demo mode's
//! "nothing on disk is real" on top of the Core rule.
//!
//! The rest are faithful local copies of C# helpers that are still being ported
//! into Core by others — `FileNameHelper`, `AppUserAgent`, `StreamDetector`,
//! `AddressRefresh` (with the `FileVersionCheck` it builds on). Each is marked
//! `// INTEGRATION:` with the Core function that replaces it, so the switch is
//! one line per function and no window changes.

use quickbyte_core::helpers::{
    clipboard_links, connection_actions, download_actions, download_search, file_checksum,
};
use quickbyte_core::models::{
    ConnectionInfo, DownloadItem,
};

use crate::shell::Shell;

pub use quickbyte_core::helpers::completion_plan::CompletionPlan;
pub use quickbyte_core::helpers::download_actions::{
    can_pause, can_refresh_address, can_resume, can_retry, can_stop, is_active,
};

// ---------------------------------------------------------- DownloadActions --

/// Core's rule, except that in demo mode no file is on disk: a completed demo
/// download still offers Open (which then does nothing).
pub fn can_open_file(item: &DownloadItem, shell: &Shell) -> bool {
    if shell.is_demo() {
        return item.status == quickbyte_core::enums::DownloadStatus::Completed;
    }
    download_actions::can_open_file(item)
}

pub fn can_open_folder(item: &DownloadItem, shell: &Shell) -> bool {
    if shell.is_demo() {
        return !item.save_folder.trim().is_empty();
    }
    download_actions::can_open_folder(item)
}

pub fn can_verify_checksum(item: &DownloadItem, shell: &Shell) -> bool {
    can_open_file(item, shell)
}

// -------------------------------------------------------- ConnectionActions --

pub fn connection_can_pause(c: &ConnectionInfo) -> bool {
    connection_actions::can_pause(c)
}

pub fn connection_can_resume(c: &ConnectionInfo) -> bool {
    connection_actions::can_resume(c)
}

pub fn connection_can_restart(c: &ConnectionInfo) -> bool {
    connection_actions::can_restart(c)
}

/// Only a transferring download has connections to command.
pub fn accepts_connection_commands(item: &DownloadItem) -> bool {
    connection_actions::accepts_commands(item)
}

// ------------------------------------------------------------ DownloadSearch --

pub fn search_terms(query: &str) -> Vec<String> {
    download_search::terms(Some(query))
}

pub fn search_matches(item: &DownloadItem, terms: &[String]) -> bool {
    download_search::matches_terms(item, terms)
}

// ------------------------------------------------------------- FileChecksum --

/// The display name of a stored checksum (`sha256:…` → `SHA-256`).
pub fn checksum_name(stored: Option<&str>) -> Option<&'static str> {
    file_checksum::from_stored(stored).map(|c| file_checksum::name(c.algorithm))
}

// ----------------------------------------------------------- ClipboardLinks --

/// The one download link a clipboard holds, if it holds exactly one link and
/// nothing else, to a file type on the list.
pub fn clipboard_download_link(text: &str, file_types: &str) -> Option<String> {
    let types = clipboard_links::parse_file_types(Some(file_types));
    clipboard_links::find_download_link(Some(text), &types)
}

/// An absolute http, https, ftp or ftps address (`Uri.TryCreate` + scheme check).
pub fn is_supported_url(text: &str) -> bool {
    url::Url::parse(text.trim()).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https" | "ftp" | "ftps")
            && u.host_str().is_some_and(|h| !h.is_empty())
    })
}

// ------------------------------------------------- Core helpers (merged M2) --

pub use quickbyte_core::helpers::app_user_agent::{DEFAULT as APP_USER_AGENT_DEFAULT, MAX_LENGTH as APP_USER_AGENT_MAX_LENGTH};
pub use quickbyte_core::helpers::file_name_helper::{file_name_from_url, sanitize_file_name};
pub use quickbyte_core::streaming::stream_detector::{is_stream, suggest_file_name as suggest_stream_file_name};

/// The value trimmed, or `None` when it cannot be sent as a header value.
pub fn user_agent_normalize(value: Option<&str>) -> Option<String> {
    quickbyte_core::helpers::app_user_agent::normalize(value)
}

/// Whether Options can accept it. Blank counts: it asks for QuickByte's own.
pub fn user_agent_is_acceptable(value: &str) -> bool {
    quickbyte_core::helpers::app_user_agent::is_acceptable(Some(value))
}

/// `Path.GetExtension`: the last `.` and what follows it, or empty.
pub fn extension_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 || name.len() > 1 => &name[i..],
        _ => "",
    }
}

// ---------------------------------------------------------- AddressRefresh --

pub use quickbyte_core::helpers::address_refresh::{
    AddressVerdict, has_progress, judge as judge_address, matches_capture,
};

/// `Path.GetFileNameWithoutExtension`.
pub fn stem_of(name: &str) -> &str {
    let ext = extension_of(name);
    &name[..name.len() - ext.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickbyte_core::models::{CapturedDownload, RemoteFileInfo, RemoteFileVersion};
    use quickbyte_core::enums::DownloadStatus;

    fn item(name: &str, url: &str) -> DownloadItem {
        DownloadItem {
            file_name: name.into(),
            url: url.into(),
            ..Default::default()
        }
    }

    #[test]
    fn search_goes_through_core() {
        let i = item(
            "ubuntu-26.04-desktop.iso",
            "https://releases.example.org/ubuntu/26.04/x.iso?token=abc123",
        );
        assert!(search_matches(&i, &search_terms("UBUNTU desktop")));
        assert!(!search_matches(&i, &search_terms("abc123")));
    }

    #[test]
    fn checksum_names_and_file_names() {
        let hex = "a".repeat(64);
        assert_eq!(
            checksum_name(Some(&format!("sha256:{hex}"))),
            Some("SHA-256")
        );
        assert_eq!(checksum_name(None), None);
        assert_eq!(
            file_name_from_url("https://h.example/a/b/setup%20x.exe"),
            "setup x.exe"
        );
        assert_eq!(file_name_from_url("not a url"), "download");
        // SourceForge's shape: the name is an earlier segment.
        assert_eq!(
            file_name_from_url("https://sf.example/7z2301-x64.exe/download"),
            "7z2301-x64.exe"
        );
        assert_eq!(file_name_from_url("https://h.example/v1.2/get"), "get");
    }

    #[test]
    fn sanitizing_drops_paths_bad_characters_and_devices() {
        assert_eq!(sanitize_file_name("../../autorun.inf"), "autorun.inf");
        assert_eq!(sanitize_file_name("a:b?.txt. "), "a_b_.txt");
        assert_eq!(sanitize_file_name("NUL.txt"), "_NUL.txt");
        assert_eq!(sanitize_file_name(".."), "download");
        assert!(sanitize_file_name(&"x".repeat(400)).chars().count() <= 180);
    }

    #[test]
    fn user_agents_are_one_line_of_at_most_512() {
        assert!(user_agent_is_acceptable(""));
        assert!(user_agent_is_acceptable("  Mozilla/5.0  "));
        assert!(!user_agent_is_acceptable("a\r\nX-Evil: 1"));
        assert!(!user_agent_is_acceptable(&"a".repeat(513)));
        assert_eq!(user_agent_normalize(Some("  x ")), Some("x".into()));
        assert_eq!(user_agent_normalize(Some("   ")), None);
    }

    #[test]
    fn streams_are_recognised_by_type_or_extension() {
        assert!(is_stream(
            "https://h/x",
            Some("application/vnd.apple.mpegurl; charset=utf-8")
        ));
        assert!(is_stream(
            "https://h/v/master.M3U8?sig=1",
            Some("text/plain")
        ));
        assert!(is_stream("https://h/manifest.mpd", None));
        assert!(!is_stream("https://h/file.zip", Some("application/zip")));
        assert_eq!(
            suggest_stream_file_name(
                "https://h/talks/keynote-2026/master.m3u8",
                "master.m3u8",
                ".ts"
            ),
            "keynote-2026.ts"
        );
        assert_eq!(
            suggest_stream_file_name("https://h/1080/index.m3u8", "", ".mp4"),
            "video.mp4"
        );
    }

    #[test]
    fn address_verdicts_follow_the_table() {
        let mut i = item("a.iso", "https://h/a.iso");
        i.total_bytes = 100;
        i.downloaded_bytes = 40;
        i.status = DownloadStatus::Failed;
        let probed = |len: i64| RemoteFileInfo {
            content_length: len,
            final_url: "https://h/a.iso".into(),
            ..Default::default()
        };
        assert_eq!(judge_address(&i, &probed(100)), AddressVerdict::SameFile);
        assert_eq!(
            judge_address(&i, &probed(200)),
            AddressVerdict::DifferentSize
        );
        assert_eq!(judge_address(&i, &probed(0)), AddressVerdict::Unverifiable);
        i.remote_version = Some(RemoteFileVersion::new(
            Some("\"a\"".into()),
            None,
            Some("h".into()),
        ));
        let mut changed = probed(100);
        changed.etag = Some("\"b\"".into());
        assert_eq!(judge_address(&i, &changed), AddressVerdict::NewVersion);
        i.status = DownloadStatus::Cancelled;
        assert_eq!(judge_address(&i, &probed(0)), AddressVerdict::SameFile);
    }

    #[test]
    fn a_capture_matches_by_size_or_else_by_name() {
        let mut i = item("a.iso", "https://h/a.iso");
        i.status = DownloadStatus::Failed;
        i.total_bytes = 100;
        let cap = |size: i64, name: Option<&str>| CapturedDownload {
            url: "https://h2/a.iso".into(),
            total_bytes: size,
            file_name: name.map(str::to_string),
            ..Default::default()
        };
        assert!(matches_capture(&i, &cap(100, None)));
        assert!(!matches_capture(&i, &cap(99, Some("a.iso"))));
        assert!(matches_capture(&i, &cap(0, Some("A.ISO"))));
        i.status = DownloadStatus::Downloading;
        assert!(!matches_capture(&i, &cap(100, None)));
    }
}
