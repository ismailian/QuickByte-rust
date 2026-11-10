//! `DownloadQueue` and `QueueSchedule` — persisted in `queues.json`, which two
//! processes read: the app and the scheduler agent. Both ask the *same* method
//! ([`DownloadQueue::is_due`]) so they can never disagree by a minute and start
//! a queue twice.

use chrono::{Datelike, NaiveDateTime, TimeDelta};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Extra, null_as_default};
use crate::enums::{PowerAction, ScheduleDays};
use crate::serde_compat::{DateTimeOffset, TimeSpan};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct DownloadQueue {
    pub id: Uuid,
    #[serde(deserialize_with = "null_as_default")]
    pub name: String,
    pub concurrent_downloads: i32,
    pub speed_limit_bytes_per_second: i64,
    #[serde(deserialize_with = "super::lenient_power_action")]
    pub when_finished: PowerAction,
    #[serde(deserialize_with = "null_as_default")]
    pub schedule: QueueSchedule,
    /// Membership is an ordered list of download ids *on the queue*: order is
    /// half of what a queue is, and one owner means the two files cannot disagree.
    #[serde(deserialize_with = "null_as_default")]
    pub item_ids: Vec<Uuid>,
    /// Written the moment a run starts, so neither watcher starts one window twice.
    pub last_run_at: Option<DateTimeOffset>,
    pub created_at: DateTimeOffset,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for DownloadQueue {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            name: "New queue".into(),
            concurrent_downloads: 1,
            speed_limit_bytes_per_second: 0,
            when_finished: PowerAction::None,
            schedule: QueueSchedule::default(),
            item_ids: Vec::new(),
            last_run_at: None,
            created_at: DateTimeOffset::now(),
            extra: Extra::new(),
        }
    }
}

impl DownloadQueue {
    pub const MIN_CONCURRENT_DOWNLOADS: i32 = 1;
    pub const MAX_CONCURRENT_DOWNLOADS: i32 = 20;
    pub const MAX_NAME_LENGTH: usize = 60;

    /// Whether the schedule says this queue should be running now and this
    /// window has not been started yet. `now` is local wall-clock time.
    pub fn is_due(&self, now: NaiveDateTime) -> bool {
        match self.schedule.window_start(now) {
            Some(start) => self.last_run_at.is_none_or(|last| last.local_naive() < start),
            None => false,
        }
    }

    /// When this queue will next start: now's window if it is due, else the next start.
    pub fn next_run_at(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        if self.is_due(now) { self.schedule.window_start(now) } else { self.schedule.next_start(now) }
    }

    pub fn clamp_concurrency(&self) -> i32 {
        self.concurrent_downloads.clamp(Self::MIN_CONCURRENT_DOWNLOADS, Self::MAX_CONCURRENT_DOWNLOADS)
    }
}

/// When a queue runs: a window, not an instant (see the C# README, "Scheduling
/// is a window, not an instant").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct QueueSchedule {
    pub enabled: bool,
    pub days: ScheduleDays,
    pub start_time: TimeSpan,
    pub stop_at_enabled: bool,
    pub stop_time: TimeSpan,
    pub wake_computer: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for QueueSchedule {
    fn default() -> Self {
        Self {
            enabled: false,
            days: ScheduleDays::EVERY_DAY,
            start_time: TimeSpan::from_hours(2),
            stop_at_enabled: false,
            stop_time: TimeSpan::from_hours(7),
            wake_computer: false,
            extra: Extra::new(),
        }
    }
}

impl QueueSchedule {
    /// How long after its start a schedule with no stop time is still "due" —
    /// the grace that lets a run missed while asleep happen on wake.
    pub const MISSED_RUN_GRACE: TimeSpan = TimeSpan::from_hours(1);

    pub fn runs_on(&self, day: chrono::Weekday) -> bool {
        self.days.contains(ScheduleDays::from_weekday(day))
    }

    /// The start of the window `now` falls in, if any. Looks at today and
    /// yesterday, because a window may cross midnight.
    pub fn window_start(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        if !self.enabled || self.days.is_empty() {
            return None;
        }
        for days_back in 0..=1 {
            let date = now.date() - TimeDelta::days(days_back);
            if !self.runs_on(date.weekday()) {
                continue;
            }
            let start = date.and_hms_opt(0, 0, 0)? + self.start_time.to_delta();
            if now >= start && now < self.window_end(start) {
                return Some(start);
            }
        }
        None
    }

    /// Where a window that started at `start` ends: the stop time (next day if
    /// it is not after the start), or the one-hour grace.
    pub fn window_end(&self, start: NaiveDateTime) -> NaiveDateTime {
        if !self.stop_at_enabled {
            return start + Self::MISSED_RUN_GRACE.to_delta();
        }
        let mut stop = start.date().and_hms_opt(0, 0, 0).expect("midnight") + self.stop_time.to_delta();
        if stop <= start {
            stop += TimeDelta::days(1);
        }
        stop
    }

    /// The first start strictly after `after`.
    pub fn next_start(&self, after: NaiveDateTime) -> Option<NaiveDateTime> {
        if !self.enabled || self.days.is_empty() {
            return None;
        }
        // Eight days rather than seven: today's occurrence may already be behind
        // us, and the same weekday a week out is then the answer.
        for offset in 0..=7 {
            let date = after.date() + TimeDelta::days(offset);
            if !self.runs_on(date.weekday()) {
                continue;
            }
            let start = date.and_hms_opt(0, 0, 0)? + self.start_time.to_delta();
            if start > after {
                return Some(start);
            }
        }
        None
    }
}
