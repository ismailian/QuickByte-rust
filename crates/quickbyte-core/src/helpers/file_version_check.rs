//! Port of `src/QuickByte.Core/Helpers/FileVersionCheck.cs`.
//!
//! Decides whether two descriptions of a remote file describe the same version
//! of it — the question a resume has to answer before it appends anything.
//!
//! The two ways to be wrong are not symmetrical, and the rules lean accordingly.
//! Calling an unchanged file changed throws away progress that was fine, which
//! on a 30 GB download is hours. Calling a changed file unchanged corrupts it,
//! but that is exactly what every resume did before this existed. So a verdict
//! of [`FileVersionVerdict::Changed`] needs evidence that does not argue with
//! itself:
//!
//! * A different size is decisive. It is also the one signal that does not
//!   depend on which server answered, so it is checked across hosts.
//! * A different ETag counts only when the Last-Modified dates do not agree.
//!   Mirrors synchronised with rsync keep the file's timestamp but can compute
//!   their ETags from something local, such as an inode number.
//! * Validators from two different hosts are not compared at all — see
//!   [`RemoteFileVersion::host`].

use crate::models::RemoteFileVersion;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileVersionVerdict {
    /// Nothing comparable, or evidence pointing both ways. Keep what is on disk.
    Unknown,
    /// A validator both sides share agrees, and nothing contradicts it.
    Same,
    /// The server is describing a different file from the one the chunks came from.
    Changed,
}

pub fn compare(
    known_size: i64,
    known: Option<&RemoteFileVersion>,
    probed_size: i64,
    probed: Option<&RemoteFileVersion>,
) -> FileVersionVerdict {
    if known_size > 0 && probed_size > 0 && known_size != probed_size {
        return FileVersionVerdict::Changed;
    }

    let (Some(known), Some(probed)) = (known, probed) else { return FileVersionVerdict::Unknown };
    if !same_host(known.host.as_deref(), probed.host.as_deref()) {
        return FileVersionVerdict::Unknown;
    }

    let etags_agree = match (&known.etag, &probed.etag) {
        (Some(a), Some(b)) => Some(same_opaque_tag(a, b)),
        _ => None,
    };
    let dates_agree = match (known.last_modified, probed.last_modified) {
        (Some(a), Some(b)) => Some(a.0 == b.0),
        _ => None,
    };

    match (etags_agree, dates_agree) {
        (Some(true), Some(false)) | (Some(false), Some(true)) => FileVersionVerdict::Unknown,
        (Some(false), _) | (_, Some(false)) => FileVersionVerdict::Changed,
        (Some(true), _) | (_, Some(true)) => FileVersionVerdict::Same,
        _ => FileVersionVerdict::Unknown,
    }
}

/// Compares the quoted part of two entity tags, ignoring weakness. A proxy that
/// compresses a response is allowed to weaken its tag (nginx does), and that
/// does not make it a different file.
pub fn same_opaque_tag(a: &str, b: &str) -> bool {
    strip_weakness(a) == strip_weakness(b)
}

/// A missing host on either side is treated as a match: it only happens for an
/// address that did not parse, and refusing to compare would disable the check
/// for no reason anyone could see.
pub fn same_host(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => true,
    }
}

fn strip_weakness(tag: &str) -> &str {
    tag.strip_prefix("W/").unwrap_or(tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serde_compat::DateTimeOffset;
    use chrono::{FixedOffset, TimeZone};

    const HOST: &str = "downloads.example.com";

    fn version(etag: Option<&str>, last_modified: Option<DateTimeOffset>, host: &str) -> RemoteFileVersion {
        RemoteFileVersion::new(etag.map(String::from), last_modified, Some(host.to_string()))
    }
    fn v(etag: &str) -> RemoteFileVersion {
        version(Some(etag), None, HOST)
    }
    fn date(day: u32) -> DateTimeOffset {
        DateTimeOffset(FixedOffset::east_opt(0).unwrap().with_ymd_and_hms(2026, 9, day, 12, 0, 0).unwrap())
    }

    #[test]
    fn a_different_size_is_a_different_file() {
        assert_eq!(compare(1000, Some(&v("\"a\"")), 1001, Some(&v("\"a\""))), FileVersionVerdict::Changed);
    }

    #[test]
    fn a_different_size_counts_even_when_another_host_answered() {
        let verdict = compare(
            1000,
            Some(&version(Some("\"a\""), None, "mirror1.example.org")),
            2000,
            Some(&version(Some("\"a\""), None, "mirror2.example.org")),
        );
        assert_eq!(verdict, FileVersionVerdict::Changed);
    }

    #[test]
    fn the_same_etag_is_the_same_file() {
        assert_eq!(compare(1000, Some(&v("\"a\"")), 1000, Some(&v("\"a\""))), FileVersionVerdict::Same);
    }

    #[test]
    fn a_different_etag_is_a_different_file() {
        assert_eq!(compare(1000, Some(&v("\"a\"")), 1000, Some(&v("\"b\""))), FileVersionVerdict::Changed);
    }

    #[test]
    fn a_tag_a_proxy_weakened_still_names_the_same_file() {
        assert_eq!(compare(1000, Some(&v("\"a\"")), 1000, Some(&v("W/\"a\""))), FileVersionVerdict::Same);
    }

    #[test]
    fn etags_that_disagree_with_dates_that_agree_decide_nothing() {
        let verdict = compare(
            1000,
            Some(&version(Some("\"inode-1\""), Some(date(1)), HOST)),
            1000,
            Some(&version(Some("\"inode-2\""), Some(date(1)), HOST)),
        );
        assert_eq!(verdict, FileVersionVerdict::Unknown);
    }

    #[test]
    fn a_different_date_is_a_different_file_when_there_is_no_etag() {
        let verdict =
            compare(1000, Some(&version(None, Some(date(1)), HOST)), 1000, Some(&version(None, Some(date(2)), HOST)));
        assert_eq!(verdict, FileVersionVerdict::Changed);
    }

    #[test]
    fn validators_from_another_host_are_not_compared() {
        let verdict = compare(
            1000,
            Some(&version(Some("\"a\""), Some(date(1)), "mirror1.example.org")),
            1000,
            Some(&version(Some("\"b\""), Some(date(2)), "mirror2.example.org")),
        );
        assert_eq!(verdict, FileVersionVerdict::Unknown);
    }

    #[test]
    fn hosts_are_compared_without_regard_to_case() {
        let verdict = compare(
            1000,
            Some(&version(Some("\"a\""), None, "Downloads.Example.com")),
            1000,
            Some(&version(Some("\"b\""), None, "downloads.example.com")),
        );
        assert_eq!(verdict, FileVersionVerdict::Changed);
    }

    #[test]
    fn a_download_recorded_before_versions_were_kept_is_judged_on_size_alone() {
        assert_eq!(compare(1000, None, 1000, Some(&v("\"a\""))), FileVersionVerdict::Unknown);
        assert_eq!(compare(1000, None, 1200, Some(&v("\"a\""))), FileVersionVerdict::Changed);
    }

    #[test]
    fn an_unknown_size_on_either_side_is_not_a_size_change() {
        assert_eq!(compare(0, Some(&v("\"a\"")), 1000, Some(&v("\"a\""))), FileVersionVerdict::Same);
    }

    #[test]
    fn nothing_to_compare_is_not_evidence_of_a_change() {
        assert_eq!(
            compare(1000, Some(&version(None, None, HOST)), 1000, Some(&version(None, None, HOST))),
            FileVersionVerdict::Unknown
        );
    }
}
