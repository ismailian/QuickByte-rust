//! One download's lifecycle, driven with a fake pool and merger so the state
//! machine can be exercised without a server or a stopwatch.
//! (`DownloadServiceTests.cs`.)

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::enums::DownloadStatus;
use crate::helpers::disk_space::scratch::TempFolder;
use crate::models::{DownloadCredentials, DownloadSettings, Headers, RemoteFileVersion};
use crate::traits::DownloadService as _;

// ---------------------------------------------------------------- fakes --

pub(crate) struct FakeSettings {
    current: Mutex<Arc<DownloadSettings>>,
    hub: EventHub<Arc<DownloadSettings>>,
}

impl FakeSettings {
    pub(crate) fn new(settings: DownloadSettings) -> Arc<Self> {
        Arc::new(Self { current: Mutex::new(Arc::new(settings)), hub: EventHub::default() })
    }
}

impl SettingsService for FakeSettings {
    fn current(&self) -> Arc<crate::models::DownloadSettings> {
        self.current.lock().clone()
    }
    fn subscribe(&self) -> EventStream<Arc<DownloadSettings>> {
        self.hub.subscribe()
    }
    fn subscribe_fn(&self, callback: EventCallback<Arc<DownloadSettings>>) {
        self.hub.subscribe_fn(callback);
    }
    fn load(&self) {}
    fn save(&self, settings: DownloadSettings) {
        let settings = Arc::new(settings);
        *self.current.lock() = settings.clone();
        self.hub.publish(settings);
    }
}

type Respond = Box<dyn Fn(i32) -> RemoteFileInfo + Send + Sync>;

#[derive(Default)]
struct FakeInfoProvider {
    info: Mutex<RemoteFileInfo>,
    probes: AtomicI32,
    last_url: Mutex<Option<String>>,
    throw: Mutex<Option<CoreError>>,
    /// Answers probe N (counting from 1) when set; otherwise every probe gets the fixture's info.
    respond: Mutex<Option<Respond>>,
}

#[async_trait]
impl RemoteFileInfoProvider for FakeInfoProvider {
    async fn get_file_info(&self, url: &str, _options: &crate::models::RequestOptions, _cancel: &CancellationToken) -> Result<RemoteFileInfo> {
        let probe = self.probes.fetch_add(1, Ordering::SeqCst) + 1;
        *self.last_url.lock() = Some(url.to_string());
        if let Some(error) = self.throw.lock().clone() {
            return Err(error);
        }
        if let Some(respond) = self.respond.lock().as_ref() {
            return Ok(respond(probe));
        }
        Ok(self.info.lock().clone())
    }
}

struct FakePool {
    succeed: AtomicBool,
    failure_message: Mutex<Option<String>>,
    block_until_cancelled: AtomicBool,
    runs: AtomicI32,
    started: Notify,
    /// How many runs report that the file changed on the server before one completes.
    throw_changed_times: AtomicI32,
    discards: AtomicI32,
    hub: EventHub<PoolEvent>,
}

impl Default for FakePool {
    fn default() -> Self {
        Self {
            succeed: AtomicBool::new(true),
            failure_message: Mutex::new(None),
            block_until_cancelled: AtomicBool::new(false),
            runs: AtomicI32::new(0),
            started: Notify::new(),
            throw_changed_times: AtomicI32::new(0),
            discards: AtomicI32::new(0),
            hub: EventHub::default(),
        }
    }
}

#[async_trait]
impl ConnectionPoolManager for FakePool {
    fn subscribe(&self) -> EventStream<PoolEvent> {
        self.hub.subscribe()
    }
    fn subscribe_fn(&self, callback: EventCallback<PoolEvent>) {
        self.hub.subscribe_fn(callback);
    }
    fn snapshot(&self) -> Vec<ConnectionInfo> {
        Vec::new()
    }
    async fn run(&self, _item: &DownloadItem, file_info: &RemoteFileInfo, _settings: Arc<DownloadSettings>, cancel: CancellationToken) -> Result<bool> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        if self.block_until_cancelled.load(Ordering::SeqCst) {
            cancel.cancelled().await;
            return Err(CoreError::Cancelled);
        }
        if self.throw_changed_times.load(Ordering::SeqCst) > 0 {
            self.throw_changed_times.fetch_sub(1, Ordering::SeqCst);
            return Err(CoreError::RemoteFileChanged("The file on the server has changed.".into()));
        }
        let succeed = self.succeed.load(Ordering::SeqCst);
        if !succeed && let Some(message) = self.failure_message.lock().clone() {
            self.hub.publish(PoolEvent::ConnectionFailed(message));
        }
        self.hub.publish(PoolEvent::Connections(Vec::new()));
        self.hub.publish(PoolEvent::Progress(DownloadProgress {
            download_id: Uuid::nil(),
            downloaded_bytes: file_info.content_length,
            total_bytes: file_info.content_length,
            speed_bytes_per_second: 0.0,
            estimated_time_remaining: None,
            merge_percentage: None,
        }));
        Ok(succeed)
    }
    fn discard_chunks(&self, _temp_folder: &str) {
        self.discards.fetch_add(1, Ordering::SeqCst);
    }
    fn forget_layout(&self, _temp_folder: &str) {}
    fn ordered_chunk_paths(&self) -> Vec<String> {
        vec!["part0.tmp".into()]
    }
    fn control_connection(&self, _connection_id: i32, _command: ConnectionCommand) -> bool {
        false
    }
}

#[derive(Default)]
struct FakeMerger {
    merged: AtomicBool,
    report_during_merge: Mutex<Vec<i64>>,
    /// Leaves a real file at the destination, as a merge would.
    write_destination: AtomicBool,
    /// Pretends a merge was interrupted in the temp folder, as a restart mid-merge leaves it.
    interrupted: AtomicBool,
    merged_chunk_lists: Mutex<Vec<Vec<String>>>,
    discards: AtomicI32,
}

#[async_trait]
impl FileMerger for FakeMerger {
    async fn merge(
        &self,
        _temp_folder: &str,
        ordered_chunk_paths: &[String],
        destination_file_path: &str,
        _buffer_size: usize,
        bytes_merged: Option<ProgressFn<i64>>,
        _cancel: &CancellationToken,
    ) -> Result<()> {
        self.merged.store(true, Ordering::SeqCst);
        self.interrupted.store(false, Ordering::SeqCst);
        self.merged_chunk_lists.lock().push(ordered_chunk_paths.to_vec());
        if self.write_destination.load(Ordering::SeqCst) {
            std::fs::write(destination_file_path, [1, 2, 3])?;
        }
        let reports = self.report_during_merge.lock().clone();
        for merged in reports {
            if let Some(report) = &bytes_merged {
                report(merged);
            }
        }
        Ok(())
    }
    fn has_interrupted_merge(&self, _temp_folder: &str) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }
    fn discard_interrupted_merge(&self, _temp_folder: &str, _destination_file_path: &str) {
        self.interrupted.store(false, Ordering::SeqCst);
        self.discards.fetch_add(1, Ordering::SeqCst);
    }
    fn cleanup_chunks(&self, _chunk_paths: &[String], _temp_folder: &str) {}
}

struct Fixture {
    folder: TempFolder,
    settings: Arc<FakeSettings>,
    provider: Arc<FakeInfoProvider>,
    pool: Arc<FakePool>,
    merger: Arc<FakeMerger>,
    service: Arc<DownloadService>,
    history: Arc<Mutex<Vec<DownloadStatus>>>,
    progress: Arc<Mutex<Vec<DownloadProgress>>>,
    persisted: Arc<AtomicI32>,
    /// For a fake that has to reach back into the service (a pause during a probe).
    me: Arc<OnceLock<Weak<DownloadService>>>,
}

impl Fixture {
    fn new() -> Self {
        Self::with(true, Some(Arc::new(|_| None)))
    }

    fn with(mark_from_internet: bool, available_bytes: Option<AvailableBytesFn>) -> Self {
        let folder = TempFolder::new();
        let item = DownloadItem {
            url: "https://example.com/file.bin".into(),
            file_name: "file.bin".into(),
            save_folder: folder.path_str(),
            ..Default::default()
        };
        let settings = FakeSettings::new(DownloadSettings { temp_folder: folder.path_str(), ..Default::default() });
        let provider = Arc::new(FakeInfoProvider {
            info: Mutex::new(RemoteFileInfo {
                content_length: 1000,
                content_type: "application/octet-stream".into(),
                final_url: "https://example.com/file.bin".into(),
                ..Default::default()
            }),
            ..Default::default()
        });
        let pool = Arc::new(FakePool::default());
        let merger = Arc::new(FakeMerger::default());
        let persisted = Arc::new(AtomicI32::new(0));
        let counter = Arc::clone(&persisted);
        let service = DownloadService::new(
            item,
            pool.clone(),
            merger.clone(),
            provider.clone(),
            settings.clone(),
            Arc::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
            ServiceOptions { mark_from_internet, available_bytes, speed_limits: None },
        );

        let history = Arc::new(Mutex::new(Vec::new()));
        let progress = Arc::new(Mutex::new(Vec::new()));
        let (h, p) = (Arc::clone(&history), Arc::clone(&progress));
        service.subscribe_fn(Arc::new(move |event| match event {
            DownloadEvent::StatusChanged(s) => h.lock().push(s.new_status),
            DownloadEvent::Progress(e) => p.lock().push(e.clone()),
            _ => {}
        }));
        let me = Arc::new(OnceLock::new());
        let _ = me.set(Arc::downgrade(&service));
        Self { folder, settings, provider, pool, merger, service, history, progress, persisted, me }
    }

    fn item(&self) -> DownloadItem {
        self.service.item()
    }

    fn edit(&self, mut f: impl FnMut(&mut DownloadItem)) {
        self.service.update_item(&mut f);
    }

    fn info(&self, f: impl FnOnce(&mut RemoteFileInfo)) {
        f(&mut self.provider.info.lock());
    }

    fn history(&self) -> Vec<DownloadStatus> {
        self.history.lock().clone()
    }

    fn spawn_start(&self) -> tokio::task::JoinHandle<Result<()>> {
        let service = Arc::clone(&self.service);
        tokio::spawn(async move { service.start().await })
    }

    /// Starts with a pool that blocks, waits for it, pauses, and waits for the run to end.
    async fn start_and_pause(&self) {
        self.pool.block_until_cancelled.store(true, Ordering::SeqCst);
        let running = self.spawn_start();
        self.pool.started.notified().await;
        self.service.pause();
        running.await.unwrap().unwrap();
    }
}

fn version(etag: &str) -> RemoteFileVersion {
    RemoteFileVersion::new(Some(etag.into()), None, Some("example.com".into()))
}

fn info(etag: Option<&str>) -> RemoteFileInfo {
    RemoteFileInfo {
        content_length: 1000,
        content_type: "application/octet-stream".into(),
        supports_range_requests: true,
        final_url: "https://example.com/file.bin".into(),
        etag: etag.map(str::to_string),
        ..Default::default()
    }
}

fn change(url: &str, info: RemoteFileInfo) -> AddressChange {
    AddressChange { url: url.into(), file_info: info, credentials: None, headers: None, discard_progress: false }
}

// ---------------------------------------------------------------- tests --

#[tokio::test]
async fn a_download_walks_from_connecting_to_completed() {
    let f = Fixture::new();
    f.service.start().await.unwrap();
    assert_eq!(f.history(), [DownloadStatus::Connecting, DownloadStatus::Downloading, DownloadStatus::Merging, DownloadStatus::Completed]);
    assert!(f.item().completed_at.is_some());
}

#[tokio::test]
async fn the_metadata_resolved_up_front_lands_on_the_item() {
    let f = Fixture::new();
    f.info(|i| {
        i.content_length = 4096;
        i.content_type = "application/zip".into();
        i.supports_range_requests = true;
    });
    f.service.start().await.unwrap();
    let item = f.item();
    assert_eq!(item.total_bytes, 4096);
    assert_eq!(item.content_type, "application/zip");
    assert!(item.supports_resume);
}

#[tokio::test]
async fn a_temp_folder_is_claimed_under_the_configured_root() {
    let f = Fixture::new();
    f.service.start().await.unwrap();
    let temp = f.item().temp_folder_path;
    assert!(temp.starts_with(&f.settings.current().temp_folder));
    let name = Path::new(&temp).file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(name.len(), 32);
    assert!(Uuid::try_parse(&name).is_ok());
}

#[tokio::test]
async fn progress_is_pinned_at_the_total_for_the_whole_merge() {
    // Every byte is on disk before the merge starts. Routing merge progress back
    // through downloaded_bytes is what made the bar visibly rewind.
    let f = Fixture::new();
    *f.merger.report_during_merge.lock() = vec![250, 500, 1000];
    f.service.start().await.unwrap();

    let merges: Vec<DownloadProgress> = f.progress.lock().iter().filter(|p| p.merge_percentage.is_some()).cloned().collect();
    assert!(!merges.is_empty());
    assert!(merges.iter().all(|p| p.downloaded_bytes == 1000));
    assert_eq!(merges.iter().map(|p| p.merge_percentage.unwrap()).collect::<Vec<_>>(), [25.0, 50.0, 100.0]);
}

#[tokio::test]
async fn a_pool_that_could_not_finish_reports_the_connection_s_own_error() {
    // "404 (Not Found)" is a message a user can act on; the generic line is one
    // they can only report.
    let f = Fixture::new();
    f.pool.succeed.store(false, Ordering::SeqCst);
    *f.pool.failure_message.lock() = Some("Connection #3: 404 (Not Found)".into());
    f.service.start().await.unwrap();
    assert_eq!(f.item().status, DownloadStatus::Failed);
    assert_eq!(f.item().error_message.as_deref(), Some("Connection #3: 404 (Not Found)"));
}

#[tokio::test]
async fn a_failure_with_nothing_recorded_still_says_something() {
    let f = Fixture::new();
    f.pool.succeed.store(false, Ordering::SeqCst);
    f.service.start().await.unwrap();
    assert_eq!(f.item().status, DownloadStatus::Failed);
    assert!(f.item().error_message.is_some_and(|m| !m.trim().is_empty()));
}

#[tokio::test]
async fn a_stale_error_from_a_previous_attempt_is_not_reported_again() {
    let f = Fixture::new();
    f.pool.succeed.store(false, Ordering::SeqCst);
    *f.pool.failure_message.lock() = Some("Connection #3: the first thing that went wrong".into());
    f.service.start().await.unwrap();

    *f.pool.failure_message.lock() = None;
    f.service.retry().await.unwrap();
    assert!(!f.item().error_message.unwrap_or_default().contains("the first thing"));
}

#[tokio::test]
async fn a_failure_is_surfaced_rather_than_returned_to_the_caller() {
    let f = Fixture::new();
    *f.provider.throw.lock() = Some(CoreError::Io("the name does not resolve".into()));
    f.service.start().await.unwrap();
    assert_eq!(f.item().status, DownloadStatus::Failed);
    assert_eq!(f.item().error_message.as_deref(), Some("the name does not resolve"));
}

#[tokio::test]
async fn starting_a_download_that_is_already_running_does_nothing() {
    let f = Fixture::new();
    f.edit(|i| i.status = DownloadStatus::Downloading);
    f.service.start().await.unwrap();
    assert_eq!(f.pool.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pause_stops_the_transfer_and_leaves_the_chunks_alone() {
    let f = Fixture::new();
    f.start_and_pause().await;
    assert_eq!(f.item().status, DownloadStatus::Paused);
    assert!(!f.merger.merged.load(Ordering::SeqCst));
}

#[test]
fn pause_is_a_no_op_for_something_not_in_flight() {
    let f = Fixture::new();
    f.edit(|i| i.status = DownloadStatus::Completed);
    f.service.pause();
    assert_eq!(f.item().status, DownloadStatus::Completed);
}

#[test]
fn stop_discards_the_download() {
    let f = Fixture::new();
    f.edit(|i| i.status = DownloadStatus::Paused);
    f.service.stop();
    assert_eq!(f.item().status, DownloadStatus::Cancelled);
}

#[tokio::test]
async fn stop_does_not_rewrite_a_download_that_has_already_finished() {
    // DownloadManager calls stop() on its way through remove(), so removing a
    // completed download used to persist it as cancelled and announce that status
    // to every open window a moment before the row disappeared.
    let f = Fixture::new();
    f.service.start().await.unwrap();
    f.history.lock().clear();
    f.service.stop();
    assert_eq!(f.item().status, DownloadStatus::Completed);
    assert!(f.history().is_empty());
}

#[tokio::test]
async fn retry_re_resolves_the_file_in_case_the_server_side_changed() {
    let f = Fixture::new();
    f.pool.succeed.store(false, Ordering::SeqCst);
    f.service.start().await.unwrap();
    assert_eq!(f.provider.probes.load(Ordering::SeqCst), 1);

    f.pool.succeed.store(true, Ordering::SeqCst);
    f.service.retry().await.unwrap();
    assert_eq!(f.provider.probes.load(Ordering::SeqCst), 2);
    assert_eq!(f.item().status, DownloadStatus::Completed);
}

#[tokio::test]
async fn resume_reuses_the_metadata_it_already_has() {
    let f = Fixture::new();
    f.start_and_pause().await;
    f.pool.block_until_cancelled.store(false, Ordering::SeqCst);
    f.service.resume().await.unwrap();
    // One probe, not two: the file was resolved when the download started.
    assert_eq!(f.provider.probes.load(Ordering::SeqCst), 1);
    assert_eq!(f.item().status, DownloadStatus::Completed);
}

#[tokio::test]
async fn every_status_change_is_persisted() {
    let f = Fixture::new();
    f.service.start().await.unwrap();
    assert_eq!(f.history().len() as i32, f.persisted.load(Ordering::SeqCst));
}

#[tokio::test]
async fn repeated_pauses_and_resumes_leave_nothing_behind() {
    // Each run used to leak the previous run's CancellationTokenSource.
    let f = Fixture::new();
    for _ in 0..5 {
        f.start_and_pause().await;
    }
    f.pool.block_until_cancelled.store(false, Ordering::SeqCst);
    f.service.resume().await.unwrap();
    assert_eq!(f.item().status, DownloadStatus::Completed);
}

// ------------------------------------------- a file replaced on the server --

#[tokio::test]
async fn a_resume_throws_away_the_chunks_of_a_file_the_server_has_since_replaced() {
    // The corruption this exists for: chunks of the old version and the tail of
    // the new one, merged into a file that is neither, with no error.
    let f = Fixture::new();
    f.edit(|i| {
        i.total_bytes = 1000;
        i.downloaded_bytes = 400;
        i.remote_version = Some(version("\"v1\""));
    });
    f.info(|i| i.etag = Some("\"v2\"".into()));
    f.service.start().await.unwrap();

    assert_eq!(f.pool.discards.load(Ordering::SeqCst), 1);
    assert_eq!(f.item().remote_version.unwrap().etag.as_deref(), Some("\"v2\""));
    assert_eq!(f.item().status, DownloadStatus::Completed);
}

#[tokio::test]
async fn a_resume_of_the_version_already_on_disk_keeps_every_chunk() {
    let f = Fixture::new();
    f.edit(|i| {
        i.total_bytes = 1000;
        i.remote_version = Some(version("\"v1\""));
    });
    f.info(|i| i.etag = Some("\"v1\"".into()));
    f.service.start().await.unwrap();
    assert_eq!(f.pool.discards.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_file_that_changed_length_is_a_different_file_even_without_validators() {
    // A length change also moves every segment boundary, so the old chunks would
    // resume at offsets that no longer belong to them.
    let f = Fixture::new();
    f.edit(|i| i.total_bytes = 900);
    f.info(|i| i.content_length = 1000);
    f.service.start().await.unwrap();
    assert_eq!(f.pool.discards.load(Ordering::SeqCst), 1);
    assert_eq!(f.item().total_bytes, 1000);
}

#[tokio::test]
async fn a_download_recorded_before_versions_were_kept_adopts_one_without_losing_anything() {
    let f = Fixture::new();
    f.edit(|i| {
        i.total_bytes = 1000;
        i.remote_version = None;
    });
    f.info(|i| i.etag = Some("\"v1\"".into()));
    f.service.start().await.unwrap();
    assert_eq!(f.pool.discards.load(Ordering::SeqCst), 0);
    assert_eq!(f.item().remote_version.unwrap().etag.as_deref(), Some("\"v1\""));
}

#[tokio::test]
async fn a_change_a_connection_reports_is_confirmed_by_a_probe_before_the_chunks_go() {
    let f = Fixture::new();
    f.pool.throw_changed_times.store(1, Ordering::SeqCst);
    *f.provider.respond.lock() = Some(Box::new(|probe| info(Some(if probe == 1 { "\"v1\"" } else { "\"v2\"" }))));
    f.service.start().await.unwrap();

    assert_eq!(f.provider.probes.load(Ordering::SeqCst), 2);
    assert_eq!(f.pool.discards.load(Ordering::SeqCst), 1);
    let item = f.item();
    assert!(!item.if_range_unsupported);
    assert_eq!(item.remote_version.unwrap().etag.as_deref(), Some("\"v2\""));
    assert_eq!(item.status, DownloadStatus::Completed);
}

#[tokio::test]
async fn a_change_the_probe_contradicts_is_the_condition_s_fault_and_costs_no_chunks() {
    // A server that does not evaluate If-Range, or whose edges mint their own
    // ETags, answers with a whole file for a version that never changed.
    // Discarding on its word would cost hours for nothing.
    let f = Fixture::new();
    f.pool.throw_changed_times.store(1, Ordering::SeqCst);
    *f.provider.respond.lock() = Some(Box::new(|_| info(Some("\"v1\""))));
    f.service.start().await.unwrap();

    assert_eq!(f.pool.discards.load(Ordering::SeqCst), 0);
    assert!(f.item().if_range_unsupported);
    assert_eq!(f.item().status, DownloadStatus::Completed);
}

#[tokio::test]
async fn a_file_that_never_holds_still_fails_instead_of_restarting_forever() {
    let f = Fixture::new();
    f.pool.throw_changed_times.store(i32::MAX, Ordering::SeqCst);
    *f.provider.respond.lock() = Some(Box::new(|probe| info(Some(&format!("\"v{probe}\"")))));
    f.service.start().await.unwrap();
    assert_eq!(f.item().status, DownloadStatus::Failed);
    assert!(f.item().error_message.unwrap().contains("kept changing"));
}

#[tokio::test]
async fn a_pause_during_the_confirming_probe_is_a_pause() {
    let f = Fixture::new();
    f.pool.throw_changed_times.store(1, Ordering::SeqCst);
    let me = Arc::clone(&f.me);
    *f.provider.respond.lock() = Some(Box::new(move |probe| {
        if probe == 2
            && let Some(service) = me.get().and_then(Weak::upgrade)
        {
            service.pause();
        }
        info(Some("\"v1\""))
    }));
    f.service.start().await.unwrap();
    assert_eq!(f.item().status, DownloadStatus::Paused);
}

// ------------------------------------------------------------- disk space --

#[tokio::test]
async fn a_download_that_will_not_fit_fails_before_its_first_byte_and_says_where() {
    // It used to fill the drive for hours and die in the merge at 100%.
    let f = Fixture::with(true, Some(Arc::new(|_| Some(999))));
    f.info(|i| i.content_length = 1000);
    f.service.start().await.unwrap();
    assert_eq!(f.item().status, DownloadStatus::Failed);
    assert!(f.item().error_message.unwrap().contains("Not enough free space"));
    assert_eq!(f.pool.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn what_is_already_downloaded_does_not_need_room_again() {
    let f = Fixture::with(true, Some(Arc::new(|_| Some(700))));
    f.info(|i| i.content_length = 1000);
    f.edit(|i| {
        i.total_bytes = 1000;
        i.downloaded_bytes = 400;
    });
    f.service.start().await.unwrap();
    assert_eq!(f.item().status, DownloadStatus::Completed);
}

#[tokio::test]
async fn a_drive_that_cannot_be_measured_is_not_a_reason_to_refuse() {
    let f = Fixture::with(true, Some(Arc::new(|_| None)));
    f.service.start().await.unwrap();
    assert_eq!(f.item().status, DownloadStatus::Completed);
}

// ---------------------------------------------------- an interrupted merge --

#[tokio::test]
async fn a_merge_interrupted_by_a_restart_finishes_without_asking_the_server() {
    // Every byte is already on disk. Probing first would fail a finished download
    // over a link that has since expired, and running the pool would misread
    // chunks the merge has already folded together.
    let f = Fixture::new();
    f.edit(|i| i.status = DownloadStatus::Paused);
    f.merger.interrupted.store(true, Ordering::SeqCst);
    *f.provider.throw.lock() = Some(CoreError::http(Some(403), "403 (Forbidden)"));
    f.service.start().await.unwrap();

    assert_eq!(f.item().status, DownloadStatus::Completed);
    assert_eq!(f.provider.probes.load(Ordering::SeqCst), 0);
    assert_eq!(f.pool.runs.load(Ordering::SeqCst), 0);
    let lists = f.merger.merged_chunk_lists.lock().clone();
    assert_eq!(lists.len(), 1);
    assert!(lists[0].is_empty());
}

#[test]
fn stopping_a_download_discards_an_interrupted_merge_with_it() {
    let f = Fixture::new();
    f.edit(|i| i.status = DownloadStatus::Paused);
    f.merger.interrupted.store(true, Ordering::SeqCst);
    f.service.stop();
    assert_eq!(f.merger.discards.load(Ordering::SeqCst), 1);
}

// --------------------------------------------------------------- checksum --

#[tokio::test]
async fn a_file_that_matches_its_checksum_is_verified_and_then_complete() {
    let f = Fixture::new();
    f.merger.write_destination.store(true, Ordering::SeqCst); // writes 01 02 03
    f.edit(|i| i.expected_checksum = Some("sha256:039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81".into()));
    f.service.start().await.unwrap();

    assert_eq!(f.item().status, DownloadStatus::Completed);
    assert_eq!(f.item().checksum_result, ChecksumResult::Match);
    let tail: Vec<DownloadStatus> = f.history().into_iter().skip_while(|s| *s != DownloadStatus::Merging).collect();
    assert_eq!(tail, [DownloadStatus::Merging, DownloadStatus::Verifying, DownloadStatus::Completed]);
}

#[tokio::test]
async fn a_file_that_does_not_match_its_checksum_fails_instead_of_completing() {
    // A damaged or substituted file must not open from a "Download complete"
    // window, or set off the shutdown queued behind it.
    let f = Fixture::new();
    f.merger.write_destination.store(true, Ordering::SeqCst);
    f.edit(|i| i.expected_checksum = Some(format!("sha256:{}", "0".repeat(64))));
    f.service.start().await.unwrap();

    let item = f.item();
    assert_eq!(item.status, DownloadStatus::Failed);
    assert_eq!(item.checksum_result, ChecksumResult::Mismatch);
    assert!(item.error_message.unwrap().contains("SHA-256"));
    assert!(!f.history().contains(&DownloadStatus::Completed));
    assert!(item.completed_at.is_none());
}

#[tokio::test]
async fn a_download_without_a_checksum_is_never_verified() {
    let f = Fixture::new();
    f.merger.write_destination.store(true, Ordering::SeqCst);
    f.service.start().await.unwrap();
    assert!(!f.history().contains(&DownloadStatus::Verifying));
    assert_eq!(f.item().checksum_result, ChecksumResult::NotChecked);
}

// ---------------------------------------------------- the mark of the web --

#[tokio::test]
async fn a_finished_file_is_marked_as_coming_from_the_internet_and_the_page_it_came_from() {
    // A download handed over from Chrome used to arrive with fewer protections
    // than letting Chrome save it: no Zone.Identifier, so no SmartScreen check and
    // no Protected View.
    let f = Fixture::new();
    f.merger.write_destination.store(true, Ordering::SeqCst);
    f.edit(|i| i.headers = Headers::from([("referer".to_string(), "https://example.com/downloads".to_string())]));
    f.service.start().await.unwrap();

    let stream = crate::platform::alternate_stream::read(&f.item().full_path(), mark_of_the_web::STREAM_NAME).unwrap();
    let stream = String::from_utf8_lossy(&stream);
    assert!(stream.contains("ZoneId=3"));
    assert!(stream.contains("HostUrl=https://example.com/file.bin"));
    assert!(stream.contains("ReferrerUrl=https://example.com/downloads"));
}

#[tokio::test]
async fn no_mark_is_written_where_policy_says_to_keep_no_zone_information() {
    let f = Fixture::with(false, Some(Arc::new(|_| None)));
    f.merger.write_destination.store(true, Ordering::SeqCst);
    f.service.start().await.unwrap();
    assert!(crate::platform::alternate_stream::read(&f.item().full_path(), mark_of_the_web::STREAM_NAME).is_err());
}

// ---------------------------------------------------------- a new address --

#[test]
fn a_new_address_keeps_the_bytes_already_on_disk() {
    // An expired signed link: the file is the same file, only the permission to
    // ask for it ran out.
    let f = Fixture::new();
    f.edit(|i| {
        i.status = DownloadStatus::Failed;
        i.error_message = Some("Response status code does not indicate success: 403 (Forbidden).".into());
        i.downloaded_bytes = 400;
    });
    assert!(f.service.change_address(change("https://cdn.example.com/file.bin?sig=new", info(Some("\"v1\"")))));

    let item = f.item();
    assert_eq!(item.url, "https://cdn.example.com/file.bin?sig=new");
    assert_eq!(f.pool.discards.load(Ordering::SeqCst), 0);
    assert_eq!(item.downloaded_bytes, 400);
    assert!(item.error_message.is_none());
    assert_eq!(item.status, DownloadStatus::Failed);
}

#[test]
fn a_new_address_the_user_chose_to_start_over_with_discards_the_chunks() {
    let f = Fixture::new();
    f.edit(|i| {
        i.status = DownloadStatus::Paused;
        i.downloaded_bytes = 400;
    });
    f.service.change_address(AddressChange { discard_progress: true, ..change("https://example.com/v2.bin", info(Some("\"v2\""))) });
    assert_eq!(f.pool.discards.load(Ordering::SeqCst), 1);
    assert_eq!(f.item().downloaded_bytes, 0);
}

#[test]
fn a_new_address_brings_its_own_login_and_headers_or_keeps_the_old_ones() {
    let f = Fixture::new();
    f.edit(|i| {
        i.status = DownloadStatus::Paused;
        i.credentials = Some(DownloadCredentials::new("alice", "old"));
        i.headers.insert("Cookie".into(), "session=expired".into());
    });

    f.service.change_address(change("https://example.com/a", info(Some("\"v1\""))));
    assert_eq!(f.item().credentials.unwrap().password, "old");
    assert_eq!(crate::models::header_get(&f.item().headers, "Cookie"), Some("session=expired"));

    f.service.change_address(AddressChange {
        credentials: Some(DownloadCredentials::new("alice", "new")),
        headers: Some(Headers::from([("Cookie".to_string(), "session=fresh".to_string())])),
        ..change("https://example.com/b", info(Some("\"v1\"")))
    });
    assert_eq!(f.item().credentials.unwrap().password, "new");
    assert_eq!(crate::models::header_get(&f.item().headers, "cookie"), Some("session=fresh"));
}

#[test]
fn a_new_server_is_not_held_to_the_old_one_s_broken_if_range() {
    let f = Fixture::new();
    f.edit(|i| {
        i.status = DownloadStatus::Paused;
        i.if_range_unsupported = true;
    });
    let probed = RemoteFileInfo { final_url: "https://other.example.com/file.bin".into(), ..info(Some("\"v1\"")) };
    f.service.change_address(change("https://other.example.com/file.bin", probed));

    assert!(!f.item().if_range_unsupported);
    assert_eq!(f.item().remote_version.unwrap().host.as_deref(), Some("other.example.com"));
}

#[tokio::test]
async fn the_address_of_a_running_download_cannot_be_changed_under_its_connections() {
    let f = Fixture::new();
    f.pool.block_until_cancelled.store(true, Ordering::SeqCst);
    let running = f.spawn_start();
    f.pool.started.notified().await;

    assert!(!f.service.change_address(change("https://example.com/new", info(Some("\"v1\"")))));
    assert_eq!(f.item().url, "https://example.com/file.bin");

    f.service.pause();
    running.await.unwrap().unwrap();
}

#[test]
fn a_finished_download_has_no_address_to_refresh() {
    let f = Fixture::new();
    f.edit(|i| i.status = DownloadStatus::Completed);
    assert!(!f.service.change_address(change("https://example.com/new", info(Some("\"v1\"")))));
}

#[tokio::test]
async fn the_next_start_probes_the_new_address_rather_than_reusing_the_old_answer() {
    let f = Fixture::new();
    f.start_and_pause().await;

    f.service.change_address(change("https://example.com/renewed", info(None)));
    f.pool.block_until_cancelled.store(false, Ordering::SeqCst);
    f.service.resume().await.unwrap();

    assert_eq!(f.provider.probes.load(Ordering::SeqCst), 2);
    assert_eq!(f.provider.last_url.lock().as_deref(), Some("https://example.com/renewed"));
}

#[test]
fn changing_the_address_is_announced_so_every_window_shows_it() {
    let f = Fixture::new();
    f.edit(|i| i.status = DownloadStatus::Paused);
    let before = f.persisted.load(Ordering::SeqCst);

    f.service.change_address(change("https://example.com/new", info(Some("\"v1\""))));
    assert_eq!(f.history(), [DownloadStatus::Paused]);
    assert!(f.persisted.load(Ordering::SeqCst) > before);
    drop(f.folder);
}
