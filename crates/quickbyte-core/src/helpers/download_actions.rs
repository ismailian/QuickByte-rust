//! Port of `DownloadActions.cs`: which lifecycle commands are legal for a
//! download in its current state.
//!
//! This lives in Core rather than in a window because it is a statement about
//! [`DownloadStatus`], not about chrome: the same predicates decide whether a
//! toolbar button is greyed, whether a context-menu entry is shown at all, and
//! whether a tray command has anything to act on. Keeping one copy is what stops
//! the three from drifting — the menus used to offer Resume on a download that
//! was already running.

use std::path::Path;

use crate::enums::DownloadStatus;
use crate::models::DownloadItem;

/// Start or continue: anything not already in flight or finished.
pub fn can_resume(item: &DownloadItem) -> bool {
    matches!(item.status, DownloadStatus::Queued | DownloadStatus::Paused | DownloadStatus::Failed | DownloadStatus::Cancelled)
}

/// Only a transfer actually in flight can be paused. `Merging` is deliberately
/// excluded — every byte is already on disk by then and the merge has no
/// resumable mid-point.
pub fn can_pause(item: &DownloadItem) -> bool {
    matches!(item.status, DownloadStatus::Connecting | DownloadStatus::Downloading)
}

/// Stop discards the partial data (it deletes the temp folder), so it is offered
/// for anything unfinished — including a paused download, where it is the
/// "discard" half of the pause/discard distinction.
pub fn can_stop(item: &DownloadItem) -> bool {
    matches!(
        item.status,
        DownloadStatus::Queued | DownloadStatus::Connecting | DownloadStatus::Downloading | DownloadStatus::Paused
    )
}

/// Retry re-resolves the remote file and starts over; only useful once a
/// download has ended badly.
pub fn can_retry(item: &DownloadItem) -> bool {
    matches!(item.status, DownloadStatus::Failed | DownloadStatus::Cancelled)
}

/// A new address can be given to anything that is not transferring and not
/// finished — the same set that can be resumed, because that is what a
/// refreshed address is for. Not while it runs: the connections are reading the
/// old address, and the run's probe describes the old server.
///
/// Not for a stream: its playlist is read afresh on every run, which is what
/// refreshing would do, and a size comparison could not vouch for a new one.
pub fn can_refresh_address(item: &DownloadItem) -> bool {
    item.stream.is_none() && can_resume(item)
}

pub fn can_open_file(item: &DownloadItem) -> bool {
    item.status == DownloadStatus::Completed && Path::new(&item.full_path()).is_file()
}

pub fn can_open_folder(item: &DownloadItem) -> bool {
    !item.save_folder.trim().is_empty() && Path::new(&item.save_folder).is_dir()
}

/// Occupying a connection slot right now — drives the status bar, the tray
/// tooltip and "Stop All".
pub fn is_active(item: &DownloadItem) -> bool {
    item.status.is_active()
}

/// A checksum can be checked against any finished file that is still where it was saved.
pub fn can_verify_checksum(item: &DownloadItem) -> bool {
    can_open_file(item)
}

#[cfg(test)]
mod tests {
    //! One set of predicates decides whether a toolbar button is greyed, whether
    //! a row's context-menu entry appears at all, and whether a tray command has
    //! anything to act on. The menus used to offer Resume on a download that was
    //! already running because there were three copies of these rules.

    use super::*;
    use crate::helpers::disk_space::scratch::TempFolder;
    use crate::models::{StreamFormat, StreamSource};
    use DownloadStatus::*;

    fn with(status: DownloadStatus) -> DownloadItem {
        DownloadItem { status, ..Default::default() }
    }

    fn check(f: fn(&DownloadItem) -> bool, cases: &[(DownloadStatus, bool)]) {
        for &(status, expected) in cases {
            assert_eq!(f(&with(status)), expected, "{status:?}");
        }
    }

    #[test]
    fn can_resume_covers_everything_not_in_flight_or_finished() {
        check(
            can_resume,
            &[
                (Queued, true),
                (Paused, true),
                (Failed, true),
                (Cancelled, true),
                (Connecting, false),
                (Downloading, false),
                (Merging, false),
                (Completed, false),
            ],
        );
    }

    #[test]
    fn can_pause_excludes_merging_which_has_no_resumable_midpoint() {
        check(
            can_pause,
            &[
                (Connecting, true),
                (Downloading, true),
                (Merging, false),
                (Queued, false),
                (Paused, false),
                (Completed, false),
                (Failed, false),
                (Cancelled, false),
            ],
        );
    }

    #[test]
    fn can_stop_is_offered_for_anything_unfinished() {
        // Including a paused download, where Stop is the discard half of the
        // pause/discard distinction.
        check(
            can_stop,
            &[
                (Queued, true),
                (Connecting, true),
                (Downloading, true),
                (Paused, true),
                (Merging, false),
                (Completed, false),
                (Failed, false),
                (Cancelled, false),
            ],
        );
    }

    #[test]
    fn can_retry_applies_only_once_a_download_has_ended_badly() {
        check(can_retry, &[(Failed, true), (Cancelled, true), (Paused, false), (Completed, false), (Downloading, false)]);
    }

    #[test]
    fn can_refresh_address_is_never_offered_while_connections_read_the_old_one() {
        check(
            can_refresh_address,
            &[
                (Queued, true),
                (Paused, true),
                (Failed, true),
                (Cancelled, true),
                (Connecting, false),
                (Downloading, false),
                (Merging, false),
                (Completed, false),
            ],
        );
    }

    #[test]
    fn can_refresh_address_is_not_offered_for_a_stream_which_reads_its_playlist_afresh_anyway() {
        let mut stream = with(Paused);
        stream.stream = Some(StreamSource {
            format: StreamFormat::Hls,
            main_playlist_url: Some("https://x.example.com/v.m3u8".into()),
            ..Default::default()
        });
        assert!(!can_refresh_address(&stream));
    }

    #[test]
    fn is_active_is_what_the_status_bar_and_tray_tooltip_count() {
        check(
            is_active,
            &[
                (Connecting, true),
                (Downloading, true),
                (Merging, true),
                (Verifying, true),
                (Queued, false),
                (Paused, false),
                (Completed, false),
            ],
        );
    }

    #[test]
    fn can_open_file_wants_a_completed_download_whose_file_is_there() {
        let folder = TempFolder::new();
        folder.write_file("done.bin", &[1]);
        let item = |status, name: &str| DownloadItem {
            status,
            save_folder: folder.path_str(),
            file_name: name.into(),
            ..Default::default()
        };

        assert!(can_open_file(&item(Completed, "done.bin")));
        assert!(!can_open_file(&item(Completed, "gone.bin")));
        assert!(!can_open_file(&item(Downloading, "done.bin")));
        assert!(can_verify_checksum(&item(Completed, "done.bin")));
    }

    #[test]
    fn can_open_folder_only_needs_the_folder_to_exist() {
        let folder = TempFolder::new();
        // True at any status — the folder is worth opening for a failed download too.
        assert!(can_open_folder(&DownloadItem { status: Failed, save_folder: folder.path_str(), ..Default::default() }));
        assert!(!can_open_folder(&DownloadItem { save_folder: folder.file("nope"), ..Default::default() }));
        assert!(!can_open_folder(&DownloadItem { save_folder: "   ".into(), ..Default::default() }));
    }
}
