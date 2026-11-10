//! Port of `src/QuickByte.Core/Helpers/AddressRefresh.cs`.
//!
//! The rules behind "Refresh Download Address": whether a new link may carry on
//! a download that already has bytes on disk, and whether a link the browser
//! just handed over is the one a refresh is waiting for.
//!
//! Kept in Core, beside `file_version_check` whose verdict it builds on, because
//! the cost of getting it wrong is a corrupt file rather than a wrong-looking
//! window.

use crate::enums::DownloadStatus;
use crate::helpers::download_actions;
use crate::helpers::file_name_helper::sanitize_file_name;
use crate::helpers::file_version_check::{self, FileVersionVerdict};
use crate::models::{CapturedDownload, DownloadItem, RemoteFileInfo};

/// What a new address turned out to point at, compared with the download it would continue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressVerdict {
    /// The same file. The download carries on from the bytes it has.
    SameFile,
    /// A file of a different length. Not this download under a new name — the
    /// chunks cannot be continued, and the file name, category and size the user
    /// chose all describe something else. It has to be added as a download of
    /// its own.
    DifferentSize,
    /// The same length, but the server that issued the download's version now
    /// describes a different one. Continuing would merge two versions into one
    /// file; starting over is the only safe way to use the address.
    NewVersion,
    /// Nothing to compare: one side never said how big the file is. Harmless
    /// when nothing has been downloaded yet, and the user's call when something has.
    Unverifiable,
}

pub fn judge(item: &DownloadItem, probed: &RemoteFileInfo) -> AddressVerdict {
    if item.total_bytes > 0 && probed.has_known_size() && item.total_bytes != probed.content_length {
        return AddressVerdict::DifferentSize;
    }
    if item.total_bytes <= 0 || !probed.has_known_size() {
        return if has_progress(item) { AddressVerdict::Unverifiable } else { AddressVerdict::SameFile };
    }

    // Same length. Only a validator from the host that issued the download's own
    // can say otherwise; a mirror's different ETag for the identical file is
    // Unknown here, which is exactly what makes switching mirrors work.
    let version = file_version_check::compare(item.total_bytes, item.remote_version.as_ref(), probed.content_length, Some(&probed.version()));
    if version == FileVersionVerdict::Changed && has_progress(item) { AddressVerdict::NewVersion } else { AddressVerdict::SameFile }
}

/// Whether there are bytes on disk a new address could cost. A cancelled
/// download has none whatever its counter says: Stop deleted its chunks.
pub fn has_progress(item: &DownloadItem) -> bool {
    item.downloaded_bytes > 0 && item.status != DownloadStatus::Cancelled
}

/// Whether a link the browser extension just handed over is plausibly the one a
/// refresh for `item` is waiting for — the user went back to the page and
/// clicked the download again.
///
/// Deliberately conservative: a capture that does not match opens the Add
/// window as usual, and one that is wrongly claimed is a download the user asked
/// for that never appears. So a known size must match exactly, and only when
/// either side has no size does the file name decide.
pub fn matches_capture(item: &DownloadItem, captured: &CapturedDownload) -> bool {
    if !download_actions::can_refresh_address(item) {
        return false;
    }
    if item.total_bytes > 0 && captured.total_bytes > 0 {
        return item.total_bytes == captured.total_bytes;
    }
    match captured.file_name.as_deref().filter(|n| !n.trim().is_empty()) {
        Some(name) => sanitize_file_name(name).to_lowercase() == item.file_name.to_lowercase(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    //! Whether a new address may carry on a download that already has bytes on
    //! disk. A refreshed link that is wrongly accepted corrupts the file,
    //! silently; one that is wrongly refused sends the user back to square one on
    //! a download that was fine. (`AddressRefreshTests.cs`.)

    use super::*;
    use crate::models::RemoteFileVersion;

    const HOST: &str = "downloads.example.com";

    fn paused(total: i64, downloaded: i64, etag: Option<&str>) -> DownloadItem {
        DownloadItem {
            file_name: "setup.exe".into(),
            status: DownloadStatus::Paused,
            total_bytes: total,
            downloaded_bytes: downloaded,
            remote_version: etag.map(|e| RemoteFileVersion::new(Some(e.into()), None, Some(HOST.into()))),
            ..Default::default()
        }
    }

    fn probe(size: i64, etag: Option<&str>, host: &str) -> RemoteFileInfo {
        RemoteFileInfo { content_length: size, etag: etag.map(str::to_string), final_url: format!("https://{host}/setup.exe"), ..Default::default() }
    }

    fn capture(file_name: Option<&str>, total: i64) -> CapturedDownload {
        CapturedDownload { url: "https://x/y".into(), file_name: file_name.map(str::to_string), total_bytes: total, ..Default::default() }
    }

    #[test]
    fn an_expired_link_renewed_for_the_same_file_carries_on() {
        assert_eq!(judge(&paused(1000, 400, Some("\"v1\"")), &probe(1000, Some("\"v1\""), HOST)), AddressVerdict::SameFile);
    }

    #[test]
    fn a_link_to_a_file_of_another_size_is_refused_whatever_the_progress() {
        assert_eq!(judge(&paused(1000, 400, Some("\"v1\"")), &probe(2000, Some("\"v1\""), HOST)), AddressVerdict::DifferentSize);
        assert_eq!(judge(&paused(1000, 0, Some("\"v1\"")), &probe(2000, Some("\"v1\""), HOST)), AddressVerdict::DifferentSize);
    }

    #[test]
    fn a_new_version_from_the_same_server_can_only_start_over() {
        assert_eq!(judge(&paused(1000, 400, Some("\"v1\"")), &probe(1000, Some("\"v2\""), HOST)), AddressVerdict::NewVersion);
    }

    #[test]
    fn a_new_version_costs_nothing_when_nothing_was_downloaded() {
        assert_eq!(judge(&paused(1000, 0, Some("\"v1\"")), &probe(1000, Some("\"v2\""), HOST)), AddressVerdict::SameFile);
    }

    #[test]
    fn another_mirror_of_the_same_file_carries_on_despite_its_own_etag() {
        // The ETag of a different host is not evidence of anything.
        let verdict = judge(&paused(1000, 400, Some("\"inode-1\"")), &probe(1000, Some("\"inode-2\""), "mirror2.example.org"));
        assert_eq!(verdict, AddressVerdict::SameFile);
    }

    #[test]
    fn a_size_nobody_can_compare_is_the_user_s_call_only_when_there_is_progress_to_lose() {
        assert_eq!(judge(&paused(1000, 400, None), &probe(0, None, HOST)), AddressVerdict::Unverifiable);
        assert_eq!(judge(&paused(0, 400, None), &probe(1000, None, HOST)), AddressVerdict::Unverifiable);
        assert_eq!(judge(&paused(0, 0, None), &probe(1000, None, HOST)), AddressVerdict::SameFile);
    }

    #[test]
    fn a_cancelled_download_has_no_progress_whatever_its_counter_says() {
        // Stop deleted the chunks; the counter is just what it last showed.
        let item = DownloadItem { status: DownloadStatus::Cancelled, ..paused(1000, 400, Some("\"v1\"")) };
        assert!(!has_progress(&item));
        assert_eq!(judge(&item, &probe(1000, Some("\"v2\""), HOST)), AddressVerdict::SameFile);
    }

    #[test]
    fn a_capture_of_the_same_size_is_the_link_a_refresh_is_waiting_for() {
        let item = paused(1000, 400, None);
        assert!(matches_capture(&item, &capture(None, 1000)));
        assert!(!matches_capture(&item, &capture(None, 999)));
    }

    #[test]
    fn without_a_size_on_either_side_the_file_name_decides() {
        let item = paused(0, 400, None);
        assert!(matches_capture(&item, &capture(Some("SETUP.EXE"), 1000)));
        assert!(!matches_capture(&item, &capture(Some("other.exe"), 1000)));
        assert!(!matches_capture(&item, &capture(None, 0)));
    }

    #[test]
    fn a_different_size_is_not_rescued_by_a_matching_name() {
        // A capture wrongly claimed is a download the user asked for that never
        // appears, so a size that disagrees is final.
        assert!(!matches_capture(&paused(1000, 400, None), &capture(Some("setup.exe"), 5)));
    }

    #[test]
    fn a_running_download_is_not_waiting_for_anything() {
        let item = DownloadItem { status: DownloadStatus::Downloading, ..paused(1000, 400, None) };
        assert!(!matches_capture(&item, &capture(None, 1000)));
    }
}
