//! Port of `src/QuickByte.Core/Helpers/SpeedCalculator.cs`.
//!
//! Computes a smoothed transfer speed from a rolling window of
//! (timestamp, total bytes) samples, avoiding the jitter you'd get from naively
//! dividing instantaneous delta-bytes by delta-time. Not tied to any specific
//! download — one instance per connection pool.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::serde_compat::{TICKS_PER_SECOND, TimeSpan};

pub struct SpeedCalculator {
    samples: Mutex<VecDeque<(Instant, i64)>>,
    window: Duration,
}

impl Default for SpeedCalculator {
    fn default() -> Self {
        Self::new(None)
    }
}

impl SpeedCalculator {
    /// `window` defaults to five seconds.
    pub fn new(window: Option<Duration>) -> Self {
        Self { samples: Mutex::new(VecDeque::new()), window: window.unwrap_or(Duration::from_secs(5)) }
    }

    pub fn add_sample(&self, total_bytes_downloaded: i64) {
        let now = Instant::now();
        let mut samples = self.samples.lock();
        samples.push_back((now, total_bytes_downloaded));
        while samples.front().is_some_and(|(at, _)| now.duration_since(*at) > self.window) {
            samples.pop_front();
        }
    }

    /// Bytes per second, smoothed over the configured window.
    pub fn speed_bytes_per_second(&self) -> f64 {
        let samples = self.samples.lock();
        if samples.len() < 2 {
            return 0.0;
        }
        let (first_at, first_bytes) = samples[0];
        let (last_at, last_bytes) = samples[samples.len() - 1];

        let seconds = last_at.duration_since(first_at).as_secs_f64();
        if seconds <= 0.0 {
            return 0.0;
        }
        let delta = last_bytes - first_bytes;
        if delta <= 0 { 0.0 } else { delta as f64 / seconds }
    }

    /// Time left at `speed_bytes_per_second`, or `None` when there is no rate or
    /// no total to measure against — or the answer would not fit a `TimeSpan`.
    pub fn estimate_time_remaining(downloaded: i64, total: i64, speed_bytes_per_second: f64) -> Option<TimeSpan> {
        if speed_bytes_per_second <= 0.01 || total <= 0 {
            return None;
        }
        let remaining = (total - downloaded).max(0);
        let seconds_left = remaining as f64 / speed_bytes_per_second;
        let max_seconds = i64::MAX as f64 / TICKS_PER_SECOND as f64;
        if seconds_left.is_infinite() || seconds_left > max_seconds {
            return None;
        }
        Some(TimeSpan::from_secs_f64(seconds_left))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_sample_is_not_a_speed() {
        assert_eq!(SpeedCalculator::default().speed_bytes_per_second(), 0.0);
        let calculator = SpeedCalculator::default();
        calculator.add_sample(1000);
        assert_eq!(calculator.speed_bytes_per_second(), 0.0);
    }

    #[test]
    fn two_samples_give_a_positive_rate() {
        let calculator = SpeedCalculator::default();
        calculator.add_sample(0);
        std::thread::sleep(Duration::from_millis(60));
        calculator.add_sample(60_000);
        assert!(calculator.speed_bytes_per_second() > 0.0);
    }

    #[test]
    fn a_download_that_stopped_moving_reports_no_speed() {
        let calculator = SpeedCalculator::default();
        calculator.add_sample(5000);
        std::thread::sleep(Duration::from_millis(30));
        calculator.add_sample(5000);
        assert_eq!(calculator.speed_bytes_per_second(), 0.0);
    }

    #[test]
    fn samples_older_than_the_window_are_dropped() {
        let calculator = SpeedCalculator::new(Some(Duration::from_millis(50)));
        calculator.add_sample(0);
        std::thread::sleep(Duration::from_millis(120));
        // The old sample falls out of the window as this one arrives, leaving one
        // sample and therefore no rate — rather than averaging over the gap.
        calculator.add_sample(1_000_000);
        assert_eq!(calculator.speed_bytes_per_second(), 0.0);
    }

    #[test]
    fn estimate_declines_to_guess_without_both_numbers() {
        for (downloaded, total, speed) in [(0, 1000, 100.0), (500, 1000, 0.0), (500, 1000, -1.0), (500, 0, 100.0), (500, -1, 100.0)] {
            let eta = SpeedCalculator::estimate_time_remaining(downloaded, total, speed);
            assert_eq!(eta.is_none(), speed <= 0.01 || total <= 0);
        }
    }

    #[test]
    fn estimate_divides_what_is_left_by_the_rate() {
        assert_eq!(SpeedCalculator::estimate_time_remaining(500, 1500, 100.0), Some(TimeSpan::from_seconds(10)));
    }

    #[test]
    fn estimate_of_an_over_counted_download_is_zero() {
        assert_eq!(SpeedCalculator::estimate_time_remaining(2000, 1000, 100.0), Some(TimeSpan::ZERO));
    }

    #[test]
    fn estimate_refuses_an_answer_it_cannot_represent() {
        assert_eq!(SpeedCalculator::estimate_time_remaining(0, i64::MAX, 0.02), None);
    }
}
