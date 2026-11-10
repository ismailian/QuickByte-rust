//! Queue membership, ordering and running, against a fake download manager.
//! (`QueueManagerTests.cs`.)
//!
//! The runner is a loop rather than a join over a snapshot, so the tests that
//! exercise it wait on an outcome rather than on a clock — a queue drains when it
//! runs out of downloads to start, which is an event, not a duration.

use std::sync::atomic::AtomicBool;

use async_trait::async_trait;

use super::*;
use crate::enums::{CompletionActions, ConnectionCommand, PowerAction, ScheduleDays};
use crate::error::Result;
use crate::events::NoopDispatcher;
use crate::helpers::file_checksum::{ChecksumResult, ExpectedChecksum};
use crate::models::{AddressChange, ConnectionInfo, DownloadRequest};
use crate::serde_compat::TimeSpan;
use crate::traits::{ProgressFn, QueueManager as _};

#[derive(Default)]
struct FakeQueueRepository {
    stored: Mutex<Vec<DownloadQueue>>,
    saved: Mutex<Vec<Vec<DownloadQueue>>>,
}

impl QueueRepository for FakeQueueRepository {
    fn load_all(&self) -> Vec<DownloadQueue> {
        self.stored.lock().clone()
    }
    fn try_load_all(&self) -> Option<Vec<DownloadQueue>> {
        Some(self.stored.lock().clone())
    }
    fn save_all(&self, queues: &[DownloadQueue]) {
        self.saved.lock().push(queues.to_vec());
    }
}

#[derive(Default)]
struct FakeDownloadManager {
    items: Mutex<Vec<DownloadItem>>,
    release: CancellationToken,
    resumed: Mutex<Vec<Uuid>>,
    paused: Mutex<Vec<Uuid>>,
    stopped: Mutex<Vec<Uuid>>,
    queue_speed_limits: Mutex<Vec<(Uuid, i64)>>,
    /// Holds resume open so concurrency and stop can be observed.
    block_resume: AtomicBool,
    /// Leaves the download pending after a resume, as a failed start would.
    leave_queued_after_resume: AtomicBool,
    hub: EventHub<DownloadEvent>,
}

impl FakeDownloadManager {
    fn add(&self, status: DownloadStatus) -> Uuid {
        let item = DownloadItem { status, ..Default::default() };
        let id = item.id;
        self.items.lock().push(item);
        id
    }
    fn set_status(&self, id: Uuid, status: DownloadStatus) {
        if let Some(i) = self.items.lock().iter_mut().find(|i| i.id == id) {
            i.status = status;
        }
    }
    /// Lets every held resume through, and every later one straight past.
    fn release_resume(&self) {
        self.block_resume.store(false, Ordering::SeqCst);
        self.release.cancel();
    }
    fn raise_removed(&self, id: Uuid) {
        let mut items = self.items.lock();
        let Some(index) = items.iter().position(|i| i.id == id) else { return };
        let item = items.remove(index);
        drop(items);
        self.hub.publish(DownloadEvent::ListChanged { change: DownloadListChangeType::Removed, item: Box::new(item) });
    }
    fn resumed(&self) -> Vec<Uuid> {
        self.resumed.lock().clone()
    }
}

#[async_trait]
impl DownloadManager for FakeDownloadManager {
    fn downloads(&self) -> Vec<DownloadItem> {
        self.items.lock().clone()
    }
    fn get(&self, id: Uuid) -> Option<DownloadItem> {
        self.items.lock().iter().find(|i| i.id == id).cloned()
    }
    fn subscribe(&self) -> EventStream<DownloadEvent> {
        self.hub.subscribe()
    }
    fn subscribe_fn(&self, callback: EventCallback<DownloadEvent>) {
        self.hub.subscribe_fn(callback);
    }
    fn connections(&self, _id: Uuid) -> Vec<ConnectionInfo> {
        Vec::new()
    }
    async fn add_download(&self, _request: DownloadRequest) -> Result<DownloadItem> {
        unimplemented!()
    }
    async fn start(&self, id: Uuid) -> Result<()> {
        self.resume(id).await
    }
    fn pause(&self, id: Uuid) {
        self.paused.lock().push(id);
        self.set_status(id, DownloadStatus::Paused);
    }
    fn pause_all(&self) -> usize {
        0
    }
    async fn resume(&self, id: Uuid) -> Result<()> {
        self.resumed.lock().push(id);
        if self.block_resume.load(Ordering::SeqCst) {
            self.set_status(id, DownloadStatus::Downloading);
            self.release.cancelled().await;
        }
        let leave = self.leave_queued_after_resume.load(Ordering::SeqCst);
        self.set_status(id, if leave { DownloadStatus::Queued } else { DownloadStatus::Completed });
        Ok(())
    }
    fn stop(&self, id: Uuid) {
        self.stopped.lock().push(id);
    }
    async fn retry(&self, id: Uuid) -> Result<()> {
        self.resume(id).await
    }
    fn remove(&self, _id: Uuid, _delete_file: bool) {}
    fn change_address(&self, _id: Uuid, _change: AddressChange) -> bool {
        false
    }
    fn control_connection(&self, _id: Uuid, _connection_id: i32, _command: ConnectionCommand) -> bool {
        false
    }
    async fn verify_checksum(&self, _id: Uuid, _e: ExpectedChecksum, _p: Option<ProgressFn<i64>>, _c: CancellationToken) -> Result<ChecksumResult> {
        Ok(ChecksumResult::NotChecked)
    }
    fn global_speed_limit(&self) -> i64 {
        0
    }
    fn set_speed_limit(&self, _id: Uuid, _bps: i64) {}
    fn set_completion_actions(&self, _id: Uuid, _a: CompletionActions, _p: PowerAction) {}
    fn set_queue_speed_limit(&self, id: Uuid, bps: i64) {
        self.queue_speed_limits.lock().push((id, bps));
    }
    fn set_global_speed_limit(&self, _bps: i64) {}
    fn load_persisted_downloads(&self) {}
    async fn cleanup_orphaned_temp_folders(&self) -> usize {
        0
    }
    fn flush(&self) {}
}

struct Fixture {
    repository: Arc<FakeQueueRepository>,
    downloads: Arc<FakeDownloadManager>,
    manager: Arc<QueueManager>,
    states: Arc<Mutex<Vec<(QueueState, bool)>>>,
    changes: Arc<Mutex<Vec<QueueChangeType>>>,
}

impl Fixture {
    fn new() -> Self {
        let repository = Arc::new(FakeQueueRepository::default());
        let downloads = Arc::new(FakeDownloadManager::default());
        let manager = QueueManager::new(repository.clone(), downloads.clone(), Arc::new(NoopDispatcher), Handle::current());
        let states = Arc::new(Mutex::new(Vec::new()));
        let changes = Arc::new(Mutex::new(Vec::new()));
        let (s, c) = (Arc::clone(&states), Arc::clone(&changes));
        manager.subscribe_fn(Arc::new(move |event| match event {
            QueueEvent::StateChanged { state, drained, .. } => s.lock().push((*state, *drained)),
            QueueEvent::QueuesChanged { change, .. } => c.lock().push(*change),
        }));
        Self { repository, downloads, manager, states, changes }
    }

    async fn wait_for(&self, condition: impl Fn() -> bool) {
        for _ in 0..500 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(condition(), "the queue did not reach the expected state in time");
    }

    async fn wait_for_idle(&self, queue_id: Uuid) {
        self.wait_for(|| self.manager.state_of(queue_id) == QueueState::Idle).await;
    }

    fn queued(&self, count: usize) -> Vec<Uuid> {
        (0..count).map(|_| self.downloads.add(DownloadStatus::Queued)).collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.manager.shutdown();
    }
}

// ------------------------------------------------------------------ CRUD --

#[tokio::test]
async fn a_new_queue_is_persisted_and_announced() {
    let f = Fixture::new();
    let queue = f.manager.create("Nightly");
    assert_eq!(queue.name, "Nightly");
    assert_eq!(*f.changes.lock(), [QueueChangeType::Added]);
    assert_eq!(f.repository.saved.lock().len(), 1);
}

#[tokio::test]
async fn a_queue_name_is_tidied_up() {
    let f = Fixture::new();
    for (given, expected) in [("", "New queue"), ("   ", "New queue"), ("  Nightly  ", "Nightly")] {
        assert_eq!(f.manager.create(given).name, expected);
    }
}

#[tokio::test]
async fn a_very_long_queue_name_is_cut_to_length() {
    let f = Fixture::new();
    assert_eq!(f.manager.create(&"x".repeat(200)).name.len(), DownloadQueue::MAX_NAME_LENGTH);
}

#[tokio::test]
async fn update_clamps_what_it_is_given() {
    let f = Fixture::new();
    let queue = f.manager.create("Nightly");
    f.manager.update(DownloadQueue { concurrent_downloads: 999, speed_limit_bytes_per_second: -5, ..queue.clone() });
    let stored = f.manager.find(queue.id).unwrap();
    assert_eq!(stored.concurrent_downloads, DownloadQueue::MAX_CONCURRENT_DOWNLOADS);
    assert_eq!(stored.speed_limit_bytes_per_second, 0);
}

#[tokio::test]
async fn update_applies_what_the_queue_does_when_it_finishes() {
    let f = Fixture::new();
    let queue = f.manager.create("Nightly");
    f.manager.update(DownloadQueue { when_finished: PowerAction::ShutDown, ..queue.clone() });
    assert_eq!(f.manager.find(queue.id).unwrap().when_finished, PowerAction::ShutDown);
}

#[tokio::test]
async fn update_of_a_queue_that_is_gone_is_harmless() {
    let f = Fixture::new();
    f.manager.update(DownloadQueue { name: "Ghost".into(), ..Default::default() });
    assert!(f.manager.queues().is_empty());
}

#[tokio::test]
async fn find_hands_back_a_copy_rather_than_the_live_queue() {
    // So a half-finished edit is not already live on the queue the runner is reading.
    let f = Fixture::new();
    let queue = f.manager.create("Nightly");
    let mut found = f.manager.find(queue.id).unwrap();
    found.name = "Edited".into();
    assert_eq!(f.manager.find(queue.id).unwrap().name, "Nightly");
}

#[tokio::test]
async fn deleting_a_queue_lifts_its_speed_limit_from_its_downloads() {
    // The queue's cap dies with it — its downloads are ordinary downloads now.
    let f = Fixture::new();
    let item = f.downloads.add(DownloadStatus::Queued);
    let queue = f.manager.create("Nightly");
    f.manager.add_to_queue(queue.id, &[item]);
    f.manager.delete(queue.id);
    assert!(f.manager.queues().is_empty());
    assert!(f.downloads.queue_speed_limits.lock().contains(&(item, 0)));
}

// ------------------------------------------------------------ membership --

#[tokio::test]
async fn a_download_belongs_to_at_most_one_queue() {
    let f = Fixture::new();
    let item = f.downloads.add(DownloadStatus::Queued);
    let first = f.manager.create("First");
    let second = f.manager.create("Second");
    f.manager.add_to_queue(first.id, &[item]);
    f.manager.add_to_queue(second.id, &[item]);
    assert!(f.manager.find(first.id).unwrap().item_ids.is_empty());
    assert_eq!(f.manager.find(second.id).unwrap().item_ids.len(), 1);
    assert_eq!(f.manager.queue_id_of(item), Some(second.id));
    assert_eq!(f.manager.queue_of(item).unwrap().id, second.id);
}

#[tokio::test]
async fn re_adding_a_download_to_the_queue_it_is_in_keeps_its_place() {
    let f = Fixture::new();
    let ids = f.queued(2);
    let queue = f.manager.create("Nightly");
    f.manager.add_to_queue(queue.id, &ids);
    f.manager.add_to_queue(queue.id, &[ids[0]]);
    assert_eq!(f.manager.find(queue.id).unwrap().item_ids, ids);
}

#[tokio::test]
async fn adding_the_same_download_twice_in_one_call_adds_it_once() {
    let f = Fixture::new();
    let item = f.downloads.add(DownloadStatus::Queued);
    let queue = f.manager.create("Nightly");
    f.manager.add_to_queue(queue.id, &[item, item]);
    assert_eq!(f.manager.find(queue.id).unwrap().item_ids.len(), 1);
}

#[tokio::test]
async fn a_deleted_download_leaves_its_queue_with_it() {
    // Or the queue keeps a slot for a file that no longer exists and the runner
    // walks past an id it can never start.
    let f = Fixture::new();
    let item = f.downloads.add(DownloadStatus::Queued);
    let queue = f.manager.create("Nightly");
    f.manager.add_to_queue(queue.id, &[item]);
    f.downloads.raise_removed(item);
    assert!(f.manager.find(queue.id).unwrap().item_ids.is_empty());
    assert_eq!(f.manager.queue_id_of(item), None);
}

#[tokio::test]
async fn move_reorders_within_the_queue() {
    let f = Fixture::new();
    let ids = f.queued(4);
    let queue = f.manager.create("Nightly");
    f.manager.add_to_queue(queue.id, &ids);
    assert!(f.manager.move_item(queue.id, ids[2], -1));
    assert_eq!(f.manager.find(queue.id).unwrap().item_ids, [ids[0], ids[2], ids[1], ids[3]]);
}

#[tokio::test]
async fn move_clamps_at_the_ends_and_reports_when_nothing_happened() {
    let f = Fixture::new();
    let ids = f.queued(3);
    let queue = f.manager.create("Nightly");
    f.manager.add_to_queue(queue.id, &ids);
    assert!(!f.manager.move_item(queue.id, ids[0], -1));
    assert!(!f.manager.move_item(queue.id, ids[2], 5));
    assert!(!f.manager.move_item(queue.id, ids[0], 0));
    assert!(!f.manager.move_item(queue.id, Uuid::new_v4(), 1));
    assert!(f.manager.move_item(queue.id, ids[0], 5));
    assert_eq!(f.manager.find(queue.id).unwrap().item_ids, [ids[1], ids[2], ids[0]]);
}

// --------------------------------------------------------------- running --

#[tokio::test]
async fn a_run_starts_the_queue_s_downloads_in_order() {
    let f = Fixture::new();
    let ids = f.queued(3);
    let queue = f.manager.create("Nightly");
    f.manager.add_to_queue(queue.id, &ids);
    f.manager.start(queue.id);
    f.wait_for_idle(queue.id).await;
    assert_eq!(f.downloads.resumed(), ids);
}

#[tokio::test]
async fn a_drained_queue_says_so() {
    let f = Fixture::new();
    let queue = f.manager.create("Nightly");
    f.manager.add_to_queue(queue.id, &f.queued(1));
    f.manager.start(queue.id);
    f.wait_for_idle(queue.id).await;
    f.wait_for(|| f.states.lock().len() == 2).await;
    assert_eq!(*f.states.lock().last().unwrap(), (QueueState::Idle, true));
    assert_eq!(f.manager.state_of(queue.id), QueueState::Idle);
}

#[tokio::test]
async fn a_run_skips_downloads_that_are_already_finished() {
    // A queue is a to-do list, not a retry loop.
    let f = Fixture::new();
    let completed = f.downloads.add(DownloadStatus::Completed);
    let failed = f.downloads.add(DownloadStatus::Failed);
    let pending = f.downloads.add(DownloadStatus::Paused);
    let queue = f.manager.create("Nightly");
    f.manager.add_to_queue(queue.id, &[completed, failed, pending]);
    f.manager.start(queue.id);
    f.wait_for_idle(queue.id).await;
    assert_eq!(f.downloads.resumed(), [pending]);
}

#[tokio::test]
async fn a_download_is_started_at_most_once_per_run() {
    // Otherwise the queue instantly restarts a download the user just paused,
    // and a start that fails outright becomes a tight loop.
    let f = Fixture::new();
    let item = f.downloads.add(DownloadStatus::Queued);
    f.downloads.leave_queued_after_resume.store(true, Ordering::SeqCst);
    let queue = f.manager.create("Nightly");
    f.manager.add_to_queue(queue.id, &[item]);
    f.manager.start(queue.id);
    f.wait_for_idle(queue.id).await;
    assert_eq!(f.downloads.resumed().len(), 1);
}

#[tokio::test]
async fn starting_a_queue_that_is_already_running_does_nothing() {
    let f = Fixture::new();
    let item = f.downloads.add(DownloadStatus::Queued);
    f.downloads.block_resume.store(true, Ordering::SeqCst);
    let queue = f.manager.create("Nightly");
    f.manager.add_to_queue(queue.id, &[item]);

    f.manager.start(queue.id);
    f.wait_for(|| f.downloads.resumed().len() == 1).await;
    f.manager.start(queue.id);
    assert_eq!(f.downloads.resumed().len(), 1);

    f.downloads.release_resume();
    f.wait_for_idle(queue.id).await;
}

#[tokio::test]
async fn a_run_honours_the_queue_s_own_concurrency() {
    let f = Fixture::new();
    let ids = f.queued(5);
    f.downloads.block_resume.store(true, Ordering::SeqCst);
    let queue = f.manager.create("Nightly");
    f.manager.add_to_queue(queue.id, &ids);
    f.manager.update(DownloadQueue { concurrent_downloads: 2, ..f.manager.find(queue.id).unwrap() });

    f.manager.start(queue.id);
    f.wait_for(|| f.downloads.resumed().len() == 2).await;

    // Two slots, five downloads: the third waits for one to finish.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(f.downloads.resumed().len(), 2);

    f.downloads.release_resume();
    f.wait_for_idle(queue.id).await;
    assert_eq!(f.downloads.resumed().len(), 5);
}

#[tokio::test]
async fn a_run_applies_the_queue_s_speed_limit_and_lifts_it_afterwards() {
    let f = Fixture::new();
    let item = f.downloads.add(DownloadStatus::Queued);
    let queue = f.manager.create("Nightly");
    f.manager.add_to_queue(queue.id, &[item]);
    f.manager.update(DownloadQueue { speed_limit_bytes_per_second: 100_000, ..f.manager.find(queue.id).unwrap() });

    f.manager.start(queue.id);
    f.wait_for_idle(queue.id).await;

    let limits = f.downloads.queue_speed_limits.lock().clone();
    assert!(limits.contains(&(item, 100_000)));
    assert_eq!(*limits.last().unwrap(), (item, 0));
}

#[tokio::test]
async fn stopping_a_queue_pauses_its_downloads_rather_than_discarding_them() {
    // The run is over, the downloads are not. Their chunk files stay on disk so
    // the next run continues them.
    let f = Fixture::new();
    let item = f.downloads.add(DownloadStatus::Queued);
    f.downloads.block_resume.store(true, Ordering::SeqCst);
    let queue = f.manager.create("Nightly");
    f.manager.add_to_queue(queue.id, &[item]);

    f.manager.start(queue.id);
    f.wait_for(|| f.downloads.resumed().len() == 1).await;
    f.downloads.set_status(item, DownloadStatus::Downloading);

    f.manager.stop(queue.id);
    assert!(f.downloads.paused.lock().contains(&item));
    assert!(f.downloads.stopped.lock().is_empty());

    f.downloads.release_resume();
    f.wait_for_idle(queue.id).await;
    // Stopped by the user is not drained: no power action behind it.
    f.wait_for(|| f.states.lock().len() == 2).await;
    assert_eq!(*f.states.lock().last().unwrap(), (QueueState::Idle, false));
}

#[tokio::test]
async fn stopping_a_queue_that_is_not_running_is_harmless() {
    let f = Fixture::new();
    let queue = f.manager.create("Nightly");
    f.manager.stop(queue.id);
    assert_eq!(f.manager.state_of(queue.id), QueueState::Idle);
}

#[tokio::test]
async fn starting_a_run_stamps_the_window_so_nothing_starts_it_twice() {
    // Persisted the moment a run starts: it is what tells this app's next tick —
    // and the agent reading the same file — that this window is under way.
    let f = Fixture::new();
    let queue = f.manager.create("Nightly");
    f.manager.start(queue.id);
    assert!(f.manager.find(queue.id).unwrap().last_run_at.is_some());
    assert!(f.repository.saved.lock().last().unwrap()[0].last_run_at.is_some());
}

#[tokio::test]
async fn has_scheduled_queues_reports_whether_the_agent_is_worth_running() {
    let f = Fixture::new();
    let queue = f.manager.create("Nightly");
    assert!(!f.manager.has_scheduled_queues());
    let mut edited = f.manager.find(queue.id).unwrap();
    edited.schedule.enabled = true;
    f.manager.update(edited);
    assert!(f.manager.has_scheduled_queues());
}

#[tokio::test]
async fn next_run_at_comes_from_the_queue_s_own_schedule() {
    let f = Fixture::new();
    let queue = f.manager.create("Nightly");
    let mut edited = f.manager.find(queue.id).unwrap();
    edited.schedule.enabled = true;
    edited.schedule.days = ScheduleDays::EVERY_DAY;
    edited.schedule.start_time = TimeSpan::from_hours(2);
    f.manager.update(edited);
    assert!(f.manager.next_run_at(queue.id).is_some());
    assert!(f.manager.next_run_at(Uuid::new_v4()).is_none());
}

#[tokio::test]
async fn load_reads_the_queues_off_disk() {
    let f = Fixture::new();
    let stored = DownloadQueue { name: "Nightly".into(), item_ids: vec![Uuid::new_v4()], ..Default::default() };
    f.repository.stored.lock().push(stored.clone());
    f.manager.load();
    assert_eq!(f.manager.queues()[0].name, "Nightly");
    assert_eq!(f.manager.queue_id_of(stored.item_ids[0]), Some(stored.id));
}

#[tokio::test]
async fn a_due_queue_is_started_by_the_schedule_tick_and_only_once_per_window() {
    let f = Fixture::new();
    let queue = f.manager.create("Nightly");
    let mut edited = f.manager.find(queue.id).unwrap();
    edited.schedule.enabled = true;
    edited.schedule.days = ScheduleDays::EVERY_DAY;
    // Started a minute ago, in its one-hour grace period.
    let now = chrono::Local::now().naive_local();
    let start = now.time() - chrono::TimeDelta::minutes(1);
    edited.schedule.start_time = TimeSpan::from_delta(start.signed_duration_since(chrono::NaiveTime::MIN));
    f.manager.update(edited);
    if now.time() < chrono::NaiveTime::from_hms_opt(0, 2, 0).unwrap() {
        return; // the window would start yesterday; not this test's subject
    }

    f.manager.on_schedule_tick();
    f.wait_for_idle(queue.id).await;
    assert!(f.manager.find(queue.id).unwrap().last_run_at.is_some());
    let runs = f.states.lock().iter().filter(|(s, _)| *s == QueueState::Running).count();
    assert_eq!(runs, 1);

    f.manager.on_schedule_tick();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(f.states.lock().iter().filter(|(s, _)| *s == QueueState::Running).count(), 1);
}
