//! Port of `ClipboardLinks.cs`: decides whether something copied to the
//! clipboard is a download worth offering — the rule behind Options > Interface >
//! "Watch the clipboard".
//!
//! The clipboard carries everything a user copies: passwords, messages, whole
//! documents. So the rule is deliberately narrow. Only a clipboard that holds
//! *one* link and nothing else counts, only to a file whose extension is on the
//! user's list, and nothing about any other clipboard content is looked at, kept
//! or shown.

use std::collections::BTreeSet;

use percent_encoding::percent_decode_str;

/// What a download manager is usually wanted for, and not what a browser shows inline.
pub const DEFAULT_FILE_TYPES: &str =
    "zip rar 7z gz xz bz2 tar iso img dmg exe msi msix appx apk deb rpm mp4 mkv avi mov webm mp3 flac wav m4a pdf epub";

/// Longer than any real download link, and short enough that a pasted document
/// is never scanned.
pub const MAX_LINK_LENGTH: usize = 2048;

/// The extensions in a user-edited list — separated by spaces, commas or
/// semicolons, with or without dots, in any case. Anything that is not a plain
/// extension is dropped rather than guessed at. Returned lower-cased.
pub fn parse_file_types(list: Option<&str>) -> BTreeSet<String> {
    let mut types = BTreeSet::new();
    for part in list.unwrap_or_default().split([' ', ',', ';', '\t', '\r', '\n']).filter(|p| !p.is_empty()) {
        let ty = part.trim().trim_start_matches('*').trim_start_matches('.');
        if (1..=10).contains(&ty.len()) && ty.bytes().all(|b| b.is_ascii_alphanumeric()) {
            types.insert(ty.to_ascii_lowercase());
        }
    }
    types
}

/// The link to offer, or `None`. `text` must be a single http(s) or ftp(s) URL —
/// surrounding whitespace aside — whose path ends in one of `file_types`
/// (lower-case, as [`parse_file_types`] returns them).
pub fn find_download_link(text: Option<&str>, file_types: &BTreeSet<String>) -> Option<String> {
    let text = text?;
    if text.trim().is_empty() || file_types.is_empty() {
        return None;
    }
    let candidate = text.trim();
    if candidate.chars().count() > MAX_LINK_LENGTH || candidate.chars().any(char::is_whitespace) {
        return None;
    }
    let uri = parse_web_or_ftp_link(candidate)?;
    let path = percent_decode_str(uri.path()).decode_utf8_lossy();
    let extension = path_extension(&path);
    (!extension.is_empty() && file_types.contains(&extension.to_ascii_lowercase())).then(|| candidate.to_string())
}

/// `Uri.TryCreate(..., Absolute)` with an http, https, ftp or ftps scheme and a
/// non-empty host — the only links QuickByte will take from pasted text.
pub(crate) fn parse_web_or_ftp_link(link: &str) -> Option<url::Url> {
    let uri = url::Url::parse(link).ok()?;
    if !matches!(uri.scheme(), "http" | "https" | "ftp" | "ftps") {
        return None;
    }
    uri.host_str().filter(|h| !h.is_empty())?;
    Some(uri)
}

/// `Path.GetExtension(...).TrimStart('.')`: what follows the last `.` of the last
/// path segment, or empty.
fn path_extension(path: &str) -> &str {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) => &name[dot + 1..],
        None => "",
    }
}

#[cfg(test)]
mod tests {
    //! Most of these tests are about everything that must *not* be offered.

    use super::*;
    use crate::models::DownloadSettings;

    fn types() -> BTreeSet<String> {
        parse_file_types(Some("zip iso exe"))
    }

    #[test]
    fn a_single_link_to_a_listed_file_type_is_offered() {
        for copied in [
            "https://example.com/files/setup.exe",
            "  https://example.com/files/archive.ZIP  ",
            "ftp://mirror.example.org/pub/disk.iso",
            "https://example.com/get/archive.zip?token=abc#part",
            "https://example.com/my%20files/setup.exe",
        ] {
            assert_eq!(find_download_link(Some(copied), &types()).as_deref(), Some(copied.trim()), "{copied}");
        }
    }

    #[test]
    fn anything_else_is_not_looked_at_twice() {
        for copied in [
            Some("https://example.com/page.html"),
            Some("https://example.com/download"),
            Some("https://example.com/files/setup.exe and some words"),
            Some("see https://example.com/setup.exe"),
            Some("https://example.com/a.zip\nhttps://example.com/b.zip"),
            Some("file:///C:/Windows/setup.exe"),
            Some("javascript:alert('x.exe')"),
            Some("my password is hunter2.exe"),
            Some("setup.exe"),
            Some(""),
            None,
        ] {
            assert_eq!(find_download_link(copied, &types()), None, "{copied:?}");
        }
    }

    #[test]
    fn a_pasted_document_is_never_scanned_for_a_link() {
        let huge = format!("https://example.com/{}.zip", "a".repeat(MAX_LINK_LENGTH));
        assert_eq!(find_download_link(Some(&huge), &types()), None);
    }

    #[test]
    fn an_empty_list_offers_nothing() {
        assert_eq!(find_download_link(Some("https://example.com/a.zip"), &parse_file_types(Some(""))), None);
    }

    #[test]
    fn the_list_takes_dots_stars_commas_and_any_case() {
        let types = parse_file_types(Some(" .ZIP, *.iso;exe  mp4 "));
        assert_eq!(types.into_iter().collect::<Vec<_>>(), ["exe", "iso", "mp4", "zip"]);
    }

    #[test]
    fn entries_that_are_not_plain_extensions_are_dropped() {
        let types = parse_file_types(Some("zip ../evil exe* verylongextension tar.gz"));
        assert_eq!(types.into_iter().collect::<Vec<_>>(), ["zip"]);
    }

    #[test]
    fn watching_the_clipboard_is_off_until_the_user_turns_it_on() {
        let settings = DownloadSettings::default();
        assert!(!settings.monitor_clipboard);
        assert!(parse_file_types(Some(&settings.clipboard_file_types)).contains("iso"));
    }
}
