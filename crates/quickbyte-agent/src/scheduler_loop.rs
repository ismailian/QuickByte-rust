//! Port of `src/QuickByte.Agent/SchedulerLoop.cs`.
//!
//! The agent's whole behaviour: wake up, ask each scheduled queue whether it is
//! due, and launch QuickByte for the ones that are.
//!
//! "Due" is [`DownloadQueue::is_due`] — Core's answer, not a second
//! implementation of it here. That matters more than it looks: the app is
//! asking the same question of the same file at the same time, and two
//! almost-identical schedule implementations would eventually disagree by a
//! minute and start a queue twice.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::NaiveDateTime;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use quickbyte_core::models::DownloadQueue;
use quickbyte_core::traits::QueueRepository;
use uuid::Uuid;

use crate::agent_log::AgentLog;
use crate::quickbyte_app::AppLauncher;

/// How often the clock is checked. Schedules have minute resolution, so this
/// only needs to be under a minute; it also bounds how long the agent takes to
/// notice a queue's schedule was switched off, and how late a run can be after
/// the machine wakes from sleep.
pub const TICK_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TickOutcome {
    /// There are scheduled queues; keep watching.
    Waiting,
    /// The queue list was read and no queue is scheduled — the agent has no reason to exist.
    NothingToWatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunEnd {
    /// No queue has a schedule left.
    NothingToWatch,
    /// The session is ending (the C# loop's `OperationCanceledException`).
    Cancelled,
}

pub struct SchedulerLoop {
    repository: Arc<dyn QueueRepository>,
    app: Arc<dyn AppLauncher>,
    log: Arc<AgentLog>,
    /// The clock, as a function rather than a bare `Local::now()` in the middle
    /// of the pass — the same reason every `QueueSchedule` method takes `now` as
    /// a parameter. "Is this queue due?" is only answerable against a stated
    /// time, and only checkable against one that stands still.
    clock: Box<dyn Fn() -> NaiveDateTime + Send>,
    /// The window start each queue was last launched for. Belt to
    /// `DownloadQueue::last_run_at`'s braces: the app records the run in
    /// queues.json, but only once it has actually started, and this stops the
    /// agent from launching a second time in the seconds before that lands — or
    /// at all, if the launch failed to take.
    launched: HashMap<Uuid, NaiveDateTime>,
    logged_unreadable: bool,
}

impl SchedulerLoop {
    pub fn new(
        repository: Arc<dyn QueueRepository>,
        app: Arc<dyn AppLauncher>,
        log: Arc<AgentLog>,
        clock: Option<Box<dyn Fn() -> NaiveDateTime + Send>>,
    ) -> Self {
        Self {
            repository,
            app,
            log,
            clock: clock.unwrap_or_else(|| Box::new(|| chrono::Local::now().naive_local())),
            launched: HashMap::new(),
            logged_unreadable: false,
        }
    }

    /// Ticks every [`TICK_INTERVAL`] until nothing is scheduled, or `cancel`
    /// fires (a message, or every sender gone).
    pub fn run(&mut self, cancel: &Receiver<()>) -> RunEnd {
        self.run_every(TICK_INTERVAL, cancel)
    }

    pub(crate) fn run_every(&mut self, interval: Duration, cancel: &Receiver<()>) -> RunEnd {
        self.log.write("scheduler started");
        loop {
            if self.evaluate_once() == TickOutcome::NothingToWatch {
                self.log.write("no queue has a schedule — scheduler exiting");
                return RunEnd::NothingToWatch;
            }
            match cancel.recv_timeout(interval) {
                Err(RecvTimeoutError::Timeout) => {}
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return RunEnd::Cancelled,
            }
        }
    }

    /// One pass over the queue list. Public so `--once` can run exactly one.
    pub fn evaluate_once(&mut self) -> TickOutcome {
        let Some(queues) = self.repository.try_load_all() else {
            // Unreadable is not the same as empty, and must never be treated as
            // "nothing to watch": the file is being written by the app for a
            // fraction of a second at a time, and exiting on that would leave the
            // user with a scheduler that quietly disappeared.
            if !self.logged_unreadable {
                self.log.write("queues.json could not be read — retrying");
                self.logged_unreadable = true;
            }
            return TickOutcome::Waiting;
        };
        self.logged_unreadable = false;

        let scheduled: Vec<DownloadQueue> = queues.into_iter().filter(|q| q.schedule.enabled).collect();
        if scheduled.is_empty() {
            return TickOutcome::NothingToWatch;
        }

        // Forget queues that are no longer scheduled, so the guard cannot grow
        // without bound over a long-running session.
        self.launched.retain(|id, _| scheduled.iter().any(|q| q.id == *id));

        let now = (self.clock)();
        for queue in &scheduled {
            if !queue.is_due(now) {
                continue;
            }
            let Some(window_start) = queue.schedule.window_start(now) else { continue };
            if self.launched.get(&queue.id) == Some(&window_start) {
                continue;
            }

            if self.app.is_running() {
                // QuickByte is up, and it runs the same schedule check itself.
                // Launching a second process would only be handed straight back
                // to that one, so the agent stands down and lets it happen there.
                self.log.write(&format!(
                    "'{}' is due at {} — QuickByte is running and will start it",
                    queue.name,
                    general_short(window_start)
                ));
                continue;
            }

            match self.app.try_launch_for_queue(queue.id) {
                Ok(()) => {
                    self.launched.insert(queue.id, window_start);
                    self.log.write(&format!("started QuickByte for queue '{}' (scheduled {})", queue.name, general_short(window_start)));
                }
                Err(error) => self.log.write(&format!("could not start QuickByte for queue '{}': {error}", queue.name)),
            }
        }

        TickOutcome::Waiting
    }
}

/// .NET's `{0:g}` (general date, short time) in en-US: `8/26/2026 2:00 AM`.
fn general_short(at: NaiveDateTime) -> String {
    at.format("%-m/%-d/%Y %-I:%M %p").to_string()
}

#[cfg(test)]
mod tests {
    //! Port of `tests/QuickByte.Agent.Tests/SchedulerLoopTests.cs`. Every test
    //! fixes the clock rather than reading it, and nothing starts a process.

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use chrono::{NaiveDate, TimeDelta};
    use quickbyte_core::enums::ScheduleDays;
    use quickbyte_core::models::QueueSchedule;
    use quickbyte_core::serde_compat::{DateTimeOffset, TimeSpan};

    use super::*;
    use crate::agent_log::tests::LogFile;

    /// 2026-08-26 is a Wednesday — the same anchor the Core suite's schedule tests use.
    fn wednesday() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 8, 26).unwrap().and_hms_opt(0, 0, 0).unwrap()
    }

    fn two_am() -> TimeSpan {
        TimeSpan::from_hours(2)
    }

    #[derive(Default)]
    struct FakeQueueRepository {
        queues: Mutex<Vec<DownloadQueue>>,
        /// Whether queues.json can be read this pass. False is the app mid-write.
        unreadable: AtomicBool,
        /// Runs inside the read, for a test that needs to interrupt the loop.
        on_load: Mutex<Option<Box<dyn Fn() + Send>>>,
    }

    impl QueueRepository for FakeQueueRepository {
        fn load_all(&self) -> Vec<DownloadQueue> {
            self.queues.lock().unwrap().clone()
        }
        fn try_load_all(&self) -> Option<Vec<DownloadQueue>> {
            if let Some(f) = self.on_load.lock().unwrap().as_ref() {
                f();
            }
            // Clones, like the real repository: the loop must not be able to
            // edit what the next pass reads.
            (!self.unreadable.load(Ordering::SeqCst)).then(|| self.queues.lock().unwrap().clone())
        }
        fn save_all(&self, _queues: &[DownloadQueue]) {
            panic!("the agent must never write queues.json — the app is its only writer");
        }
    }

    #[derive(Default)]
    struct FakeLauncher {
        running: AtomicBool,
        /// Some makes the launch fail with this message, as a refused start would.
        fails_with: Mutex<Option<String>>,
        launched: Mutex<Vec<Uuid>>,
    }

    impl AppLauncher for FakeLauncher {
        fn is_running(&self) -> bool {
            self.running.load(Ordering::SeqCst)
        }
        fn try_launch_for_queue(&self, queue_id: Uuid) -> Result<(), String> {
            if let Some(error) = self.fails_with.lock().unwrap().clone() {
                return Err(error);
            }
            self.launched.lock().unwrap().push(queue_id);
            Ok(())
        }
    }

    struct Fixture {
        repository: Arc<FakeQueueRepository>,
        app: Arc<FakeLauncher>,
        log: LogFile,
        /// Half an hour into a 02:00 window, unless a test moves it.
        now: Arc<Mutex<NaiveDateTime>>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                repository: Arc::default(),
                app: Arc::default(),
                log: LogFile::new(),
                now: Arc::new(Mutex::new(wednesday() + TimeDelta::hours(2) + TimeDelta::minutes(30))),
            }
        }

        fn scheduler(&self) -> SchedulerLoop {
            let now = self.now.clone();
            SchedulerLoop::new(self.repository.clone(), self.app.clone(), self.log.log.clone(), Some(Box::new(move || *now.lock().unwrap())))
        }

        fn scheduled(&self, name: &str, start_time: TimeSpan) -> Uuid {
            let queue = DownloadQueue {
                name: name.into(),
                schedule: QueueSchedule { enabled: true, days: ScheduleDays::EVERY_DAY, start_time, ..QueueSchedule::default() },
                ..DownloadQueue::default()
            };
            let id = queue.id;
            self.repository.queues.lock().unwrap().push(queue);
            id
        }

        fn nightly(&self) -> Uuid {
            self.scheduled("Nightly", two_am())
        }

        fn edit(&self, id: Uuid, f: impl FnOnce(&mut DownloadQueue)) {
            let mut queues = self.repository.queues.lock().unwrap();
            f(queues.iter_mut().find(|q| q.id == id).unwrap());
        }

        fn set_readable(&self, readable: bool) {
            self.repository.unreadable.store(!readable, Ordering::SeqCst);
        }

        fn advance(&self, by: TimeDelta) {
            *self.now.lock().unwrap() += by;
        }

        fn launched(&self) -> Vec<Uuid> {
            self.app.launched.lock().unwrap().clone()
        }
    }

    // ------------------------------------------------- reading the file --

    #[test]
    fn an_unreadable_queue_file_is_not_an_empty_one() {
        let f = Fixture::new();
        f.set_readable(false);

        assert_eq!(f.scheduler().evaluate_once(), TickOutcome::Waiting);
        assert!(f.launched().is_empty());
    }

    #[test]
    fn an_unreadable_queue_file_is_complained_about_once() {
        let f = Fixture::new();
        f.set_readable(false);
        let mut scheduler = f.scheduler();

        scheduler.evaluate_once();
        scheduler.evaluate_once();
        scheduler.evaluate_once();

        assert_eq!(f.log.count("could not be read"), 1);
    }

    #[test]
    fn an_outage_that_comes_back_and_returns_is_complained_about_again() {
        let f = Fixture::new();
        f.nightly();
        let mut scheduler = f.scheduler();

        f.set_readable(false);
        scheduler.evaluate_once();
        f.set_readable(true);
        scheduler.evaluate_once();
        f.set_readable(false);
        scheduler.evaluate_once();

        assert_eq!(f.log.count("could not be read"), 2);
    }

    #[test]
    fn an_empty_queue_file_leaves_nothing_to_watch() {
        let f = Fixture::new();
        assert_eq!(f.scheduler().evaluate_once(), TickOutcome::NothingToWatch);
    }

    #[test]
    fn a_queue_whose_schedule_is_switched_off_is_nothing_to_watch() {
        let f = Fixture::new();
        let id = f.nightly();
        f.edit(id, |q| q.schedule.enabled = false);

        assert_eq!(f.scheduler().evaluate_once(), TickOutcome::NothingToWatch);
    }

    #[test]
    fn the_agent_never_writes_the_queue_file() {
        // The fake panics if save_all is ever called, so a pass that starts a
        // queue proves it here.
        let f = Fixture::new();
        f.nightly();

        assert_eq!(f.scheduler().evaluate_once(), TickOutcome::Waiting);
        assert_eq!(f.launched().len(), 1);
    }

    // --------------------------------------------------- deciding to run --

    #[test]
    fn a_queue_whose_time_has_not_come_is_left_alone() {
        let f = Fixture::new();
        f.nightly();
        *f.now.lock().unwrap() = wednesday() + TimeDelta::hours(1);

        assert_eq!(f.scheduler().evaluate_once(), TickOutcome::Waiting);
        assert!(f.launched().is_empty());
    }

    #[test]
    fn a_due_queue_starts_quickbyte() {
        let f = Fixture::new();
        let id = f.nightly();

        f.scheduler().evaluate_once();

        assert_eq!(f.launched(), vec![id]);
        assert_eq!(f.log.count("started QuickByte for queue 'Nightly'"), 1);
        assert_eq!(f.log.count("(scheduled 8/26/2026 2:00 AM)"), 1);
    }

    #[test]
    fn only_the_queue_that_is_due_is_started() {
        let f = Fixture::new();
        let nightly = f.nightly();
        f.scheduled("Evening", TimeSpan::from_hours(23));

        f.scheduler().evaluate_once();

        assert_eq!(f.launched(), vec![nightly]);
    }

    #[test]
    fn two_queues_due_at_once_each_get_their_own_launch() {
        let f = Fixture::new();
        let first = f.scheduled("Nightly", two_am());
        let second = f.scheduled("Also nightly", two_am());

        f.scheduler().evaluate_once();

        assert_eq!(f.launched(), vec![first, second]);
    }

    #[test]
    fn a_queue_the_app_already_ran_is_left_alone() {
        // is_due is Core's answer, and last_run_at is how the app records that
        // the window has been served. The agent must defer to it.
        let f = Fixture::new();
        let id = f.nightly();
        let ran_at = (wednesday() + TimeDelta::hours(2)).and_local_timezone(chrono::Local).single().unwrap();
        f.edit(id, |q| q.last_run_at = Some(DateTimeOffset(ran_at.fixed_offset())));

        f.scheduler().evaluate_once();

        assert!(f.launched().is_empty());
    }

    // ---------------------------------------------- not starting it twice --

    #[test]
    fn a_queue_is_started_once_for_a_window() {
        let f = Fixture::new();
        f.nightly();
        let mut scheduler = f.scheduler();

        scheduler.evaluate_once();
        f.advance(TimeDelta::minutes(1));
        scheduler.evaluate_once();

        assert_eq!(f.launched().len(), 1);
    }

    #[test]
    fn the_next_window_starts_it_again() {
        let f = Fixture::new();
        let id = f.nightly();
        let mut scheduler = f.scheduler();

        scheduler.evaluate_once();
        f.advance(TimeDelta::days(1));
        scheduler.evaluate_once();

        assert_eq!(f.launched(), vec![id, id]);
    }

    #[test]
    fn a_queue_that_stops_being_scheduled_is_forgotten() {
        let f = Fixture::new();
        let id = f.nightly();
        f.scheduled("Something else to watch", TimeSpan::from_hours(23));
        let mut scheduler = f.scheduler();

        scheduler.evaluate_once();
        f.edit(id, |q| q.schedule.enabled = false);
        scheduler.evaluate_once();
        f.edit(id, |q| q.schedule.enabled = true);
        scheduler.evaluate_once();

        assert_eq!(f.launched().iter().filter(|l| **l == id).count(), 2);
    }

    // ----------------------------------- standing down and trying again --

    #[test]
    fn the_agent_stands_down_while_quickbyte_is_running() {
        let f = Fixture::new();
        f.nightly();
        f.app.running.store(true, Ordering::SeqCst);

        f.scheduler().evaluate_once();

        assert!(f.launched().is_empty());
        assert_eq!(f.log.count("QuickByte is running"), 1);
    }

    #[test]
    fn standing_down_is_not_recorded_as_a_start() {
        let f = Fixture::new();
        f.nightly();
        let mut scheduler = f.scheduler();

        f.app.running.store(true, Ordering::SeqCst);
        scheduler.evaluate_once();
        f.app.running.store(false, Ordering::SeqCst);
        f.advance(TimeDelta::minutes(1));
        scheduler.evaluate_once();

        assert_eq!(f.launched().len(), 1);
    }

    #[test]
    fn a_launch_that_did_not_take_is_tried_again() {
        let f = Fixture::new();
        let id = f.nightly();
        let mut scheduler = f.scheduler();

        *f.app.fails_with.lock().unwrap() = Some("the system cannot find the file specified".into());
        scheduler.evaluate_once();

        assert!(f.launched().is_empty());
        assert_eq!(f.log.count("could not start QuickByte"), 1);

        *f.app.fails_with.lock().unwrap() = None;
        f.advance(TimeDelta::seconds(30));
        scheduler.evaluate_once();

        assert_eq!(f.launched(), vec![id]);
    }

    // ------------------------------------------------------- the loop --

    #[test]
    fn the_loop_stops_as_soon_as_there_is_nothing_to_watch() {
        // A user who never schedules anything must not be left with a background
        // process — so this returns rather than sleeping on an empty file.
        let f = Fixture::new();
        let (_keep, cancel) = crossbeam_channel::bounded::<()>(1);

        assert_eq!(f.scheduler().run(&cancel), RunEnd::NothingToWatch);

        assert_eq!(f.log.count("scheduler started"), 1);
        assert_eq!(f.log.count("no queue has a schedule"), 1);
    }

    #[test]
    fn the_loop_stops_when_the_session_ends() {
        // The fake cancels from inside the read so the test does not sit
        // through a 30-second tick.
        let f = Fixture::new();
        f.nightly();
        let (tx, cancel) = crossbeam_channel::bounded::<()>(1);
        *f.repository.on_load.lock().unwrap() = Some(Box::new(move || {
            let _ = tx.try_send(());
        }));

        assert_eq!(f.scheduler().run(&cancel), RunEnd::Cancelled);

        assert_eq!(f.launched().len(), 1);
    }

    #[test]
    fn the_loop_keeps_ticking_while_something_is_scheduled() {
        let f = Fixture::new();
        let id = f.nightly();
        let (tx, cancel) = crossbeam_channel::bounded::<()>(1);
        let ticks = Arc::new(Mutex::new(0));
        let counter = ticks.clone();
        *f.repository.on_load.lock().unwrap() = Some(Box::new(move || {
            let mut n = counter.lock().unwrap();
            *n += 1;
            if *n == 3 {
                let _ = tx.try_send(());
            }
        }));

        assert_eq!(f.scheduler().run_every(Duration::from_millis(5), &cancel), RunEnd::Cancelled);

        assert_eq!(*ticks.lock().unwrap(), 3);
        assert_eq!(f.launched(), vec![id]);
    }
}
