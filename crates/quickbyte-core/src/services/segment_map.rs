//! Port of `src/QuickByte.Core/Services/SegmentMap.cs` (its twin,
//! `SegmentRange.cs`, is [`crate::net::segment_range`], shared with the
//! connections).
//!
//! Which chunk file holds which bytes of a download — persisted as
//! [`FILE_NAME`] in its temp folder.
//!
//! Resume used to be able to work this out: the split was always
//! `RangeSplitter::split` of the file into N, so `part3.tmp` could only be the
//! head of the fourth range. Dynamic segmentation breaks that — a slow range is
//! halved mid-download and its back half given a chunk of its own — so the
//! layout is written down, and resume reads it instead of re-deriving it. A
//! download without a map (every download from before this existed) is exactly
//! the fixed split, and gets a map the first time it runs.
//!
//! **Schema.** The C# record as System.Text.Json wrote it — PascalCase, compact,
//! computed properties included:
//! `{"TotalBytes":1000,"Segments":[{"Chunk":0,"Start":0,"End":499,"Length":500}],"NextChunk":1}`.
//! `Length` and `NextChunk` are written for the C# build's benefit and ignored on read.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub use crate::net::segment_range::SegmentRange;
use crate::helpers::range_splitter::Range;

pub const FILE_NAME: &str = "segments.json";

/// More chunk files than this and splitting stops: each one is a file handle and a merge step.
pub const MAX_SEGMENTS: usize = 256;

/// One chunk's bytes, `start..=end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Segment {
    pub chunk: i32,
    pub start: i64,
    pub end: i64,
}

impl Segment {
    pub const fn new(chunk: i32, start: i64, end: i64) -> Self {
        Self { chunk, start, end }
    }
    pub const fn len(&self) -> i64 {
        self.end - self.start + 1
    }
    pub const fn is_empty(&self) -> bool {
        self.len() <= 0
    }
}

impl Serialize for Segment {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("Segment", 4)?;
        s.serialize_field("Chunk", &self.chunk)?;
        s.serialize_field("Start", &self.start)?;
        s.serialize_field("End", &self.end)?;
        s.serialize_field("Length", &self.len())?;
        s.end()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SegmentMap {
    pub total_bytes: i64,
    pub segments: Vec<Segment>,
}

impl Serialize for SegmentMap {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("SegmentMap", 3)?;
        s.serialize_field("TotalBytes", &self.total_bytes)?;
        s.serialize_field("Segments", &self.segments)?;
        s.serialize_field("NextChunk", &self.next_chunk())?;
        s.end()
    }
}

impl SegmentMap {
    pub fn new(total_bytes: i64, segments: Vec<Segment>) -> Self {
        Self { total_bytes, segments }
    }

    pub fn from_ranges(total_bytes: i64, ranges: &[Range]) -> Self {
        Self {
            total_bytes,
            segments: ranges.iter().enumerate().map(|(i, r)| Segment::new(i as i32, r.start, r.end)).collect(),
        }
    }

    pub fn next_chunk(&self) -> i32 {
        self.segments.iter().map(|s| s.chunk).max().map_or(0, |m| m + 1)
    }

    /// Whether this map describes a file of `total_bytes`: every byte in exactly
    /// one segment, each chunk named once. Anything else — a server that now
    /// reports another length, a hand-edited or half-written file — is not a
    /// layout to resume from.
    pub fn describes(&self, total_bytes: i64) -> bool {
        if total_bytes <= 0 || self.total_bytes != total_bytes {
            return false;
        }
        if self.segments.is_empty() || self.segments.len() > MAX_SEGMENTS {
            return false;
        }
        let distinct: HashSet<i32> = self.segments.iter().map(|s| s.chunk).collect();
        if distinct.len() != self.segments.len() {
            return false;
        }
        if self.segments.iter().any(|s| s.chunk < 0 || s.start > s.end) {
            return false;
        }

        let mut ordered: Vec<&Segment> = self.segments.iter().collect();
        ordered.sort_by_key(|s| s.start);
        let mut next = 0;
        for segment in ordered {
            if segment.start != next {
                return false;
            }
            next = segment.end + 1;
        }
        next == total_bytes
    }

    /// The same layout with `chunk` ended at `new_end` and the rest given to `added`.
    pub fn with_split(&self, chunk: i32, new_end: i64, added: Segment) -> Self {
        let mut segments: Vec<Segment> =
            self.segments.iter().map(|s| if s.chunk == chunk { Segment { end: new_end, ..*s } } else { *s }).collect();
        segments.push(added);
        Self { total_bytes: self.total_bytes, segments }
    }

    pub fn read(temp_folder: &str) -> Option<SegmentMap> {
        // Unreadable reads as absent: the fixed split, checked against the chunks.
        let bytes = fs::read(Path::new(temp_folder).join(FILE_NAME)).ok()?;
        serde_json::from_slice(crate::serde_compat::strip_bom(&bytes)).ok()
    }

    /// Written beside the map and renamed over it, so a crash never leaves half a layout.
    pub fn write(temp_folder: &str, map: &SegmentMap) {
        let attempt = || -> std::io::Result<()> {
            fs::create_dir_all(temp_folder)?;
            let path = Path::new(temp_folder).join(FILE_NAME);
            let pending = Path::new(temp_folder).join(format!("{FILE_NAME}.pending"));
            {
                let mut file = File::create(&pending)?;
                file.write_all(&serde_json::to_vec(map).map_err(std::io::Error::other)?)?;
                file.sync_all()?;
            }
            fs::rename(&pending, &path)
        };
        // Best-effort. A map that did not land leaves the previous one, which is
        // still a correct description of every byte already written: the
        // connection that was shortened never wrote past its new end, and a chunk
        // the old map does not name is discarded rather than trusted.
        let _ = attempt();
    }

    pub fn delete(temp_folder: &str) {
        let _ = fs::remove_file(Path::new(temp_folder).join(FILE_NAME));
    }
}

#[cfg(test)]
mod tests {
    //! The two rules dynamic segmentation stands on. A connection's range may
    //! only be shortened beyond every byte it has written or reserved
    //! (`SegmentRange`), and the record of which chunk holds which bytes must
    //! describe the whole file exactly once (`SegmentMap`). Get either wrong and
    //! two chunks hold the same bytes, or none does, and the merge writes that
    //! into the file. (`SegmentLayoutTests.cs`.)

    use super::*;
    use crate::helpers::disk_space::scratch::TempFolder;
    use crate::helpers::range_splitter::UNBOUNDED_END;

    #[test]
    fn a_read_is_clamped_to_what_is_left_and_reserved() {
        let range = SegmentRange::new(100, 199);
        assert_eq!(range.reserve(100, 64), 64);
        assert_eq!(range.committed_end(100), 163);
        assert_eq!(range.reserve(164, 64), 36);
        assert_eq!(range.reserve(200, 64), 0);
    }

    #[test]
    fn a_range_cannot_be_shortened_into_a_read_under_way() {
        let range = SegmentRange::new(0, 999);
        range.reserve(100, 50); // bytes 100..149 may be arriving right now
        assert!(!range.try_shrink_end(120, 100));
        assert!(!range.try_shrink_end(148, 100));
        assert!(range.try_shrink_end(149, 100));
        assert_eq!(range.end(), 149);
    }

    #[test]
    fn a_range_cannot_be_shortened_into_bytes_already_written() {
        let range = SegmentRange::new(0, 999);
        range.release(300); // 0..299 on disk, nothing reserved
        assert!(!range.try_shrink_end(298, 300));
        assert!(range.try_shrink_end(299, 300));
    }

    #[test]
    fn a_range_only_ever_gets_shorter() {
        let range = SegmentRange::new(0, 999);
        assert!(!range.try_shrink_end(999, 0));
        assert!(!range.try_shrink_end(5000, 0));
        assert!(!range.try_shrink_end(-1, 0));
    }

    #[test]
    fn a_range_with_no_known_end_is_never_shortened() {
        let range = SegmentRange::new(0, UNBOUNDED_END);
        assert!(!range.try_shrink_end(1000, 0));
        assert!(range.is_unbounded());
    }

    #[test]
    fn the_next_read_after_a_shortening_stops_at_the_new_end() {
        let range = SegmentRange::new(0, 999);
        range.release(10);
        range.try_shrink_end(19, 10);
        assert_eq!(range.reserve(10, 64), 10);
    }

    fn map(total: i64, segments: &[(i32, i64, i64)]) -> SegmentMap {
        SegmentMap::new(total, segments.iter().map(|&(c, s, e)| Segment::new(c, s, e)).collect())
    }

    #[test]
    fn a_layout_that_covers_every_byte_once_describes_the_file_in_any_order() {
        assert!(map(100, &[(0, 0, 49), (2, 75, 99), (1, 50, 74)]).describes(100));
    }

    #[test]
    fn a_layout_for_another_length_does_not_describe_the_file() {
        for total in [101, 99] {
            assert!(!map(100, &[(0, 0, 49), (1, 50, 99)]).describes(total));
        }
    }

    #[test]
    fn a_gap_an_overlap_or_a_chunk_named_twice_is_not_a_layout() {
        assert!(!map(100, &[(0, 0, 48), (1, 50, 99)]).describes(100));
        assert!(!map(100, &[(0, 0, 50), (1, 50, 99)]).describes(100));
        assert!(!map(100, &[(0, 0, 49), (0, 50, 99)]).describes(100));
        assert!(!map(100, &[]).describes(100));
    }

    #[test]
    fn splitting_ends_one_segment_and_adds_the_rest_as_a_new_chunk() {
        let original = map(100, &[(0, 0, 49), (1, 50, 99)]);
        let split = original.with_split(1, 74, Segment::new(original.next_chunk(), 75, 99));
        assert!(split.describes(100));
        assert_eq!(split.segments.iter().find(|s| s.start == 75).unwrap().chunk, 2);
        assert_eq!(split.segments.iter().find(|s| s.chunk == 1).unwrap().end, 74);
    }

    #[test]
    fn a_layout_is_written_and_read_back_whole() {
        let folder = TempFolder::new();
        let original = map(100, &[(0, 0, 49), (3, 50, 99)]);
        SegmentMap::write(&folder.path_str(), &original);
        let read = SegmentMap::read(&folder.path_str()).unwrap();
        assert_eq!(read, original);
        assert!(!Path::new(&folder.file("segments.json.pending")).exists());
    }

    #[test]
    fn an_unreadable_layout_reads_as_none() {
        let folder = TempFolder::new();
        folder.write_file(FILE_NAME, b"{ half a layout");
        assert_eq!(SegmentMap::read(&folder.path_str()), None);
    }

    #[test]
    fn the_layout_is_written_in_the_c_sharp_schema_and_reads_what_c_sharp_wrote() {
        let folder = TempFolder::new();
        SegmentMap::write(&folder.path_str(), &map(1000, &[(0, 0, 499), (1, 500, 999)]));
        let text = fs::read_to_string(folder.file(FILE_NAME)).unwrap();
        assert_eq!(
            text,
            r#"{"TotalBytes":1000,"Segments":[{"Chunk":0,"Start":0,"End":499,"Length":500},{"Chunk":1,"Start":500,"End":999,"Length":500}],"NextChunk":2}"#
        );

        // With a BOM and the computed properties, as a C# build left it.
        let csharp = "\u{feff}{\"TotalBytes\":10,\"Segments\":[{\"Chunk\":4,\"Start\":0,\"End\":9,\"Length\":10}],\"NextChunk\":5}";
        folder.write_file(FILE_NAME, csharp.as_bytes());
        assert_eq!(SegmentMap::read(&folder.path_str()), Some(map(10, &[(4, 0, 9)])));
    }
}
