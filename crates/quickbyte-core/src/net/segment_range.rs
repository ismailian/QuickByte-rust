//! Port of `src/QuickByte.Core/Services/SegmentRange.cs`.
//!
//! The byte range one connection owns, and the one rule that makes that range
//! safe to shorten while the connection is still reading into it.
//!
//! Dynamic segmentation (see the connection pool) hands the back half of a slow
//! connection's range to a new connection. That is only correct if no byte ends
//! up in two chunks: the old connection must stop exactly where the new one
//! starts, even though its response is still streaming bytes past that point.
//! So every read is *reserved* before it happens — the connection says which
//! bytes the read may write — and a shorter end is only accepted beyond
//! everything already written or reserved. A read under way is never cut in
//! half, and the next read is clamped to the new end.
//!
//! Shared by the HTTP and the FTP connection, which both loop "reserve, read,
//! write". Public so the pool (and its tests) can reason with the same rule.

use std::sync::atomic::{AtomicI64, Ordering};

use parking_lot::Mutex;

use crate::helpers::range_splitter::UNBOUNDED_END;

pub struct SegmentRange {
    start: i64,
    end: AtomicI64,
    /// The last byte written or reserved; guarded so a shrink and a reserve never interleave.
    reserved_end: Mutex<i64>,
}

impl SegmentRange {
    pub fn new(start: i64, end: i64) -> Self {
        Self { start, end: AtomicI64::new(end), reserved_end: Mutex::new(start - 1) }
    }

    pub fn start(&self) -> i64 {
        self.start
    }

    /// The last byte of the range. Can move earlier, never later.
    pub fn end(&self) -> i64 {
        self.end.load(Ordering::SeqCst)
    }

    /// A range with no known end — see [`UNBOUNDED_END`]. Never shortened.
    pub fn is_unbounded(&self) -> bool {
        self.end() == UNBOUNDED_END
    }

    pub fn len(&self) -> i64 {
        self.end() - self.start + 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() <= 0
    }

    /// How many bytes, up to `allowance`, the read that starts at `position` may
    /// write — and reserves them against a concurrent shortening. Zero once the
    /// range is done.
    pub fn reserve(&self, position: i64, allowance: usize) -> usize {
        let mut reserved = self.reserved_end.lock();
        let left = self.end() - position + 1;
        let wanted = (allowance as i64).min(left).max(0);
        *reserved = position + wanted - 1;
        wanted as usize
    }

    /// Gives back whatever a read reserved and did not write; `position` is the next byte to write.
    pub fn release(&self, position: i64) {
        *self.reserved_end.lock() = position - 1;
    }

    /// The last byte that is written or about to be: the earliest point a split may begin after.
    pub fn committed_end(&self, position: i64) -> i64 {
        (*self.reserved_end.lock()).max(position - 1)
    }

    /// Ends the range at `new_end` if nothing written or reserved lies beyond it.
    /// Returns false, changing nothing, otherwise — and always for an unbounded
    /// range, which has no length to divide.
    pub fn try_shrink_end(&self, new_end: i64, position: i64) -> bool {
        let reserved = self.reserved_end.lock();
        let end = self.end();
        if end == UNBOUNDED_END || new_end >= end || new_end < self.start {
            return false;
        }
        if new_end < (*reserved).max(position - 1) {
            return false;
        }
        self.end.store(new_end, Ordering::SeqCst);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_is_clamped_to_what_is_left() {
        let range = SegmentRange::new(0, 99);
        assert_eq!(range.reserve(90, 64), 10);
        assert_eq!(range.reserve(100, 64), 0);
    }

    #[test]
    fn a_shrink_never_cuts_into_a_reserved_read() {
        let range = SegmentRange::new(0, 99);
        assert_eq!(range.reserve(10, 30), 30); // bytes 10..=39 reserved
        assert!(!range.try_shrink_end(30, 10));
        assert!(range.try_shrink_end(39, 10));
        assert_eq!(range.end(), 39);
        range.release(40);
        assert_eq!(range.committed_end(40), 39);
    }

    #[test]
    fn an_unbounded_range_is_never_shortened() {
        let range = SegmentRange::new(0, UNBOUNDED_END);
        assert!(range.is_unbounded());
        assert!(!range.try_shrink_end(10, 0));
    }

    #[test]
    fn a_shrink_cannot_grow_or_leave_the_range() {
        let range = SegmentRange::new(50, 99);
        assert!(!range.try_shrink_end(99, 50));
        assert!(!range.try_shrink_end(120, 50));
        assert!(!range.try_shrink_end(49, 50));
    }
}
