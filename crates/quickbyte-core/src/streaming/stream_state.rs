//! Port of `StreamState.cs`: how far a stream download has got — persisted as
//! [`FILE_NAME`] in its temp folder, beside the pieces.
//!
//! Resume needs no record of which pieces are done: a finished piece is a file
//! under its final name, and a piece in progress is still a `.part`. What it does
//! need is the shape those names belong to. The playlist is read again on every
//! run, and a playlist whose piece count has changed is a different cut of the
//! video — pieces numbered against the old one would be joined into a file that
//! stutters or will not play. And once every piece is down, the merge deletes
//! them as it goes, so the run after an interruption must go straight to the
//! merge rather than read a playlist that may have expired and "resume" pieces
//! that were already joined.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::stream_plan::{MAX_PIECES, StreamTrack};

pub const FILE_NAME: &str = "stream.json";

/// The schema is the C# record's, as System.Text.Json wrote it: PascalCase,
/// compact, `{"PieceCounts":[…],"Downloaded":false,"MergedTracks":0}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct StreamState {
    /// Pieces per output file, in output order: the main file, then the sound if separate.
    pub piece_counts: Vec<i32>,
    /// Every piece is on disk; what is left is the merge.
    pub downloaded: bool,
    /// How many outputs are already joined and in place.
    pub merged_tracks: i32,
}

impl StreamState {
    pub fn for_tracks(tracks: &[StreamTrack]) -> Self {
        Self { piece_counts: tracks.iter().map(|t| t.pieces.len() as i32).collect(), ..Default::default() }
    }

    pub fn describes(&self, tracks: &[StreamTrack]) -> bool {
        self.piece_counts.len() == tracks.len()
            && self.piece_counts.iter().zip(tracks).all(|(&count, track)| count as usize == track.pieces.len() && count >= 0)
    }

    /// Whether a state read from disk is one this engine could have written.
    fn is_plausible(&self) -> bool {
        matches!(self.piece_counts.len(), 1 | 2)
            && self.piece_counts.iter().all(|&c| c > 0 && c as usize <= MAX_PIECES + 1)
            && self.merged_tracks >= 0
            && self.merged_tracks as usize <= self.piece_counts.len()
            && (self.downloaded || self.merged_tracks == 0)
    }

    /// The state in `temp_folder`, or `None` when there is none — or none worth
    /// believing: unreadable reads as absent, and the pieces are then checked
    /// against a fresh playlist.
    pub fn read(temp_folder: &Path) -> Option<StreamState> {
        let bytes = fs::read(temp_folder.join(FILE_NAME)).ok()?;
        let state: StreamState = serde_json::from_slice(crate::serde_compat::strip_bom(&bytes)).ok()?;
        state.is_plausible().then_some(state)
    }

    /// Written beside the state and renamed over it, so a crash never leaves half of one.
    pub fn write(temp_folder: &Path, state: &StreamState) -> io::Result<()> {
        fs::create_dir_all(temp_folder)?;
        let path = temp_folder.join(FILE_NAME);
        let pending = temp_folder.join(format!("{FILE_NAME}.pending"));
        {
            let mut file = File::create(&pending)?;
            file.write_all(&serde_json::to_vec(state).map_err(io::Error::other)?)?;
            file.sync_all()?;
        }
        fs::rename(&pending, &path)
    }

    /// Best-effort.
    pub fn delete(temp_folder: &Path) {
        let _ = fs::remove_file(temp_folder.join(FILE_NAME));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::disk_space::scratch::TempFolder;
    use crate::streaming::stream_plan::StreamPiece;

    fn track(pieces: usize) -> StreamTrack {
        let uri = url::Url::parse("https://x.example.com/p").unwrap();
        StreamTrack {
            pieces: (0..pieces).map(|_| StreamPiece { uri: uri.clone(), range: None, key: None, is_initialization: false }).collect(),
        }
    }

    #[test]
    fn a_state_round_trips_in_the_c_sharp_schema() {
        let folder = TempFolder::new();
        let state = StreamState { piece_counts: vec![12, 3], downloaded: true, merged_tracks: 1 };
        StreamState::write(folder.path(), &state).unwrap();

        let text = fs::read_to_string(folder.path().join(FILE_NAME)).unwrap();
        assert_eq!(text, r#"{"PieceCounts":[12,3],"Downloaded":true,"MergedTracks":1}"#);
        assert_eq!(StreamState::read(folder.path()), Some(state));
        assert!(!folder.path().join("stream.json.pending").exists());

        StreamState::delete(folder.path());
        assert_eq!(StreamState::read(folder.path()), None);
    }

    #[test]
    fn a_state_describes_tracks_with_the_same_piece_counts() {
        let tracks = [track(3), track(2)];
        let state = StreamState::for_tracks(&tracks);
        assert_eq!(state.piece_counts, [3, 2]);
        assert!(state.describes(&tracks));
        assert!(!state.describes(&[track(3)]));
        assert!(!state.describes(&[track(4), track(2)]));
    }

    #[test]
    fn a_state_this_engine_could_not_have_written_reads_as_absent() {
        let folder = TempFolder::new();
        for json in [
            r#"{"PieceCounts":[],"Downloaded":false,"MergedTracks":0}"#,
            r#"{"PieceCounts":[1,2,3]}"#,
            r#"{"PieceCounts":[0]}"#,
            r#"{"PieceCounts":[5],"MergedTracks":1}"#,
            r#"{"PieceCounts":[5],"Downloaded":true,"MergedTracks":2}"#,
            r#"{"PieceCounts":[5],"MergedTracks":-1,"Downloaded":true}"#,
            "not json",
        ] {
            folder.write_file(FILE_NAME, json.as_bytes());
            assert_eq!(StreamState::read(folder.path()), None, "{json}");
        }
        folder.write_file(FILE_NAME, br#"{"PieceCounts":[5],"Downloaded":true,"MergedTracks":1}"#);
        assert!(StreamState::read(folder.path()).is_some());
    }
}
