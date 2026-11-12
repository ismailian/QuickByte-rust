//! Port of `BandwidthLimiter.cs`: the token bucket behind the Speed Limiter tab,
//! the composite that stacks a download's own cap on the queue's and the
//! application's, and the no-op limiter.
//!
//! The contract ([`crate::traits::BandwidthLimiter`]) is "ask, then take what
//! you were given" rather than "read, then sleep": a connection requests
//! permission for a buffer's worth of bytes, receives an allowance that may be
//! smaller, and reads at most that much. That ordering is what lets several
//! connections share one budget without any of them overshooting it first and
//! apologising afterwards.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use crate::error::{CoreError, Result};
use crate::traits::BandwidthLimiter as BandwidthLimiterTrait;

/// How much unspent allowance the bucket may hold, expressed as seconds of
/// transfer. Small enough that a paused-then-resumed download can't dump a
/// huge burst on the link, large enough to absorb ordinary scheduling jitter.
const BURST_SECONDS: f64 = 0.25;

/// Floor on bucket capacity. Without it a very low limit would cap the bucket
/// below one read's worth of bytes and every read would be split into a stream
/// of tiny, syscall-heavy fragments.
const MINIMUM_CAPACITY_BYTES: f64 = 16.0 * 1024.0;

/// Longest single sleep while waiting for allowance. Bounded so a rate change
/// (or a cancellation) is picked up promptly rather than after a nap sized by
/// the old rate.
const MAXIMUM_WAIT_MILLISECONDS: f64 = 200.0;

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// Token-bucket limiter: allowance accrues at [`bytes_per_second`](Self::bytes_per_second)
/// and callers spend it, sleeping when the bucket runs dry.
///
/// A bucket rather than a fixed sleep-per-read because the connections of one
/// download share a single instance — the bucket lets whichever connection is
/// ready next take the available bytes, so a slow mirror on one segment doesn't
/// idle the budget the others could be using.
///
/// The rate is mutable and read fresh on every request, which is what makes the
/// Speed Limiter tab apply to a transfer already in flight instead of only to
/// the next one.
pub struct BandwidthLimiter {
    bytes_per_second: AtomicI64,
    bucket: Mutex<Bucket>,
}

impl Default for BandwidthLimiter {
    fn default() -> Self {
        Self::new(0)
    }
}

impl BandwidthLimiter {
    pub fn new(bytes_per_second: i64) -> Self {
        Self {
            bytes_per_second: AtomicI64::new(bytes_per_second.max(0)),
            bucket: Mutex::new(Bucket { tokens: 0.0, last_refill: Instant::now() }),
        }
    }

    /// The cap in bytes per second. `0` means unlimited, in which case requests
    /// short-circuit and cost nothing — the common case must not pay for the feature.
    pub fn bytes_per_second(&self) -> i64 {
        self.bytes_per_second.load(Ordering::Relaxed)
    }

    /// Sets the cap; a negative value reads as unlimited.
    pub fn set_bytes_per_second(&self, value: i64) {
        self.bytes_per_second.store(value.max(0), Ordering::Relaxed);
    }

    fn capacity(rate: i64) -> f64 {
        (rate as f64 * BURST_SECONDS).max(MINIMUM_CAPACITY_BYTES)
    }

    /// Credits the bucket for time elapsed since the last request. Capping at
    /// the capacity is what keeps a limiter that has been idle — or unlimited —
    /// from releasing a huge burst the moment a rate is set.
    fn refill(bucket: &mut Bucket, rate: i64) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(bucket.last_refill).as_secs_f64();
        bucket.last_refill = now;
        if elapsed <= 0.0 {
            return;
        }
        bucket.tokens = (bucket.tokens + elapsed * rate as f64).min(Self::capacity(rate));
    }
}

#[async_trait]
impl BandwidthLimiterTrait for BandwidthLimiter {
    async fn request(&self, desired_bytes: usize, cancel: &CancellationToken) -> Result<usize> {
        if desired_bytes == 0 {
            return Ok(0);
        }
        loop {
            if cancel.is_cancelled() {
                return Err(CoreError::Cancelled);
            }
            let rate = self.bytes_per_second();
            if rate <= 0 {
                return Ok(desired_bytes);
            }

            let wait = {
                let mut bucket = self.bucket.lock();
                Self::refill(&mut bucket, rate);
                if bucket.tokens >= 1.0 {
                    let granted = (desired_bytes as f64).min(bucket.tokens.floor()) as usize;
                    bucket.tokens -= granted as f64;
                    return Ok(granted);
                }
                // Sleep only as long as it takes to accrue the one byte that
                // gets us back into the branch above.
                let ms = ((1.0 - bucket.tokens) / rate as f64 * 1000.0).clamp(1.0, MAXIMUM_WAIT_MILLISECONDS);
                Duration::from_secs_f64(ms / 1000.0)
            };

            tokio::select! {
                _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    fn give_back(&self, unused_bytes: usize) {
        if unused_bytes == 0 {
            return;
        }
        let rate = self.bytes_per_second();
        if rate <= 0 {
            return;
        }
        let mut bucket = self.bucket.lock();
        bucket.tokens = (bucket.tokens + unused_bytes as f64).min(Self::capacity(rate));
    }
}

/// Applies several limiters at once — in practice a download's own cap plus the
/// queue's and the application-wide one — granting only what all of them allow.
///
/// The refund in `request` is the whole point: asking limiter A for 64 KB and
/// then discovering limiter B will only allow 8 KB would otherwise spend 64 KB
/// of A's budget to move 8 KB, and a per-download limit would drag the global
/// one down with it.
pub struct CompositeBandwidthLimiter {
    limiters: Vec<Arc<dyn BandwidthLimiterTrait>>,
}

impl CompositeBandwidthLimiter {
    pub fn new(limiters: Vec<Arc<dyn BandwidthLimiterTrait>>) -> Self {
        Self { limiters }
    }
}

#[async_trait]
impl BandwidthLimiterTrait for CompositeBandwidthLimiter {
    async fn request(&self, desired_bytes: usize, cancel: &CancellationToken) -> Result<usize> {
        let mut granted = desired_bytes;
        for (i, limiter) in self.limiters.iter().enumerate() {
            let allowed = match limiter.request(granted, cancel).await {
                Ok(allowed) => allowed,
                Err(e) => {
                    // Whatever the earlier limiters handed out will not be spent now.
                    for earlier in &self.limiters[..i] {
                        earlier.give_back(granted);
                    }
                    return Err(e);
                }
            };
            if allowed >= granted {
                continue;
            }
            for earlier in &self.limiters[..i] {
                earlier.give_back(granted - allowed);
            }
            granted = allowed;
        }
        Ok(granted)
    }

    fn give_back(&self, unused_bytes: usize) {
        for limiter in &self.limiters {
            limiter.give_back(unused_bytes);
        }
    }
}

/// The no-op limiter used when a connection is built without one, so the read
/// loop stays a single code path instead of branching on `None` every buffer.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnlimitedBandwidthLimiter;

impl UnlimitedBandwidthLimiter {
    /// A shared instance (`UnlimitedBandwidthLimiter.Instance`).
    pub fn shared() -> Arc<dyn BandwidthLimiterTrait> {
        static INSTANCE: std::sync::OnceLock<Arc<dyn BandwidthLimiterTrait>> = std::sync::OnceLock::new();
        INSTANCE.get_or_init(|| Arc::new(UnlimitedBandwidthLimiter)).clone()
    }
}

#[async_trait]
impl BandwidthLimiterTrait for UnlimitedBandwidthLimiter {
    async fn request(&self, desired_bytes: usize, _cancel: &CancellationToken) -> Result<usize> {
        Ok(desired_bytes)
    }

    fn give_back(&self, _unused_bytes: usize) {}
}

#[cfg(test)]
mod tests {
    //! Two invariants matter more than the arithmetic: the unlimited case must
    //! cost nothing, and unused allowance must find its way back — budget spent
    //! on bytes that never moved is what makes a download settle well below its
    //! configured rate.

    use super::*;

    fn none() -> CancellationToken {
        CancellationToken::new()
    }

    #[tokio::test]
    async fn rate_of_zero_grants_everything_asked_for() {
        let limiter = BandwidthLimiter::new(0);
        assert_eq!(limiter.request(64 * 1024, &none()).await.unwrap(), 64 * 1024);
    }

    #[tokio::test]
    async fn a_rate_never_grants_more_than_was_asked_for() {
        let limiter = BandwidthLimiter::new(10_000_000);
        let granted = limiter.request(4096, &none()).await.unwrap();
        assert!((1..=4096).contains(&granted));
    }

    #[tokio::test]
    async fn a_rate_grants_at_least_one_byte_rather_than_spinning() {
        let limiter = BandwidthLimiter::new(64);
        let granted = limiter.request(64 * 1024, &none()).await.unwrap();
        assert!(granted >= 1);
    }

    #[tokio::test]
    async fn nothing_is_granted_for_a_request_of_nothing() {
        assert_eq!(BandwidthLimiter::new(1000).request(0, &none()).await.unwrap(), 0);
    }

    #[test]
    fn negative_rates_are_read_as_unlimited() {
        assert_eq!(BandwidthLimiter::new(-5).bytes_per_second(), 0);
        let limiter = BandwidthLimiter::new(10);
        limiter.set_bytes_per_second(-1);
        assert_eq!(limiter.bytes_per_second(), 0);
    }

    #[tokio::test]
    async fn the_rate_is_read_fresh_on_every_request() {
        // This is what makes the Speed Limiter tab apply to a transfer already in
        // flight rather than only to the next one.
        let limiter = BandwidthLimiter::new(0);
        assert_eq!(limiter.request(8192, &none()).await.unwrap(), 8192);

        limiter.set_bytes_per_second(128);
        assert!(limiter.request(8192, &none()).await.unwrap() < 8192);
    }

    #[tokio::test]
    async fn returned_allowance_can_be_spent_again() {
        let limiter = BandwidthLimiter::new(100_000);
        let first = limiter.request(50_000, &none()).await.unwrap();

        limiter.give_back(first);

        // Without the refund the bucket would still be empty of what it just
        // handed out for bytes that never moved.
        assert!(limiter.request(first, &none()).await.unwrap() >= first);
    }

    #[tokio::test]
    async fn a_cancelled_request_does_not_wait_out_its_backoff() {
        // One byte a second: the bucket is dry for a very long time.
        let limiter = BandwidthLimiter::new(1);
        let _ = limiter.request(1024, &none()).await;

        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = limiter.request(1024, &cancel).await;
        assert!(matches!(result, Err(CoreError::Cancelled)));
    }

    #[tokio::test]
    async fn a_request_waiting_for_allowance_wakes_on_cancellation() {
        let limiter = Arc::new(BandwidthLimiter::new(1));
        let _ = limiter.request(1024, &none()).await;
        let cancel = CancellationToken::new();
        let waiter = {
            let limiter = limiter.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { limiter.request(1024, &cancel).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), waiter).await.unwrap().unwrap();
        assert!(matches!(result, Err(CoreError::Cancelled)));
    }

    #[tokio::test]
    async fn a_request_waiting_under_a_cap_is_released_when_the_cap_is_lifted() {
        // Unticking "Limit the maximum download speed" must free a transfer that
        // is already throttled, not just the next one.
        let limiter = Arc::new(BandwidthLimiter::new(1));
        let _ = limiter.request(1024, &none()).await;
        let waiter = {
            let limiter = limiter.clone();
            tokio::spawn(async move { limiter.request(64 * 1024, &none()).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        limiter.set_bytes_per_second(0);
        let granted = tokio::time::timeout(Duration::from_secs(1), waiter).await.expect("still throttled").unwrap().unwrap();
        assert_eq!(granted, 64 * 1024);
    }

    #[tokio::test]
    async fn unlimited_grants_whatever_it_is_asked_for() {
        let unlimited = UnlimitedBandwidthLimiter::shared();
        assert_eq!(unlimited.request(1234, &none()).await.unwrap(), 1234);
        // And has nothing to do with a refund.
        unlimited.give_back(1234);
    }

    // ------------------------------------------------------------ composite --

    struct RecordingLimiter {
        grant: usize,
        last_requested: parking_lot::Mutex<usize>,
        returned: parking_lot::Mutex<usize>,
    }

    impl RecordingLimiter {
        fn new(grant: usize) -> Arc<Self> {
            Arc::new(Self { grant, last_requested: parking_lot::Mutex::new(0), returned: parking_lot::Mutex::new(0) })
        }
        fn last_requested(&self) -> usize {
            *self.last_requested.lock()
        }
        fn returned(&self) -> usize {
            *self.returned.lock()
        }
    }

    #[async_trait]
    impl BandwidthLimiterTrait for RecordingLimiter {
        async fn request(&self, desired_bytes: usize, _cancel: &CancellationToken) -> Result<usize> {
            *self.last_requested.lock() = desired_bytes;
            Ok(desired_bytes.min(self.grant))
        }
        fn give_back(&self, unused_bytes: usize) {
            *self.returned.lock() += unused_bytes;
        }
    }

    fn composite(limiters: &[Arc<RecordingLimiter>]) -> CompositeBandwidthLimiter {
        CompositeBandwidthLimiter::new(limiters.iter().map(|l| l.clone() as Arc<dyn BandwidthLimiterTrait>).collect())
    }

    #[tokio::test]
    async fn composite_grants_only_what_the_tightest_limiter_allows() {
        let loose = RecordingLimiter::new(64 * 1024);
        let tight = RecordingLimiter::new(8 * 1024);
        let granted = composite(&[loose, tight]).request(64 * 1024, &none()).await.unwrap();
        assert_eq!(granted, 8 * 1024);
    }

    #[tokio::test]
    async fn composite_refunds_the_looser_limiter_the_difference() {
        // Spending 64 KB of the global budget to move 8 KB would drag the global
        // limit down to the per-download one.
        let global = RecordingLimiter::new(64 * 1024);
        let per_download = RecordingLimiter::new(8 * 1024);
        composite(&[global.clone(), per_download.clone()]).request(64 * 1024, &none()).await.unwrap();
        assert_eq!(global.returned(), 64 * 1024 - 8 * 1024);
        assert_eq!(per_download.returned(), 0);
    }

    #[tokio::test]
    async fn composite_asks_each_limiter_only_for_what_is_still_on_the_table() {
        let first = RecordingLimiter::new(4096);
        let second = RecordingLimiter::new(64 * 1024);
        composite(&[first.clone(), second.clone()]).request(64 * 1024, &none()).await.unwrap();
        assert_eq!(first.last_requested(), 64 * 1024);
        assert_eq!(second.last_requested(), 4096);
    }

    #[test]
    fn composite_returns_unused_allowance_to_every_limiter() {
        let first = RecordingLimiter::new(1);
        let second = RecordingLimiter::new(1);
        composite(&[first.clone(), second.clone()]).give_back(500);
        assert_eq!(first.returned(), 500);
        assert_eq!(second.returned(), 500);
    }

    #[tokio::test]
    async fn composite_of_nothing_grants_everything() {
        assert_eq!(CompositeBandwidthLimiter::new(Vec::new()).request(4096, &none()).await.unwrap(), 4096);
    }

    #[tokio::test]
    async fn a_low_rate_actually_throttles() {
        // 64 KiB/s with a 16 KiB floor on the bucket: moving 48 KiB must take
        // a measurable amount of time once the initial (empty) bucket is spent.
        let limiter = BandwidthLimiter::new(64 * 1024);
        let start = Instant::now();
        let mut moved = 0;
        while moved < 48 * 1024 {
            moved += limiter.request(8192, &none()).await.unwrap();
        }
        assert!(start.elapsed() >= Duration::from_millis(400), "{:?}", start.elapsed());
    }
}
