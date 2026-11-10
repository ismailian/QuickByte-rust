//! Port of `src/QuickByte.Core/Helpers/FileNameHelper.cs` — file name
//! derivation and collision-avoidance utilities.

use std::path::Path;

use percent_encoding::percent_decode_str;

use crate::models::join_path;

/// Content types worth turning into an extension. Deliberately a short,
/// unambiguous list rather than a full MIME registry: this only ever fires when
/// the URL yielded no extension at all, and guessing wrong there produces a file
/// Explorer refuses to open with anything sensible. `application/octet-stream`
/// is absent on purpose — it means "bytes", which is exactly what we already knew.
const EXTENSION_BY_CONTENT_TYPE: &[(&str, &str)] = &[
    ("application/epub+zip", "epub"),
    ("application/gzip", "gz"),
    ("application/java-archive", "jar"),
    ("application/json", "json"),
    ("application/pdf", "pdf"),
    ("application/vnd.android.package-archive", "apk"),
    ("application/vnd.debian.binary-package", "deb"),
    ("application/vnd.microsoft.portable-executable", "exe"),
    ("application/vnd.rar", "rar"),
    ("application/x-7z-compressed", "7z"),
    ("application/x-apple-diskimage", "dmg"),
    ("application/x-bzip2", "bz2"),
    ("application/x-gzip", "gz"),
    ("application/x-iso9660-image", "iso"),
    ("application/x-msdownload", "exe"),
    ("application/x-ms-installer", "msi"),
    ("application/x-msi", "msi"),
    ("application/x-rar-compressed", "rar"),
    ("application/x-rpm", "rpm"),
    ("application/x-tar", "tar"),
    ("application/x-xz", "xz"),
    ("application/zip", "zip"),
    ("audio/flac", "flac"),
    ("audio/mp4", "m4a"),
    ("audio/mpeg", "mp3"),
    ("audio/ogg", "ogg"),
    ("audio/wav", "wav"),
    ("image/gif", "gif"),
    ("image/jpeg", "jpg"),
    ("image/png", "png"),
    ("image/svg+xml", "svg"),
    ("image/webp", "webp"),
    ("text/csv", "csv"),
    ("text/html", "html"),
    ("text/plain", "txt"),
    ("video/mp4", "mp4"),
    ("video/quicktime", "mov"),
    ("video/webm", "webm"),
    ("video/x-matroska", "mkv"),
    ("video/x-msvideo", "avi"),
];

/// Names Windows reserves for devices. They are not files at any extension —
/// `NUL.txt` is the null device just as much as `NUL` is — and opening one
/// succeeds, so a download named after one silently writes its bytes to the
/// device instead of to the disk.
const RESERVED_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2",
    "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Longest name kept. MAX_PATH is 260 characters for the *whole* path, and the
/// download folder, its separator and a " (12)" collision suffix all come out of
/// the same budget — so a server answering with a 400-character
/// `Content-Disposition` must not be the reason the download fails at the moment
/// the file is created, several minutes of transfer later.
const MAX_FILE_NAME_LENGTH: usize = 180;

/// `Path.GetInvalidFileNameChars()` on Windows.
fn is_invalid_file_name_char(c: char) -> bool {
    matches!(c, '"' | '<' | '>' | '|' | ':' | '*' | '?' | '\\' | '/') || (c as u32) < 32
}

pub fn sanitize_file_name(name: &str) -> String {
    // A server-supplied name may carry a path — deliberately, in the case of a
    // "../../autorun.inf" — and the caller wants a file name, not a route out of
    // the download folder.
    let name = match name.rfind(['/', '\\']) {
        Some(separator) => &name[separator + 1..],
        None => name,
    };

    let name: String = name
        .trim()
        .trim_matches('"')
        .chars()
        .map(|c| if is_invalid_file_name_char(c) { '_' } else { c })
        .collect();

    // Trailing dots and spaces are legal on the wire and not part of a Windows
    // file name: the shell drops them when the file is created, so a name kept
    // with them would stop matching the file that actually appears — and every
    // later existence check on it. Dropping them here also disposes of "." and
    // "..", which survive the replacement above intact.
    let name = name.trim_end_matches(['.', ' ']);
    if name.is_empty() {
        return "download".into();
    }

    let name = shorten(name);
    if is_reserved_device_name(&name) { format!("_{name}") } else { name }
}

fn shorten(name: &str) -> String {
    let length = name.chars().count();
    if length <= MAX_FILE_NAME_LENGTH {
        return name.to_string();
    }

    // An "extension" that is most of the name is not one — a query string that
    // came through as part of a segment, say. Truncating the stem to nothing in
    // order to preserve it helps nobody.
    let mut extension = extension_of(name);
    if extension.chars().count() > 12 {
        extension = "";
    }

    let keep = MAX_FILE_NAME_LENGTH - extension.chars().count();
    let stem: String = name.chars().take(keep).collect();
    format!("{}{extension}", stem.trim_end_matches(['.', ' ']))
}

fn is_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    RESERVED_DEVICE_NAMES.iter().any(|reserved| reserved.eq_ignore_ascii_case(stem))
}

/// `Path.GetExtension` for a bare file name: from the last dot, dot included;
/// empty when there is none or the dot is the last character.
pub(crate) fn extension_of(name: &str) -> &str {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match file.rfind('.') {
        Some(dot) if dot + 1 < file.len() => &file[dot..],
        _ => "",
    }
}

/// `Path.GetFileNameWithoutExtension` for a bare file name.
pub(crate) fn stem_of(name: &str) -> &str {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match file.rfind('.') {
        Some(dot) => &file[..dot],
        None => file,
    }
}

/// Derives a file name from a URL's path, ignoring query and fragment.
///
/// Callers must pass the URL *after* redirects. A great many download links are
/// an opaque endpoint that redirects to the real file — SourceForge's
/// `.../7z2301-x64.exe/download` being the canonical example — and naming the
/// file from the address the user pasted produces "download".
///
/// When the last segment carries no extension, earlier segments are tried in
/// turn, which recovers the name from that same shape without a redirect having
/// to happen at all. The check for what counts as an extension is deliberately
/// strict, so a version number in a path (`/v1.2/get`) isn't mistaken for a file.
pub fn file_name_from_url(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else { return "download".into() };
    let Some(segments) = parsed.path_segments() else { return "download".into() };
    let segments: Vec<String> =
        segments.filter(|s| !s.is_empty()).map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();

    for candidate in segments.iter().rev() {
        if looks_like_file_name(candidate) {
            return sanitize_file_name(candidate);
        }
    }

    segments.last().map(|s| sanitize_file_name(s)).unwrap_or_else(|| "download".into())
}

/// Whether a path segment reads as a file rather than as a directory. Wants a
/// dot with 2–8 trailing characters that are alphanumeric and include at least
/// one letter — enough for `.mp4`, `.7z` and `.tar.gz`, and not enough for `v1.2`.
fn looks_like_file_name(segment: &str) -> bool {
    let Some(dot) = segment.rfind('.') else { return false };
    if dot == 0 || dot == segment.len() - 1 {
        return false;
    }
    let extension = &segment[dot + 1..];
    let length = extension.chars().count();
    (2..=8).contains(&length) && extension.chars().all(char::is_alphanumeric) && extension.chars().any(char::is_alphabetic)
}

/// Gives a name an extension from the server's content type when it has none —
/// an endpoint like `/files/download?id=8821` otherwise saves a video as an
/// extensionless "download" that nothing on the machine will open. A name that
/// already has an extension is left alone: the server's idea of the type is not
/// better than an explicit `.mkv`, and overriding it would rename every file
/// served as the wrong MIME type.
pub fn ensure_extension(file_name: &str, content_type: Option<&str>) -> String {
    let Some(content_type) = content_type.filter(|c| !c.trim().is_empty()) else { return file_name.to_string() };
    if looks_like_file_name(file_name) {
        return file_name.to_string();
    }

    // "video/mp4; charset=binary" — the parameters are not part of the type.
    let media_type = content_type.split(';').next().unwrap_or("").trim();
    match EXTENSION_BY_CONTENT_TYPE.iter().find(|(t, _)| t.eq_ignore_ascii_case(media_type)) {
        Some((_, extension)) => format!("{}.{extension}", file_name.trim_end_matches('.')),
        None => file_name.to_string(),
    }
}

/// Appends " (1)", " (2)"... to avoid overwriting an existing file.
pub fn get_available_file_path(folder: &str, file_name: &str) -> String {
    get_available_file_path_with(folder, file_name, |_| false)
}

/// The same, also stepping past any path `is_reserved` claims — a download that
/// has not written its file yet. Two downloads added with the same name used to
/// be given the same path, and the second to finish replaced the first; a batch
/// of links that all end in `download.php` made that the rule rather than the
/// exception.
pub fn get_available_file_path_with(folder: &str, file_name: &str, is_reserved: impl Fn(&str) -> bool) -> String {
    let full_path = join_path(folder, file_name);
    if !Path::new(&full_path).is_file() && !is_reserved(&full_path) {
        return full_path;
    }

    let name_only = stem_of(file_name);
    let extension = extension_of(file_name);
    (1..)
        .map(|i| join_path(folder, &format!("{name_only} ({i}){extension}")))
        .find(|candidate| !Path::new(candidate).is_file() && !is_reserved(candidate))
        .expect("an unbounded search always finds a free name")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempFolder(std::path::PathBuf);
    impl TempFolder {
        fn new() -> Self {
            let path = std::env::temp_dir().join("QuickByte.Tests").join(uuid::Uuid::new_v4().simple().to_string());
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }
    impl Drop for TempFolder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn sanitize_strips_any_directory_part() {
        for (given, expected) in [
            ("../../autorun.inf", "autorun.inf"),
            ("..\\..\\windows\\system32\\evil.dll", "evil.dll"),
            ("/etc/passwd", "passwd"),
            ("C:\\Users\\someone\\report.pdf", "report.pdf"),
        ] {
            assert_eq!(sanitize_file_name(given), expected);
        }
    }

    #[test]
    fn sanitize_replaces_characters_windows_will_not_accept() {
        let sanitized = sanitize_file_name("in<va>lid:na|me?.txt");
        assert_eq!(sanitized, "in_va_lid_na_me_.txt");
        assert!(!sanitized.chars().any(is_invalid_file_name_char));
    }

    #[test]
    fn sanitize_unwraps_the_quotes_a_content_disposition_arrives_in() {
        assert_eq!(sanitize_file_name("  \"archive.zip\"  "), "archive.zip");
    }

    #[test]
    fn sanitize_falls_back_when_nothing_usable_is_left() {
        for given in ["", "   ", ".", "..", "..."] {
            assert_eq!(sanitize_file_name(given), "download", "{given:?}");
        }
    }

    #[test]
    fn sanitize_drops_trailing_dots_and_spaces() {
        for (given, expected) in [("report.", "report"), ("report.txt.", "report.txt"), ("report ", "report")] {
            assert_eq!(sanitize_file_name(given), expected);
        }
    }

    #[test]
    fn sanitize_defuses_reserved_device_names() {
        for given in ["CON", "nul", "NUL.txt", "COM1", "LPT9.dat", "aux", "PRN.pdf"] {
            let sanitized = sanitize_file_name(given);
            assert_ne!(sanitized, given);
            assert!(sanitized.starts_with('_'), "{sanitized}");
        }
    }

    #[test]
    fn sanitize_leaves_names_that_merely_start_like_a_device() {
        for given in ["CONTENTS.txt", "console.log", "COM10.bin", "auxiliary.dat"] {
            assert_eq!(sanitize_file_name(given), given);
        }
    }

    #[test]
    fn sanitize_caps_the_length_but_keeps_the_extension() {
        let sanitized = sanitize_file_name(&format!("{}.zip", "a".repeat(400)));
        assert!(sanitized.chars().count() <= 180, "length was {}", sanitized.len());
        assert!(sanitized.ends_with(".zip"));
        assert!(sanitized.starts_with("aaaa"));
    }

    #[test]
    fn sanitize_gives_up_on_an_extension_that_is_most_of_the_name() {
        let sanitized = sanitize_file_name(&format!("file.{}", "b".repeat(400)));
        assert!(sanitized.chars().count() <= 180);
        assert!(sanitized.starts_with("file.bbbb"));
    }

    #[test]
    fn file_name_from_url_takes_the_last_segment() {
        assert_eq!(file_name_from_url("https://example.com/files/setup.exe"), "setup.exe");
    }

    #[test]
    fn file_name_from_url_ignores_query_and_fragment() {
        assert_eq!(file_name_from_url("https://example.com/setup.exe?token=abc&x=1#top"), "setup.exe");
    }

    #[test]
    fn file_name_from_url_walks_back_past_a_segment_that_is_not_a_file() {
        assert_eq!(
            file_name_from_url("https://sourceforge.net/projects/sevenzip/files/7z2301-x64.exe/download"),
            "7z2301-x64.exe"
        );
    }

    #[test]
    fn file_name_from_url_does_not_mistake_a_version_number_for_a_file() {
        assert_eq!(file_name_from_url("https://example.com/v1.2/get"), "get");
        assert_eq!(file_name_from_url("https://example.com/api/v2.0/fetch"), "fetch");
    }

    #[test]
    fn file_name_from_url_decodes_percent_escapes() {
        assert_eq!(file_name_from_url("https://example.com/my%20report.pdf"), "my report.pdf");
    }

    #[test]
    fn file_name_from_url_sanitizes_what_it_recovers() {
        assert_eq!(file_name_from_url("https://example.com/a/..%2F..%2Fpasswd"), "passwd");
    }

    #[test]
    fn file_name_from_url_falls_back_rather_than_throwing() {
        for url in ["not a url at all", "", "https://example.com", "https://example.com/"] {
            assert_eq!(file_name_from_url(url), "download", "{url}");
        }
    }

    #[test]
    fn ensure_extension_names_an_extensionless_file_from_the_content_type() {
        for (content_type, expected) in [
            ("video/mp4", "download.mp4"),
            ("application/pdf", "download.pdf"),
            ("application/x-7z-compressed", "download.7z"),
            ("video/mp4; charset=binary", "download.mp4"),
            ("VIDEO/MP4", "download.mp4"),
        ] {
            assert_eq!(ensure_extension("download", Some(content_type)), expected);
        }
    }

    #[test]
    fn ensure_extension_leaves_a_name_that_already_has_one() {
        assert_eq!(ensure_extension("movie.mkv", Some("application/octet-stream")), "movie.mkv");
        assert_eq!(ensure_extension("movie.mkv", Some("text/html")), "movie.mkv");
    }

    #[test]
    fn ensure_extension_leaves_the_name_alone_when_the_type_says_nothing() {
        for content_type in [Some("application/octet-stream"), Some("application/some-thing-nobody-has-heard-of"), Some(""), None] {
            assert_eq!(ensure_extension("download", content_type), "download");
        }
    }

    #[test]
    fn available_path_is_the_plain_path_when_nothing_is_there() {
        let folder = TempFolder::new();
        assert_eq!(get_available_file_path(&folder.path(), "a.bin"), join_path(&folder.path(), "a.bin"));
    }

    #[test]
    fn available_path_counts_up_past_every_collision() {
        let folder = TempFolder::new();
        std::fs::write(join_path(&folder.path(), "a.bin"), "1").unwrap();
        std::fs::write(join_path(&folder.path(), "a (1).bin"), "2").unwrap();
        assert_eq!(get_available_file_path(&folder.path(), "a.bin"), join_path(&folder.path(), "a (2).bin"));
    }

    #[test]
    fn available_path_steps_past_a_name_a_download_has_spoken_for() {
        let folder = TempFolder::new();
        let reserved = join_path(&folder.path(), "download.php");
        let path = get_available_file_path_with(&folder.path(), "download.php", |candidate| candidate == reserved);
        assert_eq!(path, join_path(&folder.path(), "download (1).php"));
    }

    #[test]
    fn available_path_keeps_the_extension_on_the_end() {
        let folder = TempFolder::new();
        std::fs::write(join_path(&folder.path(), "archive.tar.gz"), "1").unwrap();
        assert_eq!(get_available_file_path(&folder.path(), "archive.tar.gz"), join_path(&folder.path(), "archive.tar (1).gz"));
    }
}
