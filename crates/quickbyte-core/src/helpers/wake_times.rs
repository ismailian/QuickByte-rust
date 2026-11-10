//! Port of `WakeTimes.cs`: the moments the computer has to be woken for — the
//! start of every scheduled queue that asked to wake it — reduced to as few as
//! describe them all.
//!
//! Waking is only needed at a window's start: `QueueSchedule` is a window, and
//! once the machine is awake the scheduler finds the queue due as it would at any
//! other moment inside it. Two queues starting at the same minute share one wake,
//! on the days of both.

use crate::enums::ScheduleDays;
use crate::models::DownloadQueue;
use crate::serde_compat::{TICKS_PER_SECOND, TimeSpan};

/// A time of day, on some days of the week, at which the computer should be awake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeTime {
    pub days: ScheduleDays,
    pub at: TimeSpan,
}

/// Task Scheduler takes at most this many triggers on one task.
pub const MAX_TRIGGERS: usize = 48;

pub fn for_queues<'a>(queues: impl IntoIterator<Item = &'a DownloadQueue>) -> Vec<WakeTime> {
    let mut wakes: Vec<WakeTime> = Vec::new();
    for schedule in queues.into_iter().map(|q| &q.schedule) {
        let days = schedule.days & ScheduleDays::EVERY_DAY;
        if !schedule.enabled || !schedule.wake_computer || days.is_empty() {
            continue;
        }
        let at = normalise(schedule.start_time);
        match wakes.iter_mut().find(|w| w.at == at) {
            Some(wake) => wake.days = wake.days | days,
            None => wakes.push(WakeTime { days, at }),
        }
    }
    wakes.sort_by_key(|w| w.at);
    wakes.truncate(MAX_TRIGGERS);
    wakes
}

/// To the second, inside one day: a hand-edited queues.json can hold anything.
fn normalise(time: TimeSpan) -> TimeSpan {
    let day = TimeSpan::from_hours(24);
    let time = if time < TimeSpan::ZERO || time >= day { TimeSpan::ZERO } else { time };
    TimeSpan::from_ticks(time.ticks() - time.ticks() % TICKS_PER_SECOND)
}

#[cfg(test)]
mod tests {
    //! When the computer is woken: only for a queue that is scheduled, has days,
    //! and asked to wake it — a machine switching itself on in the night is the
    //! one thing here nobody should get without asking.

    use super::*;
    use crate::models::{DownloadSettings, QueueSchedule};

    fn queue(start: TimeSpan, days: ScheduleDays, enabled: bool, wake: bool) -> DownloadQueue {
        DownloadQueue {
            schedule: QueueSchedule { enabled, days, start_time: start, wake_computer: wake, ..Default::default() },
            ..Default::default()
        }
    }

    fn hm(h: i64, m: i64) -> TimeSpan {
        TimeSpan::from_minutes(h * 60 + m)
    }

    #[test]
    fn only_a_scheduled_queue_that_asked_to_wake_the_computer_wakes_it() {
        let queues = [
            queue(TimeSpan::from_hours(1), ScheduleDays::EVERY_DAY, true, false),
            queue(TimeSpan::from_hours(2), ScheduleDays::EVERY_DAY, false, true),
            queue(TimeSpan::from_hours(3), ScheduleDays::NONE, true, true),
            queue(TimeSpan::from_hours(4), ScheduleDays::WEEKEND, true, true),
        ];
        assert_eq!(for_queues(&queues), [WakeTime { days: ScheduleDays::WEEKEND, at: TimeSpan::from_hours(4) }]);
    }

    #[test]
    fn queues_starting_at_the_same_moment_share_a_wake_on_all_their_days() {
        let queues = [
            queue(hm(3, 30), ScheduleDays::MONDAY, true, true),
            queue(hm(1, 0), ScheduleDays::FRIDAY, true, true),
            queue(hm(3, 30) + TimeSpan::from_ticks(400 * 10_000), ScheduleDays::TUESDAY | ScheduleDays::MONDAY, true, true),
        ];
        assert_eq!(
            for_queues(&queues),
            [
                WakeTime { days: ScheduleDays::FRIDAY, at: hm(1, 0) },
                WakeTime { days: ScheduleDays::MONDAY | ScheduleDays::TUESDAY, at: hm(3, 30) },
            ]
        );
    }

    #[test]
    fn a_start_time_outside_the_day_from_a_hand_edited_file_is_read_as_midnight() {
        let wakes = for_queues(&[queue(TimeSpan::from_hours(30), ScheduleDays::EVERY_DAY, true, true)]);
        assert_eq!(wakes.len(), 1);
        assert_eq!(wakes[0].at, TimeSpan::ZERO);
    }

    #[test]
    fn no_more_wakes_than_one_task_can_hold() {
        let queues: Vec<DownloadQueue> =
            (0..100).map(|i| queue(TimeSpan::from_minutes(i * 7), ScheduleDays::EVERY_DAY, true, true)).collect();
        assert_eq!(for_queues(&queues).len(), MAX_TRIGGERS);
    }

    #[test]
    fn the_wake_choice_is_kept_with_the_schedule() {
        let schedule = QueueSchedule { enabled: true, wake_computer: true, ..Default::default() };
        assert!(schedule.clone().wake_computer);
        let json = serde_json::to_string(&schedule).unwrap();
        assert!(serde_json::from_str::<QueueSchedule>(&json).unwrap().wake_computer);
        assert!(!serde_json::from_str::<QueueSchedule>(r#"{"Enabled":true}"#).unwrap().wake_computer);
    }

    #[test]
    fn keeping_the_computer_awake_is_on_unless_turned_off_even_for_an_older_settings_file() {
        assert!(DownloadSettings::default().keep_awake_while_downloading);
        assert!(serde_json::from_str::<DownloadSettings>("{}").unwrap().keep_awake_while_downloading);
        assert!(
            !serde_json::from_str::<DownloadSettings>(r#"{"KeepAwakeWhileDownloading":false}"#)
                .unwrap()
                .keep_awake_while_downloading
        );
    }
}
