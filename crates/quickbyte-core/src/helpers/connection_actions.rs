//! Port of `ConnectionActions.cs`: which [`ConnectionCommand`]s make sense for
//! one connection right now — `download_actions`' counterpart for a single
//! segment. The details window greys its menu with these, and the pool refuses a
//! command with them, so the two cannot disagree about what a click would do.
//!
//! Only a download that is actually transferring has connections to command: the
//! pool exists for the length of one run, and a paused download's connections
//! are already stopped.

use crate::enums::{ConnectionCommand, ConnectionStatus, DownloadStatus};
use crate::models::{ConnectionInfo, DownloadItem};

/// Running, or waiting its turn to run.
pub fn can_pause(connection: &ConnectionInfo) -> bool {
    matches!(connection.status, ConnectionStatus::Idle | ConnectionStatus::SendingRequest | ConnectionStatus::ReceivingData)
}

/// A failed connection resumes too: it is the connection-sized Retry.
pub fn can_resume(connection: &ConnectionInfo) -> bool {
    matches!(connection.status, ConnectionStatus::Paused | ConnectionStatus::Failed)
}

/// Anything, finished included — a restart is for a chunk the user no longer trusts.
pub fn can_restart(_connection: &ConnectionInfo) -> bool {
    true
}

pub fn allows(command: ConnectionCommand, connection: &ConnectionInfo) -> bool {
    match command {
        ConnectionCommand::Pause => can_pause(connection),
        ConnectionCommand::Resume => can_resume(connection),
        ConnectionCommand::Restart => can_restart(connection),
    }
}

/// Whether the download is in the one state where its connections can be commanded at all.
pub fn accepts_commands(item: &DownloadItem) -> bool {
    item.status == DownloadStatus::Downloading
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectionStatus::*;

    fn conn(status: ConnectionStatus) -> ConnectionInfo {
        ConnectionInfo { status, ..Default::default() }
    }

    #[test]
    fn pause_is_for_a_connection_that_is_running_or_waiting_to() {
        for (status, expected) in
            [(Idle, true), (SendingRequest, true), (ReceivingData, true), (Finished, false), (Failed, false), (Paused, false)]
        {
            assert_eq!(allows(ConnectionCommand::Pause, &conn(status)), expected, "{status:?}");
        }
    }

    #[test]
    fn resume_is_for_a_paused_or_failed_connection() {
        for (status, expected) in
            [(Paused, true), (Failed, true), (Idle, false), (SendingRequest, false), (ReceivingData, false), (Finished, false)]
        {
            assert_eq!(allows(ConnectionCommand::Resume, &conn(status)), expected, "{status:?}");
        }
    }

    #[test]
    fn restart_is_always_allowed() {
        for status in [Idle, SendingRequest, ReceivingData, Finished, Failed, Paused] {
            assert!(allows(ConnectionCommand::Restart, &conn(status)));
        }
    }

    #[test]
    fn only_a_transferring_download_takes_connection_commands() {
        for status in [
            DownloadStatus::Queued,
            DownloadStatus::Connecting,
            DownloadStatus::Downloading,
            DownloadStatus::Paused,
            DownloadStatus::Merging,
            DownloadStatus::Completed,
            DownloadStatus::Failed,
            DownloadStatus::Cancelled,
            DownloadStatus::Verifying,
        ] {
            let item = DownloadItem { status, ..Default::default() };
            assert_eq!(accepts_commands(&item), status == DownloadStatus::Downloading, "{status:?}");
        }
    }
}
