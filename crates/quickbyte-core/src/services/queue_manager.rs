//! Port of `src/QuickByte.Core/Services/QueueManager.cs`.
//!
//! Runs queues and keeps their schedules.
//!
//! **One runner per running queue.** A run is a loop rather than a join over a
//! snapshot: it re-reads the queue on every pass, so a download appended while
//! the queue is running is picked up, a concurrency change takes effect on the
//! next free slot, and a stop time can end the run between downloads instead of
//! only between queues. Each download is started at most once per run — the
//! alternative is a queue that instantly restarts a download the user just
//! paused, and (when a start fails outright) a tight loop.
//!
//! **Two schedulers, one file.** The timer here starts due queues while the app
//! is open. When it is not, `QuickByte.Agent` — a separate process that outlives
//! it — reads the same queues.json, reaches the same verdict through
//! `DownloadQueue::is_due`, and launches QuickByte with `--run-queue`.
//! `DownloadQueue::last_run_at` is persisted the moment a run starts, which is
//! what stops the two from starting the same window twice.
//!
//! **When a queue drains** it says so (`QueueEvent::StateChanged { drained: true }`);
//! the UI runs the queue's power action, behind its countdown.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::enums::{DownloadStatus, QueueState};
use crate::events::{DownloadEvent, DownloadListChangeType, Dispatcher, EventCallback, EventHub, EventStream, QueueChangeType, QueueEvent};
use crate::helpers::{completion_plan, download_actions};
use crate::models::{DownloadItem, DownloadQueue};
use crate::serde_compat::DateTimeOffset;
use crate::traits::{self, DownloadManager, QueueRepository};

/// How often due queues are looked for. Schedules have minute resolution, so
/// this only has to be comfortably below a minute; it costs one pass over a
/// handful of objects.
const SCHEDULE_TICK_INTERVAL: Duration = Duration::from_secs(20);

/// Longest a runner waits before re-reading its queue while it has downloads in
/// flight. Short enough that a stop time, a concurrency change or a newly
/// appended download is acted on promptly.
const RUNNER_POLL: Duration = Duration::from_millis(1000);

/// One queue's in-flight run.
struct QueueRun {
    /// True when the schedule started this run, not the user — only those obey a stop time.
    scheduled: bool,
    cancel: CancellationToken,
}

#[derive(Default)]
struct State {
    queues: Vec<DownloadQueue>,
    /// Download id → queue id. Rebuilt on every membership change; a download is in at most one queue.
    membership: HashMap<Uuid, Uuid>,
    runs: HashMap<Uuid, QueueRun>,
}

impl State {
    fn find(&self, queue_id: Uuid) -> Option<&DownloadQueue> {
        self.queues.iter().find(|q| q.id == queue_id)
    }
    fn find_mut(&mut self, queue_id: Uuid) -> Option<&mut DownloadQueue> {
        self.queues.iter_mut().find(|q| q.id == queue_id)
    }
    fn reindex(&mut self) {
        self.membership.clear();
        for queue in &self.queues {
            for id in &queue.item_ids {
                self.membership.insert(*id, queue.id);
            }
        }
    }
}

pub struct QueueManager {
    me: Weak<QueueManager>,
    repository: Arc<dyn QueueRepository>,
    downloads: Arc<dyn DownloadManager>,
    runtime: Handle,
    sync: Mutex<State>,
    schedule_timer: Mutex<Option<CancellationToken>>,
    /// Guards against a slow tick overlapping the next one.
    tick_in_progress: AtomicBool,
    disposed: AtomicBool,
    hub: EventHub<QueueEvent>,
}

impl QueueManager {
    /// `runtime` runs the queue runners and the schedule timer.
    pub fn new(
        repository: Arc<dyn QueueRepository>,
        downloads: Arc<dyn DownloadManager>,
        dispatcher: Arc<dyn Dispatcher>,
        runtime: Handle,
    ) -> Arc<Self> {
        let manager = Arc::new_cyclic(|me| Self {
            me: me.clone(),
            repository,
            downloads: Arc::clone(&downloads),
            runtime,
            sync: Mutex::new(State::default()),
            schedule_timer: Mutex::new(None),
            tick_in_progress: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
            hub: EventHub::new(Some(dispatcher)),
        });

        // A download that is deleted has to leave its queue with it, or the queue
        // keeps a slot for a file that no longer exists and the runner walks past
        // an id it can never start.
        let me = Arc::downgrade(&manager);
        downloads.subscribe_fn(Arc::new(move |event: &DownloadEvent| {
            if let DownloadEvent::ListChanged { change: DownloadListChangeType::Removed, item } = event
                && let Some(manager) = me.upgrade()
            {
                traits::QueueManager::remove_from_queues(&*manager, &[item.id]);
            }
        }));
        manager
    }

    fn persist(&self, state: &State) {
        self.repository.save_all(&state.queues);
    }

    fn sanitize_name(name: &str) -> String {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return "New queue".into();
        }
        trimmed.chars().take(DownloadQueue::MAX_NAME_LENGTH).collect()
    }

    fn items_of(&self, queue_id: Uuid) -> Vec<Uuid> {
        self.sync.lock().find(queue_id).map(|q| q.item_ids.clone()).unwrap_or_default()
    }

    fn find_item(&self, download_id: Uuid) -> Option<DownloadItem> {
        self.downloads.get(download_id)
    }

    fn is_running(&self, queue_id: Uuid) -> bool {
        self.sync.lock().runs.contains_key(&queue_id)
    }

    fn is_run_scheduled(&self, queue_id: Uuid) -> bool {
        self.sync.lock().runs.get(&queue_id).is_some_and(|r| r.scheduled)
    }

    fn pause_if_running(&self, download_id: Uuid) {
        if self.find_item(download_id).is_some_and(|i| download_actions::can_pause(&i)) {
            self.downloads.pause(download_id);
        }
    }

    fn raise_queues_changed(&self, change: QueueChangeType, queue: Option<DownloadQueue>) {
        self.hub.publish(QueueEvent::QueuesChanged { change, queue: queue.map(Box::new) });
    }

    fn raise_state_changed(&self, queue_id: Uuid, state: QueueState, drained: bool) {
        self.hub.publish(QueueEvent::StateChanged { queue_id, state, drained });
    }

    fn start_internal(&self, queue_id: Uuid, scheduled: bool) {
        let Some(me) = self.me.upgrade() else { return };
        {
            let mut state = self.sync.lock();
            if state.runs.contains_key(&queue_id) {
                return;
            }
            let Some(queue) = state.find_mut(queue_id) else { return };
            // Written before the first download starts, and persisted: it is what
            // tells this app's next tick — and the agent reading the same file —
            // that this window's run is already under way.
            queue.last_run_at = Some(DateTimeOffset::now());
            let cancel = CancellationToken::new();
            state.runs.insert(queue_id, QueueRun { scheduled, cancel: cancel.clone() });
            self.persist(&state);
            self.runtime.spawn(async move { me.run(queue_id, cancel).await });
        }
        self.raise_state_changed(queue_id, QueueState::Running, false);
    }

    /// Walks one queue until it runs out of downloads to start. Only ever touches
    /// the engine through [`DownloadManager`], so the application-wide
    /// concurrency gate and the global speed limit still apply on top of the
    /// queue's own.
    async fn run(self: Arc<Self>, queue_id: Uuid, cancel: CancellationToken) {
        // Started once each, whatever they do next: a download that fails, or
        // that the user pauses by hand, is not started again by this run.
        let mut attempted: HashSet<Uuid> = HashSet::new();
        let mut active: HashMap<Uuid, JoinHandle<()>> = HashMap::new();
        let mut drained = false;

        while !cancel.is_cancelled() {
            let Some(queue) = traits::QueueManager::find(&*self, queue_id) else { break };

            // Only a run its own schedule started answers to that schedule's stop
            // time; one the user started by hand runs until it is done.
            if self.is_run_scheduled(queue_id) && queue.schedule.stop_at_enabled && queue.schedule.window_start(local_now()).is_none() {
                break;
            }

            let next = if (active.len() as i32) < queue.clamp_concurrency() { self.next_pending(&queue, &attempted) } else { None };
            if let Some(id) = next {
                attempted.insert(id);
                self.downloads.set_queue_speed_limit(id, queue.speed_limit_bytes_per_second);
                let downloads = Arc::clone(&self.downloads);
                // A queue is a sequence, not a transaction: a link that 404s must
                // not take the rest of the queue down with it, and the download's
                // own status already records what happened.
                active.insert(id, self.runtime.spawn(async move {
                    let _ = downloads.resume(id).await;
                }));
                continue;
            }

            if active.is_empty() {
                drained = true;
                break;
            }

            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(RUNNER_POLL) => {}
                _ = futures::future::select_all(active.values_mut()) => {}
            }

            let finished: Vec<Uuid> = active.iter().filter(|(_, h)| h.is_finished()).map(|(id, _)| *id).collect();
            for id in finished {
                active.remove(&id);
                self.downloads.set_queue_speed_limit(id, 0);
            }
        }

        // Left the loop with downloads still in flight, and not because the run
        // was cancelled: the stop time arrived. Nothing else will pause them — the
        // scheduler tick only stops a run that is still registered, and this one
        // is about to deregister itself — so a queue that promised to stop at
        // 06:00 has to do it here.
        let pause_active = !drained && !cancel.is_cancelled();
        for id in active.keys() {
            if pause_active {
                self.pause_if_running(*id);
            }
            self.downloads.set_queue_speed_limit(*id, 0);
        }

        self.sync.lock().runs.remove(&queue_id);
        self.raise_state_changed(queue_id, QueueState::Idle, drained);
    }

    /// The first download in queue order that is waiting to run and has not been
    /// started by this run yet. Completed, failed and cancelled downloads are
    /// skipped: a queue is a to-do list, not a retry loop.
    fn next_pending(&self, queue: &DownloadQueue, attempted: &HashSet<Uuid>) -> Option<Uuid> {
        let items: HashMap<Uuid, DownloadStatus> = self.downloads.downloads().into_iter().map(|i| (i.id, i.status)).collect();
        queue
            .item_ids
            .iter()
            .filter(|id| !attempted.contains(id))
            .find(|id| matches!(items.get(id), Some(DownloadStatus::Queued | DownloadStatus::Paused)))
            .copied()
    }

    /// One look at the schedules: starts the queues that are due, and stops a
    /// scheduled run that has reached its stop time. Public for the tests (and a
    /// host that wants to evaluate right now); the timer calls it every 20 s.
    pub fn on_schedule_tick(&self) {
        if self.disposed.load(Ordering::SeqCst) {
            return;
        }
        if self.tick_in_progress.swap(true, Ordering::SeqCst) {
            return;
        }
        let now = local_now();
        for queue in traits::QueueManager::queues(self) {
            if self.is_running(queue.id) {
                // A scheduled run that has reached its stop time. The runner
                // notices too, but only between downloads; this is what ends a
                // run whose last download would otherwise carry it past the
                // window on its own.
                if self.is_run_scheduled(queue.id) && queue.schedule.stop_at_enabled && queue.schedule.window_start(now).is_none() {
                    traits::QueueManager::stop(self, queue.id);
                }
                continue;
            }
            if queue.is_due(now) {
                self.start_internal(queue.id, true);
            }
        }
        self.tick_in_progress.store(false, Ordering::SeqCst);
    }
}

fn local_now() -> chrono::NaiveDateTime {
    chrono::Local::now().naive_local()
}

impl traits::QueueManager for QueueManager {
    fn queues(&self) -> Vec<DownloadQueue> {
        self.sync.lock().queues.clone()
    }

    fn subscribe(&self) -> EventStream<QueueEvent> {
        self.hub.subscribe()
    }

    fn load(&self) {
        {
            let mut state = self.sync.lock();
            state.queues = self.repository.load_all();
            state.reindex();
        }

        let cancel = CancellationToken::new();
        if let Some(previous) = self.schedule_timer.lock().replace(cancel.clone()) {
            previous.cancel();
        }
        let me = self.me.clone();
        self.runtime.spawn(async move {
            let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + SCHEDULE_TICK_INTERVAL, SCHEDULE_TICK_INTERVAL);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = ticker.tick() => {}
                }
                let Some(manager) = me.upgrade() else { return };
                // A tick that fails must not kill the timer — the next schedule
                // still deserves to fire. (Nothing here returns an error; a panic
                // would end only this task's tick, not the app.)
                manager.on_schedule_tick();
            }
        });
    }

    fn create(&self, name: &str) -> DownloadQueue {
        let queue = DownloadQueue { name: Self::sanitize_name(name), ..Default::default() };
        {
            let mut state = self.sync.lock();
            state.queues.push(queue.clone());
            self.persist(&state);
        }
        self.raise_queues_changed(QueueChangeType::Added, Some(queue.clone()));
        queue
    }

    fn update(&self, edited: DownloadQueue) {
        let (snapshot, speed_limit) = {
            let mut state = self.sync.lock();
            let Some(queue) = state.find_mut(edited.id) else { return };
            queue.name = Self::sanitize_name(&edited.name);
            queue.concurrent_downloads =
                edited.concurrent_downloads.clamp(DownloadQueue::MIN_CONCURRENT_DOWNLOADS, DownloadQueue::MAX_CONCURRENT_DOWNLOADS);
            queue.speed_limit_bytes_per_second = edited.speed_limit_bytes_per_second.max(0);
            queue.when_finished = completion_plan::defined(edited.when_finished);
            queue.schedule = edited.schedule.clone();
            let snapshot = queue.clone();
            self.persist(&state);
            (snapshot, edited.speed_limit_bytes_per_second.max(0))
        };

        // A speed limit changed while the queue is running applies to the
        // transfers already in flight, for the same reason the global one does: a
        // limit you have to restart a download to apply is not a limit. Only the
        // ones actually transferring are touched — the rest are given the new
        // limit as the queue reaches them.
        if self.is_running(edited.id) {
            for id in self.items_of(edited.id) {
                if self.find_item(id).is_some_and(|i| download_actions::can_pause(&i)) {
                    self.downloads.set_queue_speed_limit(id, speed_limit);
                }
            }
        }
        self.raise_queues_changed(QueueChangeType::Updated, Some(snapshot));
    }

    fn delete(&self, queue_id: Uuid) {
        self.stop(queue_id);
        let removed = {
            let mut state = self.sync.lock();
            let Some(index) = state.queues.iter().position(|q| q.id == queue_id) else { return };
            let removed = state.queues.remove(index);
            state.reindex();
            self.persist(&state);
            removed
        };
        // The queue's cap dies with it — its downloads are ordinary downloads now.
        for id in &removed.item_ids {
            self.downloads.set_queue_speed_limit(*id, 0);
        }
        self.raise_queues_changed(QueueChangeType::Removed, Some(removed));
    }

    fn find(&self, queue_id: Uuid) -> Option<DownloadQueue> {
        self.sync.lock().find(queue_id).cloned()
    }

    fn queue_of(&self, download_id: Uuid) -> Option<DownloadQueue> {
        let state = self.sync.lock();
        state.membership.get(&download_id).and_then(|q| state.find(*q)).cloned()
    }

    fn queue_id_of(&self, download_id: Uuid) -> Option<Uuid> {
        self.sync.lock().membership.get(&download_id).copied()
    }

    fn add_to_queue(&self, queue_id: Uuid, download_ids: &[Uuid]) {
        let mut ids: Vec<Uuid> = Vec::new();
        for id in download_ids {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        if ids.is_empty() {
            return;
        }
        let snapshot = {
            let mut state = self.sync.lock();
            if state.find(queue_id).is_none() {
                return;
            }
            for id in ids {
                if let Some(&current) = state.membership.get(&id) {
                    if current == queue_id {
                        continue; // already here, keep its place
                    }
                    if let Some(other) = state.find_mut(current) {
                        other.item_ids.retain(|i| *i != id);
                    }
                }
                if let Some(queue) = state.find_mut(queue_id) {
                    queue.item_ids.push(id);
                }
                state.membership.insert(id, queue_id);
            }
            state.reindex();
            self.persist(&state);
            state.find(queue_id).cloned()
        };
        self.raise_queues_changed(QueueChangeType::MembershipChanged, snapshot);
    }

    fn remove_from_queues(&self, download_ids: &[Uuid]) {
        if download_ids.is_empty() {
            return;
        }
        {
            let mut state = self.sync.lock();
            let mut changed = false;
            for id in download_ids {
                let Some(&queue_id) = state.membership.get(id) else { continue };
                if let Some(queue) = state.find_mut(queue_id) {
                    let before = queue.item_ids.len();
                    queue.item_ids.retain(|i| i != id);
                    changed |= queue.item_ids.len() != before;
                }
            }
            if !changed {
                return;
            }
            state.reindex();
            self.persist(&state);
        }
        for id in download_ids {
            self.downloads.set_queue_speed_limit(*id, 0);
        }
        self.raise_queues_changed(QueueChangeType::MembershipChanged, None);
    }

    fn move_item(&self, queue_id: Uuid, download_id: Uuid, offset: i32) -> bool {
        if offset == 0 {
            return false;
        }
        let snapshot = {
            let mut state = self.sync.lock();
            let Some(queue) = state.find_mut(queue_id) else { return false };
            let Some(from) = queue.item_ids.iter().position(|i| *i == download_id) else { return false };
            let to = (from as i64 + offset as i64).clamp(0, queue.item_ids.len() as i64 - 1) as usize;
            if to == from {
                return false;
            }
            let id = queue.item_ids.remove(from);
            queue.item_ids.insert(to, id);
            let snapshot = queue.clone();
            self.persist(&state);
            snapshot
        };
        self.raise_queues_changed(QueueChangeType::MembershipChanged, Some(snapshot));
        true
    }

    fn start(&self, queue_id: Uuid) {
        self.start_internal(queue_id, false);
    }

    /// Starts a queue because something outside the app said its time had come —
    /// the scheduler agent, via `--run-queue` on the command line. Treated as a
    /// scheduled run (so its stop time applies) but started even if the window
    /// has since closed: the agent already decided, and the launch it asked for
    /// took time to arrive.
    fn start_from_scheduler(&self, queue_id: Uuid) {
        self.start_internal(queue_id, true);
    }

    fn stop(&self, queue_id: Uuid) {
        let Some(cancel) = self.sync.lock().runs.get(&queue_id).map(|r| r.cancel.clone()) else { return };
        cancel.cancel();
        // Pause rather than stop: the run is over, the downloads are not. Their
        // chunk files stay on disk so the next run continues them.
        for id in self.items_of(queue_id) {
            self.pause_if_running(id);
            self.downloads.set_queue_speed_limit(id, 0);
        }
    }

    fn state_of(&self, queue_id: Uuid) -> QueueState {
        if self.is_running(queue_id) { QueueState::Running } else { QueueState::Idle }
    }

    fn next_run_at(&self, queue_id: Uuid) -> Option<chrono::NaiveDateTime> {
        self.sync.lock().find(queue_id).and_then(|q| q.next_run_at(local_now()))
    }

    fn has_scheduled_queues(&self) -> bool {
        self.sync.lock().queues.iter().any(|q| q.schedule.enabled)
    }

    fn shutdown(&self) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(timer) = self.schedule_timer.lock().take() {
            timer.cancel();
        }
        // Cancelled, not stopped: shutdown pauses every download through
        // DownloadManager::pause_all anyway, and doing it twice would race with
        // the close.
        let runs: Vec<CancellationToken> = self.sync.lock().runs.drain().map(|(_, r)| r.cancel).collect();
        for cancel in runs {
            cancel.cancel();
        }
    }
}

/// Not part of the trait, but what a UI that wants queue events inline uses.
impl QueueManager {
    pub fn subscribe_fn(&self, callback: EventCallback<QueueEvent>) {
        self.hub.subscribe_fn(callback);
    }
}

#[cfg(test)]
mod tests;
