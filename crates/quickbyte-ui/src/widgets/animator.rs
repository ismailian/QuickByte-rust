//! Eases displayed progress values toward the last value the download engine
//! reported — port of `Controls/ProgressAnimator.cs` as a generic `Animator<K>`,
//! keyed by whatever identifies the thing being drawn (download id, connection id).
//!
//! Progress samples land every ~100 ms; painting them raw makes the bars step in
//! visible jumps. Every UI frame each displayed value moves a fraction of the way
//! to its target, so progress that really arrives in discrete samples reads as
//! continuous motion. Values only move backwards on an explicit reset or a large
//! jump (a retry), never on ordinary jitter.
//!
//! egui frames do not arrive on a fixed timer, so [`Animator::advance_by`] scales
//! the easing to the time that actually passed: a 32 ms frame covers what two
//! 16 ms frames would have. [`Animator::advance`] is exactly one 16 ms frame.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::Duration;

/// Frame interval the easing is tuned for (~60 fps).
pub const FRAME_INTERVAL_MS: u64 = 16;
/// Fraction of the remaining distance covered per frame.
pub const EASING: f64 = 0.22;
/// Below this gap a value snaps — sub-pixel motion isn't worth a repaint.
pub const SNAP_THRESHOLD: f64 = 0.05;

pub fn frame_interval() -> Duration {
    Duration::from_millis(FRAME_INTERVAL_MS)
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    displayed: f64,
    target: f64,
}

#[derive(Clone, Debug)]
pub struct Animator<K> {
    entries: HashMap<K, Entry>,
    max: f64,
    snap: f64,
    /// A drop larger than this is a reset (a retry), not jitter: snap rather than rewind.
    reset_gap: f64,
}

impl<K: Eq + Hash + Clone> Default for Animator<K> {
    fn default() -> Self {
        Self::percentages()
    }
}

impl<K: Eq + Hash + Clone> Animator<K> {
    /// Values 0–100, snapping under 0.05 and treating a 5-point drop as a reset.
    pub fn percentages() -> Self {
        Self {
            entries: HashMap::new(),
            max: 100.0,
            snap: SNAP_THRESHOLD,
            reset_gap: 5.0,
        }
    }

    /// Values 0–1 (the segments bar), with the C# bar's own thresholds.
    pub fn fractions() -> Self {
        Self {
            entries: HashMap::new(),
            max: 1.0,
            snap: 0.0005,
            reset_gap: 0.05,
        }
    }

    pub fn set_target(&mut self, key: K, value: f64, immediate: bool) {
        let value = if value.is_nan() {
            0.0
        } else {
            value.clamp(0.0, self.max)
        };
        match self.entries.get_mut(&key) {
            // Nothing animates up from zero when a download is added at 40% —
            // that is a resumed download, not progress being made.
            None => {
                self.entries.insert(
                    key,
                    Entry {
                        displayed: value,
                        target: value,
                    },
                );
            }
            Some(entry) => {
                entry.target = value;
                // A large backwards jump means the download was reset/retried,
                // not that progress regressed — snap instead of animating in reverse.
                if immediate || value < entry.displayed - self.reset_gap {
                    entry.displayed = value;
                }
            }
        }
    }

    pub fn displayed(&self, key: &K) -> f64 {
        self.entries.get(key).map_or(0.0, |e| e.displayed)
    }

    pub fn displayed_or(&self, key: &K, fallback: f64) -> f64 {
        self.entries.get(key).map_or(fallback, |e| e.displayed)
    }

    pub fn contains(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    pub fn remove(&mut self, key: &K) {
        self.entries.remove(key);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Keeps only the keys `keep` says to (rows that still exist).
    pub fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        self.entries.retain(|k, _| keep(k));
    }

    /// Whether this key's value still has distance to cover.
    pub fn is_key_animating(&self, key: &K) -> bool {
        self.entries
            .get(key)
            .is_some_and(|e| e.displayed != e.target)
    }

    /// Whether any value still has distance to cover — the caller keeps
    /// requesting frames while this is true, and stops when it is not.
    pub fn is_animating(&self) -> bool {
        self.entries.values().any(|e| e.displayed != e.target)
    }

    /// Advances every value one 16 ms frame. Returns the keys that actually moved.
    pub fn advance(&mut self) -> Vec<K> {
        self.step(EASING)
    }

    /// Advances by however much time really passed. Returns whether anything moved.
    pub fn advance_by(&mut self, elapsed: Duration) -> bool {
        let frames = (elapsed.as_secs_f64() * 1000.0 / FRAME_INTERVAL_MS as f64).clamp(0.0, 30.0);
        if frames <= 0.0 {
            return false;
        }
        let fraction = 1.0 - (1.0 - EASING).powf(frames);
        !self.step(fraction).is_empty()
    }

    fn step(&mut self, fraction: f64) -> Vec<K> {
        let mut moved = Vec::new();
        for (key, entry) in &mut self.entries {
            let gap = entry.target - entry.displayed;
            if gap.abs() <= self.snap {
                if entry.displayed == entry.target {
                    continue;
                }
                entry.displayed = entry.target;
            } else {
                entry.displayed += gap * fraction;
            }
            moved.push(key.clone());
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    //! Port of `ProgressAnimatorTests.cs`. The interesting behaviour is at the
    //! edges: a value that has arrived must stop moving (or the repaints never
    //! stop), and a download that was retried has to snap back rather than
    //! animate backwards for a second and a half.

    use super::*;

    fn animator() -> Animator<&'static str> {
        Animator::percentages()
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn a_value_seen_for_the_first_time_is_displayed_as_it_is() {
        let mut a = animator();
        a.set_target("a", 40.0, false);
        assert_eq!(a.displayed(&"a"), 40.0);
    }

    #[test]
    fn a_key_nobody_has_reported_is_at_zero() {
        assert_eq!(animator().displayed(&"never seen"), 0.0);
    }

    #[test]
    fn a_new_target_is_approached_a_fraction_at_a_time() {
        let mut a = animator();
        a.set_target("a", 0.0, false);
        a.set_target("a", 100.0, false);
        a.advance();
        assert!(approx(a.displayed(&"a"), 100.0 * EASING));
        assert!(a.displayed(&"a") < 100.0);
    }

    #[test]
    fn enough_frames_arrive_at_the_target_and_then_stop() {
        let mut a = animator();
        a.set_target("a", 0.0, false);
        a.set_target("a", 100.0, false);
        for _ in 0..200 {
            a.advance();
        }
        assert_eq!(a.displayed(&"a"), 100.0);
        // Nothing moved on the last frame, so the caller can stop repainting.
        assert!(a.advance().is_empty());
        assert!(!a.is_animating());
    }

    #[test]
    fn advance_names_only_the_keys_that_moved() {
        let mut a = animator();
        a.set_target("still", 50.0, false);
        a.set_target("moving", 0.0, false);
        a.set_target("moving", 90.0, false);
        assert_eq!(a.advance(), vec!["moving"]);
    }

    #[test]
    fn a_gap_too_small_to_see_snaps_shut() {
        let mut a = animator();
        a.set_target("a", 50.0, false);
        a.set_target("a", 50.0 + SNAP_THRESHOLD / 2.0, false);
        a.advance();
        assert_eq!(a.displayed(&"a"), 50.0 + SNAP_THRESHOLD / 2.0);
        assert!(a.advance().is_empty());
    }

    #[test]
    fn a_retry_snaps_back_instead_of_animating_in_reverse() {
        let mut a = animator();
        a.set_target("a", 80.0, false);
        a.set_target("a", 0.0, false);
        assert_eq!(a.displayed(&"a"), 0.0);
    }

    #[test]
    fn a_small_regression_is_eased_rather_than_snapped() {
        let mut a = animator();
        a.set_target("a", 80.0, false);
        a.set_target("a", 78.0, false);
        assert_eq!(a.displayed(&"a"), 80.0);
        a.advance();
        let d = a.displayed(&"a");
        assert!((78.0..=80.0).contains(&d));
    }

    #[test]
    fn an_immediate_target_skips_the_animation() {
        let mut a = animator();
        a.set_target("a", 0.0, false);
        a.set_target("a", 100.0, true);
        assert_eq!(a.displayed(&"a"), 100.0);
    }

    #[test]
    fn a_percentage_is_clamped_to_a_percentage() {
        for (given, expected) in [(-10.0, 0.0), (150.0, 100.0), (0.0, 0.0), (100.0, 100.0)] {
            let mut a = animator();
            a.set_target("a", given, false);
            assert_eq!(a.displayed(&"a"), expected);
        }
    }

    #[test]
    fn a_removed_key_is_forgotten() {
        let mut a = animator();
        a.set_target("a", 40.0, false);
        a.remove(&"a");
        assert_eq!(a.displayed(&"a"), 0.0);
        assert!(a.advance().is_empty());
    }

    #[test]
    fn clear_forgets_everything() {
        let mut a = animator();
        a.set_target("a", 40.0, false);
        a.set_target("b", 60.0, false);
        a.clear();
        assert_eq!(a.displayed(&"a"), 0.0);
        assert_eq!(a.displayed(&"b"), 0.0);
    }

    #[test]
    fn every_key_animates_on_its_own() {
        let mut a = animator();
        a.set_target("a", 0.0, false);
        a.set_target("b", 0.0, false);
        a.set_target("a", 100.0, false);
        a.set_target("b", 50.0, false);
        a.advance();
        assert!(approx(a.displayed(&"a"), 100.0 * EASING));
        assert!(approx(a.displayed(&"b"), 50.0 * EASING));
    }

    #[test]
    fn the_frame_interval_is_about_sixty_a_second() {
        let fps = 1000.0 / FRAME_INTERVAL_MS as f64;
        assert!((55.0..=65.0).contains(&fps));
    }

    #[test]
    fn two_short_frames_cover_what_one_long_frame_does() {
        let mut a = animator();
        a.set_target("a", 0.0, false);
        a.set_target("a", 100.0, false);
        let mut b = a.clone();
        a.advance();
        a.advance();
        b.advance_by(Duration::from_millis(2 * FRAME_INTERVAL_MS));
        assert!(approx(a.displayed(&"a"), b.displayed(&"a")));
    }

    #[test]
    fn fractions_use_their_own_scale() {
        let mut a: Animator<i32> = Animator::fractions();
        a.set_target(1, 0.9, false);
        a.set_target(1, 2.0, false);
        assert!(a.displayed(&1) <= 1.0);
        a.set_target(1, 0.5, false); // a restarted connection snaps back
        assert_eq!(a.displayed(&1), 0.5);
    }
}
