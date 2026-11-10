//! Port of `src/QuickByte.Core/Helpers/RangeSplitter.cs`.
//!
//! Splits a total content length into N contiguous, near-equal byte ranges
//! (each connection gets one). Remainder bytes are distributed to the first
//! ranges so every byte is covered exactly once.

use crate::error::{CoreError, Result};

/// The end of a range that has no known end — the single connection given to a
/// file whose size the server never disclosed.
///
/// It lives here, with the rest of the engine's range vocabulary, because three
/// places have to agree on it: the pool invents it, and both the HTTP and FTP
/// connections have to recognise it to know that "the stream ended" is a
/// completed segment rather than a truncated one. Spelled out as
/// `i64::MAX - 1` in each of them, it was a magic number that had to be kept in
/// step by hand.
pub const UNBOUNDED_END: i64 = i64::MAX - 1;

/// One connection's inclusive byte range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Range {
    pub start: i64,
    pub end: i64,
}

impl Range {
    pub const fn new(start: i64, end: i64) -> Self {
        Self { start, end }
    }
    pub const fn len(&self) -> i64 {
        self.end - self.start + 1
    }
    pub const fn is_empty(&self) -> bool {
        self.len() <= 0
    }
}

pub fn split(total_bytes: i64, connections_count: i32) -> Result<Vec<Range>> {
    if total_bytes <= 0 {
        return Err(CoreError::invalid(format!("totalBytes must be positive (was {total_bytes}).")));
    }
    if connections_count < 1 {
        return Err(CoreError::invalid(format!("connectionsCount must be at least 1 (was {connections_count}).")));
    }

    // Never open more connections than there are bytes to fetch.
    let count = (connections_count as i64).min(total_bytes);

    let chunk_size = total_bytes / count;
    let remainder = total_bytes % count;
    let mut ranges = Vec::with_capacity(count as usize);
    let mut start = 0;
    for i in 0..count {
        let size = chunk_size + if i < remainder { 1 } else { 0 };
        let end = start + size - 1;
        ranges.push(Range::new(start, end));
        start = end + 1;
    }
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_covers_every_byte_exactly_once() {
        for (total, connections) in [(1000, 1), (1000, 8), (1001, 8), (1007, 8), (1_000_000_007, 32), (3, 2)] {
            let ranges = split(total, connections).unwrap();
            assert_eq!(ranges[0].start, 0);
            assert_eq!(ranges.last().unwrap().end, total - 1);
            for pair in ranges.windows(2) {
                assert_eq!(pair[0].end + 1, pair[1].start);
            }
            assert_eq!(ranges.iter().map(Range::len).sum::<i64>(), total);
        }
    }

    #[test]
    fn split_distributes_the_remainder_rather_than_dropping_it() {
        let ranges = split(10, 4).unwrap();
        assert_eq!(ranges.iter().map(Range::len).collect::<Vec<_>>(), vec![3, 3, 2, 2]);
    }

    #[test]
    fn split_never_opens_more_connections_than_there_are_bytes() {
        let ranges = split(3, 8).unwrap();
        assert_eq!(ranges.len(), 3);
        assert!(ranges.iter().all(|r| r.len() == 1));
    }

    #[test]
    fn split_of_a_single_byte_is_one_range() {
        assert_eq!(split(1, 8).unwrap(), vec![Range::new(0, 0)]);
    }

    #[test]
    fn split_rejects_a_length_it_cannot_divide() {
        assert!(split(0, 4).is_err());
        assert!(split(-1, 4).is_err());
    }

    #[test]
    fn split_rejects_a_connection_count_below_one() {
        assert!(split(1000, 0).is_err());
        assert!(split(1000, -1).is_err());
    }

    #[test]
    fn unbounded_end_leaves_room_for_the_inclusive_end_to_be_incremented() {
        // Connections compute end - start + 1. i64::MAX itself would overflow
        // that; the sentinel is one below it for exactly that reason.
        assert_eq!(UNBOUNDED_END, i64::MAX - 1);
        let start = 0;
        assert_eq!(UNBOUNDED_END - start + 1, i64::MAX);
    }
}
