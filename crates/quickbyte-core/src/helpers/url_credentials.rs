//! Port of `UrlCredentials.cs`: pulls the `user:password@` part out of a URL.
//!
//! `ftp://alice:hunter2@example.com/disk.iso` is still the way FTP links are
//! pasted around, and the whole point of `secret_protector` is lost if that
//! password is then persisted as part of `DownloadItem.url` — where it would sit
//! in plain text in downloads.json, in the details window, and in the main list's
//! tooltip. Splitting it at the point the download is created keeps the secret in
//! the one field that knows how to protect itself.

use percent_encoding::percent_decode_str;

use crate::models::DownloadCredentials;

/// A URL with its login taken out, and the login.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Split {
    pub url: String,
    pub credentials: Option<DownloadCredentials>,
}

pub fn extract(url: &str) -> Split {
    let untouched = || Split { url: url.to_string(), credentials: None };
    let Ok(mut uri) = url::Url::parse(url) else {
        return untouched();
    };
    if uri.username().is_empty() && uri.password().is_none() {
        return untouched();
    }

    let unescape = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
    // The URL parser splits on the first colon only — the rest belongs to the password.
    let credentials = DownloadCredentials::new(unescape(uri.username()), uri.password().map(unescape).unwrap_or_default());

    let _ = uri.set_username("");
    let _ = uri.set_password(None);
    Split { url: uri.to_string(), credentials: (!credentials.is_empty()).then_some(credentials) }
}

#[cfg(test)]
mod tests {
    //! A login typed into the address has to come out of it, or the probe
    //! resolves a real file size and the connections fetch copies of a login page.

    use super::*;

    #[test]
    fn extract_pulls_the_login_out_of_the_url() {
        let split = extract("ftp://alice:hunter2@example.com/disk.iso");
        let credentials = split.credentials.unwrap();
        assert_eq!(credentials.user_name, "alice");
        assert_eq!(credentials.password, "hunter2");
    }

    #[test]
    fn extract_leaves_no_password_in_the_url_that_gets_persisted() {
        let split = extract("ftp://alice:hunter2@example.com/disk.iso");
        // DownloadItem.Url is persisted, displayed, and put in tooltips. The
        // credential field is the only one that knows how to protect itself.
        assert!(!split.url.contains("hunter2"));
        assert!(!split.url.contains("alice"));
        assert!(split.url.contains("example.com/disk.iso"));
    }

    #[test]
    fn extract_handles_a_user_with_no_password() {
        let split = extract("ftp://alice@example.com/disk.iso");
        let credentials = split.credentials.unwrap();
        assert_eq!(credentials.user_name, "alice");
        assert_eq!(credentials.password, "");
    }

    #[test]
    fn extract_decodes_percent_escapes_in_the_login() {
        let credentials = extract("ftp://user%40corp:p%40ss@example.com/x.iso").credentials.unwrap();
        assert_eq!(credentials.user_name, "user@corp");
        assert_eq!(credentials.password, "p@ss");
    }

    #[test]
    fn extract_keeps_a_password_containing_a_colon() {
        // Split on the first colon only — the rest belongs to the password.
        assert_eq!(extract("ftp://alice:a:b:c@example.com/x.iso").credentials.unwrap().password, "a:b:c");
    }

    #[test]
    fn extract_returns_the_url_untouched_when_there_is_no_login() {
        for url in ["https://example.com/file.zip", "ftp://example.com/file.zip"] {
            let split = extract(url);
            assert_eq!(split.credentials, None);
            assert_eq!(split.url, url);
        }
    }

    #[test]
    fn extract_leaves_something_that_is_not_a_url_alone() {
        let split = extract("this is not a url");
        assert_eq!(split.credentials, None);
        assert_eq!(split.url, "this is not a url");
    }
}
