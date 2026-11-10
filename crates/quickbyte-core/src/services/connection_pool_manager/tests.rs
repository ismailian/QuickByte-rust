//! The pool with fake connections, so the split, the resume offsets, the chunk
//! bookkeeping, recovery and the per-connection commands can be checked without
//! a server. Ports `ConnectionPoolManagerTests.cs`, `ConnectionRecoveryTests.cs`
//! and the fake-connection half of `DynamicSegmentationTests.cs`; the end-to-end
//! halves with real connections are in `tests/engine.rs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::helpers::disk_space::scratch::TempFolder;
use crate::models::DownloadCredentials;
use crate::net::segment_range::SegmentRange;
use crate::traits::{ConnectionPoolManager as _, ResizableConnection};

const MIB: i64 = 1024 * 1024;

fn item(folder: &TempFolder, connections: i32) -> DownloadItem {
    DownloadItem {
        url: "https://example.com/file.bin".into(),
        connections_count: connections,
        temp_folder_path: folder.path_str(),
        ..Default::default()
    }
}

fn info(length: i64, ranges: bool) -> RemoteFileInfo {
    RemoteFileInfo { content_length: length, supports_range_requests: ranges, ..Default::default() }
}

fn settings(dynamic: bool) -> Arc<DownloadSettings> {
    Arc::new(DownloadSettings { dynamic_segmentation: dynamic, max_retries: 0, retry_delay_milliseconds: 0, ..Default::default() })
}

fn chunk_names(folder: &TempFolder) -> Vec<String> {
    let mut names: Vec<String> =
        std::fs::read_dir(folder.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

async fn until(condition: impl Fn() -> bool) {
    for _ in 0..400 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the condition never held");
}

type RunHandle = tokio::task::JoinHandle<Result<bool>>;

fn spawn_run(pool: &Arc<ConnectionPoolManager>, item: DownloadItem, info: RemoteFileInfo, settings: Arc<DownloadSettings>, cancel: CancellationToken) -> RunHandle {
    let pool = Arc::clone(pool);
    tokio::spawn(async move { pool.run(&item, &info, settings, cancel).await })
}

fn status_of(pool: &ConnectionPoolManager, id: i32) -> Option<ConnectionStatus> {
    pool.snapshot().into_iter().find(|c| c.connection_id == id).map(|c| c.status)
}

// ---------------------------------------------------------------------------
// Plain fakes (ConnectionPoolManagerTests)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeConnectionFactory {
    created: Mutex<Vec<Arc<FakeConnection>>>,
    /// Id of a connection that should fail instead of transferring.
    fail_connection: Option<i32>,
    /// Id of a connection that finds the file changed on the server.
    changed_connection: Option<i32>,
    /// Every connection but `changed_connection` runs until it is cancelled.
    block_others_until_cancelled: bool,
}

impl FakeConnectionFactory {
    fn created(&self) -> Vec<Arc<FakeConnection>> {
        self.created.lock().clone()
    }
}

impl ConnectionFactory for FakeConnectionFactory {
    fn create(&self, spec: ConnectionSpec) -> Arc<dyn DownloadConnection> {
        let connection = Arc::new(FakeConnection {
            id: spec.connection_id,
            start: spec.range_start,
            end: spec.range_end,
            already: spec.already_downloaded,
            path: spec.chunk_file_path,
            options: spec.options,
            should_fail: self.fail_connection == Some(spec.connection_id),
            finds_file_changed: self.changed_connection == Some(spec.connection_id),
            blocks_until_cancelled: self.block_others_until_cancelled && self.changed_connection != Some(spec.connection_id),
            bytes: AtomicI64::new(0),
            status: Mutex::new(ConnectionStatus::Idle),
            last_error: Mutex::new(None),
            was_cancelled: AtomicBool::new(false),
        });
        self.created.lock().push(Arc::clone(&connection));
        connection
    }
}

struct FakeConnection {
    id: i32,
    start: i64,
    end: i64,
    already: i64,
    path: String,
    options: RequestOptions,
    should_fail: bool,
    finds_file_changed: bool,
    blocks_until_cancelled: bool,
    bytes: AtomicI64,
    status: Mutex<ConnectionStatus>,
    last_error: Mutex<Option<String>>,
    was_cancelled: AtomicBool,
}

#[async_trait]
impl DownloadConnection for FakeConnection {
    fn connection_id(&self) -> i32 {
        self.id
    }
    fn range_start(&self) -> i64 {
        self.start
    }
    fn range_end(&self) -> i64 {
        self.end
    }
    fn bytes_downloaded(&self) -> i64 {
        self.bytes.load(Ordering::SeqCst)
    }
    fn retry_count(&self) -> i32 {
        0
    }
    fn status(&self) -> ConnectionStatus {
        *self.status.lock()
    }
    fn last_error(&self) -> Option<String> {
        self.last_error.lock().clone()
    }
    fn chunk_file_path(&self) -> &str {
        &self.path
    }

    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        if self.finds_file_changed {
            // Give the siblings a moment to be mid-transfer.
            tokio::time::sleep(Duration::from_millis(20)).await;
            return Err(CoreError::RemoteFileChanged("changed".into()));
        }
        if self.blocks_until_cancelled {
            cancel.cancelled().await;
            self.was_cancelled.store(true, Ordering::SeqCst);
            return Err(CoreError::Cancelled);
        }
        if self.should_fail {
            *self.status.lock() = ConnectionStatus::Failed;
            *self.last_error.lock() = Some("the server hung up".into());
            return Err(CoreError::Io("the server hung up".into()));
        }
        self.bytes.store(self.end - self.start + 1, Ordering::SeqCst);
        *self.status.lock() = ConnectionStatus::Finished;
        Ok(())
    }
}

fn plain_pool(factory: &Arc<FakeConnectionFactory>) -> ConnectionPoolManager {
    ConnectionPoolManager::new(Arc::clone(factory) as Arc<dyn ConnectionFactory>, None).with_recovery_base_delay(Duration::ZERO)
}

async fn run_once(factory: &Arc<FakeConnectionFactory>, item: DownloadItem, info: RemoteFileInfo) -> (ConnectionPoolManager, Result<bool>) {
    let pool = plain_pool(factory);
    let result = pool.run(&item, &info, Arc::new(DownloadSettings::default()), CancellationToken::new()).await;
    (pool, result)
}

#[tokio::test]
async fn a_server_that_supports_ranges_gets_the_download_s_connection_count() {
    let folder = TempFolder::new();
    let factory = Arc::new(FakeConnectionFactory::default());
    run_once(&factory, item(&folder, 8), info(1000, true)).await.1.unwrap();

    let created = factory.created();
    assert_eq!(created.len(), 8);
    assert_eq!(created[0].start, 0);
    assert_eq!(created[7].end, 999);
}

#[tokio::test]
async fn a_server_without_range_support_drops_to_a_single_connection() {
    let folder = TempFolder::new();
    let factory = Arc::new(FakeConnectionFactory::default());
    run_once(&factory, item(&folder, 8), info(1000, false)).await.1.unwrap();

    let created = factory.created();
    assert_eq!(created.len(), 1);
    assert_eq!((created[0].start, created[0].end), (0, 999));
}

#[tokio::test]
async fn an_unknown_size_drops_to_a_single_open_ended_connection() {
    let folder = TempFolder::new();
    let factory = Arc::new(FakeConnectionFactory::default());
    run_once(&factory, item(&folder, 8), info(0, true)).await.1.unwrap();

    let created = factory.created();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].end, UNBOUNDED_END);
}

#[tokio::test]
async fn the_connection_count_is_clamped_by_the_application_settings() {
    let folder = TempFolder::new();
    let factory = Arc::new(FakeConnectionFactory::default());
    run_once(&factory, item(&folder, 500), info(100_000, true)).await.1.unwrap();
    assert_eq!(factory.created().len(), DownloadSettings::MAX_CONNECTIONS as usize);
}

#[tokio::test]
async fn resume_offsets_come_from_the_length_of_the_chunks_on_disk() {
    // Not from a persisted counter: the bytes that are actually there are the
    // only thing that cannot be out of date.
    let folder = TempFolder::new();
    folder.write_file("part0.tmp", &[0; 120]);
    folder.write_file("part2.tmp", &[0; 45]);
    let factory = Arc::new(FakeConnectionFactory::default());
    run_once(&factory, item(&folder, 4), info(1000, true)).await.1.unwrap();

    let created = factory.created();
    assert_eq!(created[0].already, 120);
    assert_eq!(created[1].already, 0);
    assert_eq!(created[2].already, 45);
}

#[tokio::test]
async fn chunks_from_a_wider_split_are_thrown_away() {
    // A Retry re-resolves the file, and a server that has stopped advertising
    // byte ranges moves the download from eight connections to one. part0.tmp
    // would then be read as the head of the whole file when it holds only the
    // first eighth of it, and part1-7 would be merged in from nowhere.
    let folder = TempFolder::new();
    for i in 0..8 {
        folder.write_file(&format!("part{i}.tmp"), &[0; 125]);
    }
    let factory = Arc::new(FakeConnectionFactory::default());
    run_once(&factory, item(&folder, 8), info(1000, false)).await.1.unwrap();

    assert_eq!(factory.created()[0].already, 0);
    for (path, _) in chunk_files(&folder.path_str()) {
        assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
    }
}

#[tokio::test]
async fn chunks_from_a_narrower_split_are_thrown_away() {
    // The other direction: one connection's chunk holds a prefix of the whole
    // file, which is longer than the first range of the new eight-way split.
    let folder = TempFolder::new();
    folder.write_file("part0.tmp", &[0; 700]);
    let factory = Arc::new(FakeConnectionFactory::default());
    run_once(&factory, item(&folder, 8), info(1000, true)).await.1.unwrap();
    assert!(factory.created().iter().all(|c| c.already == 0));
}

#[tokio::test]
async fn chunks_from_the_same_split_are_kept() {
    // A run paused before every connection had opened its file leaves fewer
    // chunks than there are connections. That is an ordinary resume, not an
    // incompatible chunk set, and the bytes must survive it.
    let folder = TempFolder::new();
    folder.write_file("part0.tmp", &[0; 50]);
    folder.write_file("part1.tmp", &[0; 30]);
    let factory = Arc::new(FakeConnectionFactory::default());
    run_once(&factory, item(&folder, 4), info(1000, true)).await.1.unwrap();

    let created = factory.created();
    assert_eq!((created[0].already, created[1].already, created[2].already), (50, 30, 0));
}

#[tokio::test]
async fn ordered_chunk_paths_is_in_byte_order_whatever_order_the_connections_finished_in() {
    let folder = TempFolder::new();
    let factory = Arc::new(FakeConnectionFactory::default());
    let (pool, _) = run_once(&factory, item(&folder, 4), info(1000, true)).await;
    let names: Vec<String> = pool
        .ordered_chunk_paths()
        .iter()
        .map(|p| Path::new(p).file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["part0.tmp", "part1.tmp", "part2.tmp", "part3.tmp"]);
}

#[tokio::test]
async fn run_reports_success_only_when_every_connection_finished() {
    let folder = TempFolder::new();
    let all_good = Arc::new(FakeConnectionFactory::default());
    assert!(run_once(&all_good, item(&folder, 4), info(1000, true)).await.1.unwrap());

    let second = TempFolder::new();
    let one_failed = Arc::new(FakeConnectionFactory { fail_connection: Some(2), ..Default::default() });
    assert!(!run_once(&one_failed, item(&second, 4), info(1000, true)).await.1.unwrap());
}

#[tokio::test]
async fn a_connection_that_fails_is_reported_rather_than_taking_the_pool_down() {
    let folder = TempFolder::new();
    let factory = Arc::new(FakeConnectionFactory { fail_connection: Some(1), ..Default::default() });
    let pool = plain_pool(&factory);
    let events = pool.subscribe();
    pool.run(&item(&folder, 4), &info(1000, true), Arc::new(DownloadSettings::default()), CancellationToken::new()).await.unwrap();

    let reported: Vec<String> = events
        .try_iter()
        .filter_map(|e| if let PoolEvent::ConnectionFailed(m) = e { Some(m) } else { None })
        .collect();
    assert!(reported.last().is_some_and(|m| m.contains("#1")), "{reported:?}");
}

#[tokio::test]
async fn progress_is_reported_with_a_final_exact_snapshot() {
    let folder = TempFolder::new();
    let factory = Arc::new(FakeConnectionFactory::default());
    let pool = plain_pool(&factory);
    let events = pool.subscribe();
    pool.run(&item(&folder, 4), &info(1000, true), Arc::new(DownloadSettings::default()), CancellationToken::new()).await.unwrap();

    let progress: Vec<i64> =
        events.try_iter().filter_map(|e| if let PoolEvent::Progress(p) = e { Some(p.downloaded_bytes) } else { None }).collect();
    assert!(!progress.is_empty());
    assert_eq!(*progress.last().unwrap(), 1000);
}

#[tokio::test]
async fn the_snapshot_describes_every_connection() {
    let folder = TempFolder::new();
    let factory = Arc::new(FakeConnectionFactory::default());
    let (pool, _) = run_once(&factory, item(&folder, 4), info(1000, true)).await;

    let snapshot = pool.snapshot();
    assert_eq!(snapshot.len(), 4);
    assert!(snapshot.iter().all(|c| c.status == ConnectionStatus::Finished));
    assert_eq!(snapshot.iter().map(|c| c.connection_id).collect::<Vec<_>>(), [0, 1, 2, 3]);
}

#[tokio::test]
async fn every_connection_of_a_download_is_handed_the_same_request_options() {
    // A probe that authenticates and connections that do not resolve a real
    // size and then fetch eight copies of a login page.
    let folder = TempFolder::new();
    let mut download = item(&folder, 4);
    download.credentials = Some(DownloadCredentials::new("alice", "hunter2"));
    download.headers.insert("Cookie".into(), "session=abc".into());

    let factory = Arc::new(FakeConnectionFactory::default());
    run_once(&factory, download, info(1000, true)).await.1.unwrap();

    let created = factory.created();
    for c in &created {
        assert_eq!(c.options.credentials.as_ref().unwrap().user_name, "alice");
        assert_eq!(c.options.headers.as_ref().unwrap()["Cookie"], "session=abc");
        assert_eq!(c.options, created[0].options);
    }
}

#[tokio::test]
async fn a_connection_that_finds_the_file_changed_stops_every_sibling_and_reports_it() {
    // Every chunk belongs to the version that changed, not only the one whose
    // connection noticed. The siblings would otherwise go on appending bytes of
    // a file that no longer exists.
    let folder = TempFolder::new();
    let factory =
        Arc::new(FakeConnectionFactory { changed_connection: Some(2), block_others_until_cancelled: true, ..Default::default() });
    let error = run_once(&factory, item(&folder, 4), info(1000, true)).await.1.unwrap_err();
    assert!(matches!(error, CoreError::RemoteFileChanged(_)));
    assert!(factory.created().iter().filter(|c| c.id != 2).all(|c| c.was_cancelled.load(Ordering::SeqCst)));
}

#[tokio::test]
async fn a_changed_file_is_not_reported_as_a_failed_connection() {
    let folder = TempFolder::new();
    let factory = Arc::new(FakeConnectionFactory { changed_connection: Some(0), ..Default::default() });
    let pool = plain_pool(&factory);
    let events = pool.subscribe();
    let error = pool.run(&item(&folder, 2), &info(1000, true), Arc::new(DownloadSettings::default()), CancellationToken::new()).await;
    assert!(matches!(error, Err(CoreError::RemoteFileChanged(_))));
    assert!(!events.try_iter().any(|e| matches!(e, PoolEvent::ConnectionFailed(_))));
}

#[tokio::test]
async fn a_pause_is_still_a_pause_and_not_a_changed_file() {
    let folder = TempFolder::new();
    let factory = Arc::new(FakeConnectionFactory { block_others_until_cancelled: true, ..Default::default() });
    let pool = Arc::new(plain_pool(&factory));
    let cancel = CancellationToken::new();
    let running = spawn_run(&pool, item(&folder, 2), info(1000, true), Arc::new(DownloadSettings::default()), cancel.clone());
    tokio::time::sleep(Duration::from_millis(20)).await;
    cancel.cancel();
    assert!(matches!(running.await.unwrap(), Err(CoreError::Cancelled)));
}

#[test]
fn discard_chunks_deletes_the_chunk_files_and_nothing_else() {
    let folder = TempFolder::new();
    folder.write_file("part0.tmp", &[1]);
    folder.write_file("part12.tmp", &[2]);
    folder.write_file("partial-notes.tmp", &[3]);
    folder.write_file("readme.txt", &[4]);

    plain_pool(&Arc::new(FakeConnectionFactory::default())).discard_chunks(&folder.path_str());
    assert_eq!(chunk_names(&folder), ["partial-notes.tmp", "readme.txt"]);
}

#[test]
fn discard_chunks_shrugs_at_a_folder_that_was_never_created() {
    let folder = TempFolder::new();
    plain_pool(&Arc::new(FakeConnectionFactory::default())).discard_chunks(&folder.file("missing"));
}

// ---------------------------------------------------------------------------
// Resizable fakes (DynamicSegmentationTests / ConnectionRecoveryTests)
// ---------------------------------------------------------------------------

type Blocks = Box<dyn Fn(i32) -> bool + Send + Sync>;
type Fails = Box<dyn Fn(i32, i32) -> bool + Send + Sync>;

/// A chunk that blocks runs until released, the rest finish at once.
struct ResizableFactory {
    me: std::sync::Weak<ResizableFactory>,
    blocks: Blocks,
    fails: Fails,
    finish_delay: Duration,
    fail_with: Option<fn() -> CoreError>,
    attempts: Mutex<HashMap<i32, i32>>,
    release: CancellationToken,
    /// (running now, most ever at once)
    running: Mutex<(i32, i32)>,
    created: Mutex<Vec<Arc<FakeResizable>>>,
}

impl ResizableFactory {
    fn new(blocks: impl Fn(i32) -> bool + Send + Sync + 'static) -> Arc<Self> {
        Self::build(blocks, Duration::ZERO, |_, _| false, None)
    }

    fn build(
        blocks: impl Fn(i32) -> bool + Send + Sync + 'static,
        finish_delay: Duration,
        fails: impl Fn(i32, i32) -> bool + Send + Sync + 'static,
        fail_with: Option<fn() -> CoreError>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            blocks: Box::new(blocks),
            fails: Box::new(fails),
            finish_delay,
            fail_with,
            attempts: Mutex::new(HashMap::new()),
            release: CancellationToken::new(),
            running: Mutex::new((0, 0)),
            created: Mutex::new(Vec::new()),
        })
    }

    fn release_all(&self) {
        self.release.cancel();
    }

    fn created(&self) -> Vec<Arc<FakeResizable>> {
        self.created.lock().clone()
    }

    fn max_concurrent(&self) -> i32 {
        self.running.lock().1
    }

    async fn when_created(&self, count: usize) {
        for _ in 0..400 {
            if self.created.lock().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("only {} connections were created", self.created.lock().len());
    }

    fn pool(self: &Arc<Self>, delay: Duration) -> Arc<ConnectionPoolManager> {
        Arc::new(ConnectionPoolManager::new(Arc::clone(self) as Arc<dyn ConnectionFactory>, None).with_recovery_base_delay(delay))
    }

    async fn run_one(&self, connection: &FakeResizable, cancel: &CancellationToken) -> Result<()> {
        {
            let mut running = self.running.lock();
            running.0 += 1;
            running.1 = running.1.max(running.0);
        }
        let result = async {
            if connection.blocks {
                tokio::select! {
                    _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                    _ = self.release.cancelled() => {}
                }
            } else if !self.finish_delay.is_zero() {
                tokio::select! {
                    _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                    _ = tokio::time::sleep(self.finish_delay) => {}
                }
            }
            Ok(())
        }
        .await;
        self.running.lock().0 -= 1;
        result
    }
}

impl ConnectionFactory for ResizableFactory {
    fn create(&self, spec: ConnectionSpec) -> Arc<dyn DownloadConnection> {
        let attempt = {
            let mut attempts = self.attempts.lock();
            let a = attempts.entry(spec.connection_id).or_insert(0);
            *a += 1;
            *a
        };
        let connection = Arc::new(FakeResizable {
            factory: self.me.clone(),
            id: spec.connection_id,
            range: SegmentRange::new(spec.range_start, spec.range_end),
            already: spec.already_downloaded,
            path: spec.chunk_file_path,
            blocks: (self.blocks)(spec.connection_id),
            fails: (self.fails)(spec.connection_id, attempt),
            failure: self.fail_with.map(|f| f()),
            bytes: AtomicI64::new(0),
            status: Mutex::new(ConnectionStatus::Idle),
        });
        self.created.lock().push(Arc::clone(&connection));
        connection
    }
}

struct FakeResizable {
    /// Set after creation (the factory hands itself to the connection's run).
    factory: std::sync::Weak<ResizableFactory>,
    id: i32,
    range: SegmentRange,
    already: i64,
    path: String,
    blocks: bool,
    fails: bool,
    failure: Option<CoreError>,
    bytes: AtomicI64,
    status: Mutex<ConnectionStatus>,
}

#[async_trait]
impl DownloadConnection for FakeResizable {
    fn connection_id(&self) -> i32 {
        self.id
    }
    fn range_start(&self) -> i64 {
        self.range.start()
    }
    fn range_end(&self) -> i64 {
        self.range.end()
    }
    fn bytes_downloaded(&self) -> i64 {
        self.bytes.load(Ordering::SeqCst)
    }
    fn retry_count(&self) -> i32 {
        0
    }
    fn status(&self) -> ConnectionStatus {
        *self.status.lock()
    }
    fn last_error(&self) -> Option<String> {
        None
    }
    fn chunk_file_path(&self) -> &str {
        &self.path
    }

    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        *self.status.lock() = ConnectionStatus::ReceivingData;
        if self.fails {
            *self.status.lock() = ConnectionStatus::Failed;
            return Err(self.failure.clone().unwrap_or_else(|| {
                CoreError::http(Some(429), "Response status code does not indicate success: 429 (Too Many Requests).")
            }));
        }
        let factory = self.factory.upgrade().expect("the factory is alive for the whole test");
        factory.run_one(self, &cancel).await?;
        self.bytes.store(self.range.len(), Ordering::SeqCst);
        *self.status.lock() = ConnectionStatus::Finished;
        Ok(())
    }

    fn as_resizable(&self) -> Option<&dyn ResizableConnection> {
        Some(self)
    }
}

impl ResizableConnection for FakeResizable {
    fn committed_end(&self) -> i64 {
        self.range.committed_end(self.range.start() + self.bytes_downloaded())
    }
    fn try_shrink_end(&self, new_end: i64) -> bool {
        self.range.try_shrink_end(new_end, self.range.start() + self.bytes_downloaded())
    }
}

fn resizable(blocks: impl Fn(i32) -> bool + Send + Sync + 'static) -> Arc<ResizableFactory> {
    ResizableFactory::new(blocks)
}

fn resizable_with(
    blocks: impl Fn(i32) -> bool + Send + Sync + 'static,
    finish_delay: Duration,
    fails: impl Fn(i32, i32) -> bool + Send + Sync + 'static,
    fail_with: Option<fn() -> CoreError>,
) -> Arc<ResizableFactory> {
    ResizableFactory::build(blocks, finish_delay, fails, fail_with)
}

fn sinfo(length: i64) -> RemoteFileInfo {
    info(length, true)
}

fn assert_covers_every_byte_once(connections: &[Arc<FakeResizable>], total: i64) {
    let mut ordered: Vec<&Arc<FakeResizable>> = connections.iter().collect();
    ordered.sort_by_key(|c| c.range.start());
    let mut next = 0;
    for c in ordered {
        assert_eq!(c.range.start(), next);
        next = c.range.end() + 1;
    }
    assert_eq!(next, total);
}

fn failures(events: &EventStream<PoolEvent>) -> Vec<String> {
    events.try_iter().filter_map(|e| if let PoolEvent::ConnectionFailed(m) = e { Some(m) } else { None }).collect()
}

fn auth_failure() -> CoreError {
    CoreError::auth("The server requires a user name and password.", false)
}

// -------------------------------------------------------------- splitting --

#[tokio::test]
async fn a_finished_connection_s_slot_goes_to_the_back_half_of_a_slow_one() {
    let folder = TempFolder::new();
    let factory = resizable(|chunk| chunk == 1);
    let pool = factory.pool(Duration::from_secs(2));
    let running = spawn_run(&pool, item(&folder, 2), sinfo(8 * MIB), settings(true), CancellationToken::new());

    factory.when_created(3).await;
    factory.release_all();
    assert!(running.await.unwrap().unwrap());

    let created = factory.created();
    let slow = created.iter().find(|c| c.id == 1).unwrap();
    assert!(slow.range.end() < 8 * MIB - 1, "the slow range was never shortened");
    assert_covers_every_byte_once(&created, 8 * MIB);

    let map = SegmentMap::read(&folder.path_str()).unwrap();
    assert!(map.describes(8 * MIB));
    assert_eq!(map.segments.len(), created.len());
}

#[tokio::test]
async fn no_more_connections_run_at_once_than_the_download_allows() {
    let folder = TempFolder::new();
    let factory = resizable_with(|chunk| chunk % 2 == 1, Duration::from_millis(5), |_, _| false, None);
    let pool = factory.pool(Duration::from_secs(2));
    let running = spawn_run(&pool, item(&folder, 3), sinfo(32 * MIB), settings(true), CancellationToken::new());

    factory.when_created(6).await;
    factory.release_all();
    running.await.unwrap().unwrap();
    assert!(factory.max_concurrent() <= 3, "{} ran at once", factory.max_concurrent());
}

#[tokio::test]
async fn a_range_too_small_to_repay_a_new_connection_is_left_alone() {
    let folder = TempFolder::new();
    let factory = resizable(|chunk| chunk == 1);
    let pool = factory.pool(Duration::from_secs(2));
    let running = spawn_run(&pool, item(&folder, 2), sinfo(MINIMUM_SPLIT_BYTES), settings(true), CancellationToken::new());

    tokio::time::sleep(Duration::from_millis(50)).await;
    factory.release_all();
    running.await.unwrap().unwrap();
    assert_eq!(factory.created().len(), 2);
}

#[tokio::test]
async fn turned_off_nothing_is_split() {
    let folder = TempFolder::new();
    let factory = resizable(|chunk| chunk == 1);
    let pool = factory.pool(Duration::from_secs(2));
    let running = spawn_run(&pool, item(&folder, 2), sinfo(8 * MIB), settings(false), CancellationToken::new());

    tokio::time::sleep(Duration::from_millis(50)).await;
    factory.release_all();
    running.await.unwrap().unwrap();
    assert_eq!(factory.created().len(), 2);
}

// ---------------------------------------------- a server that refuses new ones --

#[tokio::test]
async fn a_split_off_range_the_server_refuses_is_tried_again_and_the_download_still_completes() {
    // Measured against a real server: a split opens a new connection, and a rate
    // limiter answers 429. Splitting must never be why a download fails.
    let folder = TempFolder::new();
    let factory = resizable_with(|chunk| chunk == 1, Duration::ZERO, |chunk, attempt| chunk >= 2 && attempt == 1, None);
    let pool = factory.pool(Duration::from_millis(10));
    let events = pool.subscribe();
    let running = spawn_run(&pool, item(&folder, 2), sinfo(8 * MIB), settings(true), CancellationToken::new());

    factory.when_created(4).await; // 0, 1, the split-off 2, and 2 again
    factory.release_all();
    assert!(running.await.unwrap().unwrap());
    assert!(failures(&events).is_empty());
}

#[tokio::test]
async fn the_first_refusal_stops_any_more_splitting() {
    let folder = TempFolder::new();
    let factory = resizable_with(|chunk| chunk == 1, Duration::ZERO, |chunk, attempt| chunk == 2 && attempt == 1, None);
    let pool = factory.pool(Duration::from_millis(10));
    let running = spawn_run(&pool, item(&folder, 2), sinfo(64 * MIB), settings(true), CancellationToken::new());

    factory.when_created(4).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    factory.release_all();
    running.await.unwrap().unwrap();

    // Chunk 2 twice, and never a chunk 3.
    let mut ids: Vec<i32> = factory.created().iter().map(|c| c.id).collect();
    ids.sort();
    assert_eq!(ids, [0, 1, 2, 2]);
}

#[tokio::test]
async fn a_split_off_range_that_keeps_failing_is_reported_in_the_end() {
    let folder = TempFolder::new();
    let factory = resizable_with(|chunk| chunk == 1, Duration::ZERO, |chunk, _| chunk == 2, None);
    let pool = factory.pool(Duration::from_millis(5));
    let events = pool.subscribe();
    let running = spawn_run(&pool, item(&folder, 2), sinfo(8 * MIB), settings(true), CancellationToken::new());

    factory.when_created(2 + RECOVERY_ATTEMPTS as usize).await;
    factory.release_all();
    assert!(!running.await.unwrap().unwrap());
    assert_eq!(failures(&events).len(), 1);
    assert_eq!(factory.created().iter().filter(|c| c.id == 2).count(), RECOVERY_ATTEMPTS as usize);
}

// ----------------------------------------------------------------- resume --

#[tokio::test]
async fn a_resume_follows_the_saved_layout_and_its_chunks() {
    let folder = TempFolder::new();
    SegmentMap::write(&folder.path_str(), &SegmentMap::new(1000, vec![Segment::new(0, 0, 499), Segment::new(1, 500, 749), Segment::new(2, 750, 999)]));
    folder.write_file("part1.tmp", &[0; 100]);
    folder.write_file("part2.tmp", &[0; 250]);

    let factory = resizable(|_| false);
    factory.pool(Duration::ZERO).run(&item(&folder, 2), &sinfo(1000), settings(true), CancellationToken::new()).await.unwrap();

    let created = factory.created();
    let by = |id: i32| created.iter().find(|c| c.id == id).unwrap();
    assert_eq!((by(1).range.start(), by(1).range.end(), by(1).already), (500, 749, 100));
    assert_eq!((by(2).range.start(), by(2).range.end(), by(2).already), (750, 999, 250));
}

#[tokio::test]
async fn a_resume_with_more_unfinished_segments_than_connections_runs_them_a_few_at_a_time() {
    let folder = TempFolder::new();
    SegmentMap::write(
        &folder.path_str(),
        &SegmentMap::new(500, (0..5).map(|i| Segment::new(i, i as i64 * 100, i as i64 * 100 + 99)).collect()),
    );
    let factory = resizable_with(|_| false, Duration::from_millis(10), |_, _| false, None);
    factory.pool(Duration::ZERO).run(&item(&folder, 2), &sinfo(500), settings(true), CancellationToken::new()).await.unwrap();

    assert_eq!(factory.created().len(), 5);
    assert!(factory.max_concurrent() <= 2);
}

#[tokio::test]
async fn a_layout_for_a_file_of_another_length_is_replaced_and_its_chunks_discarded() {
    let folder = TempFolder::new();
    SegmentMap::write(&folder.path_str(), &SegmentMap::new(900, vec![Segment::new(0, 0, 449), Segment::new(7, 450, 899)]));
    folder.write_file("part7.tmp", &[0; 10]);

    let factory = resizable(|_| false);
    factory.pool(Duration::ZERO).run(&item(&folder, 2), &sinfo(1000), settings(true), CancellationToken::new()).await.unwrap();

    let mut ids: Vec<i32> = factory.created().iter().map(|c| c.id).collect();
    ids.sort();
    assert_eq!(ids, [0, 1]);
    assert!(!Path::new(&folder.file("part7.tmp")).exists());
    assert!(SegmentMap::read(&folder.path_str()).unwrap().describes(1000));
}

#[tokio::test]
async fn a_chunk_the_layout_does_not_name_discards_the_set_rather_than_trusting_it() {
    let folder = TempFolder::new();
    SegmentMap::write(&folder.path_str(), &SegmentMap::new(1000, vec![Segment::new(0, 0, 499), Segment::new(1, 500, 999)]));
    folder.write_file("part0.tmp", &[0; 100]);
    folder.write_file("part5.tmp", &[0; 100]);

    let factory = resizable(|_| false);
    factory.pool(Duration::ZERO).run(&item(&folder, 2), &sinfo(1000), settings(true), CancellationToken::new()).await.unwrap();
    assert!(factory.created().iter().all(|c| c.already == 0));
}

#[tokio::test]
async fn a_download_that_cannot_be_split_keeps_no_layout() {
    let folder = TempFolder::new();
    SegmentMap::write(&folder.path_str(), &SegmentMap::new(1000, vec![Segment::new(0, 0, 999)]));
    let factory = resizable(|_| false);
    factory.pool(Duration::ZERO).run(&item(&folder, 8), &info(1000, false), settings(true), CancellationToken::new()).await.unwrap();
    assert!(!Path::new(&folder.file(crate::services::segment_map::FILE_NAME)).exists());
}

#[test]
fn discarding_the_chunks_discards_the_layout_and_forgetting_it_keeps_the_chunks() {
    let folder = TempFolder::new();
    let pool = resizable(|_| false).pool(Duration::ZERO);
    let map_file = folder.file(crate::services::segment_map::FILE_NAME);

    SegmentMap::write(&folder.path_str(), &SegmentMap::new(1, vec![Segment::new(0, 0, 0)]));
    folder.write_file("part0.tmp", &[0]);
    pool.forget_layout(&folder.path_str());
    assert!(!Path::new(&map_file).exists());
    assert!(Path::new(&folder.file("part0.tmp")).exists());

    SegmentMap::write(&folder.path_str(), &SegmentMap::new(1, vec![Segment::new(0, 0, 0)]));
    pool.discard_chunks(&folder.path_str());
    assert!(!Path::new(&map_file).exists());
}

// --------------------------------------------------------------- recovery --

#[tokio::test]
async fn a_connection_that_runs_out_of_retries_is_replaced_and_its_chunk_dropped() {
    let folder = TempFolder::new();
    folder.write_file("part0.tmp", &[0; 100]);
    let factory = resizable_with(|_| false, Duration::ZERO, |chunk, attempt| chunk == 0 && attempt == 1, None);
    let pool = factory.pool(Duration::from_millis(5));
    let events = pool.subscribe();

    assert!(pool.run(&item(&folder, 2), &sinfo(8 * MIB), settings(true), CancellationToken::new()).await.unwrap());

    assert!(failures(&events).is_empty());
    let attempts: Vec<Arc<FakeResizable>> = factory.created().into_iter().filter(|c| c.id == 0).collect();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].already, 100);
    assert_eq!(attempts[1].already, 0); // from scratch, not from the dropped chunk
    assert_eq!(attempts[0].range.start(), attempts[1].range.start());
    assert_eq!(attempts[0].range.end(), attempts[1].range.end());
}

#[tokio::test]
async fn a_replacement_that_fails_too_waits_for_another_connection_to_finish() {
    // The second failure of one range says the server will not serve this many
    // connections at once, so the third try is not a third parallel connection.
    let folder = TempFolder::new();
    let factory = resizable_with(|chunk| chunk == 1, Duration::ZERO, |chunk, attempt| chunk == 0 && attempt <= 2, None);
    let pool = factory.pool(Duration::from_millis(5));
    let running = spawn_run(&pool, item(&folder, 2), sinfo(8 * MIB), settings(true), CancellationToken::new());

    factory.when_created(4).await; // 0, 1, the replacement of 0, and 0 waiting its turn
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!running.is_finished());
    let last_zero = factory.created().into_iter().rfind(|c| c.id == 0).unwrap();
    assert_eq!(last_zero.status(), ConnectionStatus::Idle);

    factory.release_all();
    assert!(running.await.unwrap().unwrap());
    assert_eq!(factory.created().iter().filter(|c| c.id == 0).count(), 3);
}

#[tokio::test]
async fn a_range_that_fails_every_time_is_reported_once_it_has_had_every_attempt() {
    let folder = TempFolder::new();
    let factory = resizable_with(|_| false, Duration::ZERO, |chunk, _| chunk == 0, None);
    let pool = factory.pool(Duration::from_millis(5));
    let events = pool.subscribe();

    assert!(!pool.run(&item(&folder, 2), &sinfo(8 * MIB), settings(true), CancellationToken::new()).await.unwrap());

    let reported = failures(&events);
    assert_eq!(reported.len(), 1);
    assert!(reported[0].contains("#0"));
    assert_eq!(factory.created().iter().filter(|c| c.id == 0).count(), RECOVERY_ATTEMPTS as usize);
}

#[tokio::test]
async fn a_failure_a_new_connection_cannot_fix_is_reported_without_one() {
    // A wrong password is as wrong on a fresh socket, and every replacement would
    // be another strike towards a lockout.
    let folder = TempFolder::new();
    let factory = resizable_with(|_| false, Duration::ZERO, |chunk, _| chunk == 0, Some(auth_failure));
    let pool = factory.pool(Duration::from_millis(5));
    let events = pool.subscribe();

    assert!(!pool.run(&item(&folder, 2), &sinfo(8 * MIB), settings(true), CancellationToken::new()).await.unwrap());
    assert_eq!(failures(&events).len(), 1);
    assert_eq!(factory.created().iter().filter(|c| c.id == 0).count(), 1);
}

#[tokio::test]
async fn a_failure_is_not_recovered_after_the_download_was_paused() {
    let folder = TempFolder::new();
    let cancel = CancellationToken::new();
    let factory = resizable_with(|chunk| chunk == 1, Duration::ZERO, |chunk, _| chunk == 0, None);
    let pool = factory.pool(Duration::from_secs(30));
    let running = spawn_run(&pool, item(&folder, 2), sinfo(8 * MIB), settings(true), cancel.clone());

    factory.when_created(3).await;
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), running).await.expect("the run ended").unwrap();
    assert!(matches!(result, Err(CoreError::Cancelled)));
}

// ------------------------------------------------ per-connection commands --

#[test]
fn a_command_is_refused_when_no_run_is_under_way() {
    let pool = resizable(|_| false).pool(Duration::ZERO);
    assert!(!pool.control_connection(0, ConnectionCommand::Pause));
    assert!(!pool.control_connection(0, ConnectionCommand::Restart));
}

#[tokio::test]
async fn a_paused_connection_keeps_its_chunk_and_carries_on_from_it_when_resumed() {
    let folder = TempFolder::new();
    folder.write_file("part1.tmp", &[0; 50]);
    let factory = resizable(|chunk| chunk == 1);
    let pool = factory.pool(Duration::from_millis(5));
    let running = spawn_run(&pool, item(&folder, 2), sinfo(8 * MIB), settings(false), CancellationToken::new());

    until(|| status_of(&pool, 1) == Some(ConnectionStatus::ReceivingData)).await;
    assert!(pool.control_connection(1, ConnectionCommand::Pause));
    until(|| status_of(&pool, 1) == Some(ConnectionStatus::Paused)).await;

    assert!(!running.is_finished()); // a paused connection is not a finished download
    assert!(Path::new(&folder.file("part1.tmp")).exists());
    assert!(!pool.control_connection(1, ConnectionCommand::Pause));

    factory.release_all();
    assert!(pool.control_connection(1, ConnectionCommand::Resume));
    assert!(running.await.unwrap().unwrap());

    let attempts: Vec<Arc<FakeResizable>> = factory.created().into_iter().filter(|c| c.id == 1).collect();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1].already, 50);
}

#[tokio::test]
async fn a_restarted_connection_throws_its_chunk_away_and_starts_its_range_again() {
    let folder = TempFolder::new();
    folder.write_file("part1.tmp", &[0; 50]);
    let factory = resizable(|chunk| chunk == 1);
    let pool = factory.pool(Duration::from_millis(5));
    let running = spawn_run(&pool, item(&folder, 2), sinfo(8 * MIB), settings(false), CancellationToken::new());

    until(|| status_of(&pool, 1) == Some(ConnectionStatus::ReceivingData)).await;
    assert!(pool.control_connection(1, ConnectionCommand::Restart));
    factory.when_created(3).await;

    assert!(!Path::new(&folder.file("part1.tmp")).exists());
    factory.release_all();
    assert!(running.await.unwrap().unwrap());

    let attempts: Vec<Arc<FakeResizable>> = factory.created().into_iter().filter(|c| c.id == 1).collect();
    assert_eq!(attempts.len(), 2);
    assert_eq!((attempts[0].already, attempts[1].already), (50, 0));
    assert_eq!(attempts[0].range.start(), attempts[1].range.start());
}

#[tokio::test]
async fn a_finished_connection_can_be_restarted_but_not_paused_or_resumed() {
    let folder = TempFolder::new();
    let factory = resizable(|chunk| chunk == 1);
    let pool = factory.pool(Duration::from_millis(5));
    let running = spawn_run(&pool, item(&folder, 2), sinfo(8 * MIB), settings(false), CancellationToken::new());

    until(|| status_of(&pool, 0) == Some(ConnectionStatus::Finished)).await;
    assert!(!pool.control_connection(0, ConnectionCommand::Pause));
    assert!(!pool.control_connection(0, ConnectionCommand::Resume));
    assert!(pool.control_connection(0, ConnectionCommand::Restart));
    factory.when_created(3).await;

    factory.release_all();
    assert!(running.await.unwrap().unwrap());
    assert_eq!(factory.created().iter().filter(|c| c.id == 0).count(), 2);
}

#[tokio::test]
async fn a_failed_connection_can_be_resumed_while_the_others_are_still_running() {
    // Refused outright the first time — a login the user has since fixed — so it
    // is reported straight away and the Resume is what brings it back.
    let folder = TempFolder::new();
    let factory = resizable_with(|chunk| chunk == 1, Duration::ZERO, |chunk, attempt| chunk == 0 && attempt == 1, Some(auth_failure));
    let pool = factory.pool(Duration::from_millis(5));
    let events = pool.subscribe();
    let running = spawn_run(&pool, item(&folder, 2), sinfo(8 * MIB), settings(false), CancellationToken::new());

    let seen = Arc::new(Mutex::new(0));
    until(|| {
        *seen.lock() += failures(&events).len();
        *seen.lock() == 1 && status_of(&pool, 0) == Some(ConnectionStatus::Failed)
    })
    .await;
    assert!(pool.control_connection(0, ConnectionCommand::Resume));
    factory.when_created(3).await;

    factory.release_all();
    assert!(running.await.unwrap().unwrap());
}

#[tokio::test]
async fn pausing_the_download_ends_a_run_whose_every_connection_is_paused() {
    // Nothing is running to wake the scheduler, so the download's own pause has to.
    let folder = TempFolder::new();
    let cancel = CancellationToken::new();
    let factory = resizable(|_| true);
    let pool = factory.pool(Duration::from_millis(5));
    let running = spawn_run(&pool, item(&folder, 1), sinfo(8 * MIB), settings(false), cancel.clone());

    until(|| status_of(&pool, 0) == Some(ConnectionStatus::ReceivingData)).await;
    assert!(pool.control_connection(0, ConnectionCommand::Pause));
    until(|| status_of(&pool, 0) == Some(ConnectionStatus::Paused)).await;

    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), running).await.expect("the run ended").unwrap();
    assert!(matches!(result, Err(CoreError::Cancelled)));
    assert!(!pool.control_connection(0, ConnectionCommand::Resume));
}

#[tokio::test]
async fn a_paused_connection_keeps_its_slot() {
    // Pausing one of two is asking for one connection, not for the waiting
    // segment to take its place.
    let folder = TempFolder::new();
    SegmentMap::write(
        &folder.path_str(),
        &SegmentMap::new(3000, vec![Segment::new(0, 0, 999), Segment::new(1, 1000, 1999), Segment::new(2, 2000, 2999)]),
    );
    let factory = resizable(|_| true);
    let pool = factory.pool(Duration::from_millis(5));
    let running = spawn_run(&pool, item(&folder, 2), sinfo(3000), settings(false), CancellationToken::new());

    until(|| status_of(&pool, 0) == Some(ConnectionStatus::ReceivingData)).await;
    assert!(pool.control_connection(0, ConnectionCommand::Pause));
    until(|| status_of(&pool, 0) == Some(ConnectionStatus::Paused)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(status_of(&pool, 2), Some(ConnectionStatus::Idle));

    factory.release_all();
    assert!(pool.control_connection(0, ConnectionCommand::Resume));
    assert!(running.await.unwrap().unwrap());
}
