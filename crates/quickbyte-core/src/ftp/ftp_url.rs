//! Port of `src/QuickByte.Core/Services/Ftp/FtpUrl.cs`.
//!
//! Turns an `ftp://` URL into the two strings the protocol actually wants: the
//! path to hand to `RETR`, and a file name to save under.

use percent_encoding::percent_decode_str;
use url::Url;

use crate::helpers::file_name_helper;

/// True for the schemes the FTP connection factory and info provider handle.
pub fn is_ftp_url(url: &Url) -> bool {
    matches!(url.scheme(), "ftp" | "ftps")
}

pub fn is_ftp(url: &str) -> bool {
    Url::parse(url.trim()).is_ok_and(|u| is_ftp_url(&u))
}

/// The path as the server expects to see it. A URL's path is percent-encoded — a
/// file with a space in its name arrives as `%20`, and FTP has no such encoding,
/// so it must be undone before the path goes on the wire or the server looks for
/// a file whose name really does contain a percent sign.
pub fn path_of(url: &Url) -> String {
    let path = percent_decode_str(url.path()).decode_utf8_lossy().into_owned();
    if path.is_empty() { "/".into() } else { path }
}

pub fn file_name_of(url: &Url) -> String {
    let path = path_of(url);
    let name = path.rsplit(['/', '\\']).next().unwrap_or("");
    if name.trim().is_empty() { "download".into() } else { file_name_helper::sanitize_file_name(name) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn is_ftp_recognises_only_the_schemes_the_ftp_client_handles() {
        for (text, expected) in [
            ("ftp://example.com/x.iso", true),
            ("ftps://example.com/x.iso", true),
            ("FtP://example.com/x.iso", true),
            ("https://example.com/x.iso", false),
            ("sftp://example.com/x.iso", false),
            ("nonsense", false),
            ("", false),
        ] {
            assert_eq!(is_ftp(text), expected, "{text}");
        }
    }

    #[test]
    fn path_of_undoes_percent_encoding_before_the_path_goes_on_the_wire() {
        assert_eq!(path_of(&url("ftp://example.com/pub/my%20file.iso")), "/pub/my file.iso");
    }

    #[test]
    fn path_of_a_bare_host_is_the_root() {
        assert_eq!(path_of(&url("ftp://example.com")), "/");
    }

    #[test]
    fn file_name_of_takes_the_last_segment() {
        assert_eq!(file_name_of(&url("ftp://example.com/pub/disk.iso")), "disk.iso");
    }

    #[test]
    fn file_name_of_sanitizes_what_it_finds() {
        assert_eq!(file_name_of(&url("ftp://example.com/pub/a%3Ab.iso")), "a_b.iso");
    }

    #[test]
    fn file_name_of_falls_back_when_the_url_names_no_file() {
        assert_eq!(file_name_of(&url("ftp://example.com/pub/")), "download");
    }
}
