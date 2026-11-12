//! The facade every window talks to. Most of these run the real pool, the real
//! service and the real merger over a fake connection factory, so the wiring is
//! exercised rather than mocked out — including the end-to-end case where eight
//! segments become one file. (`DownloadManagerTests.cs`.)

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::enums::ConnectionStatus;
use crate::events::NoopDispatcher;
use crate::helpers::disk_space::scratch::TempFolder;
use crate::helpers::file_checksum::ChecksumAlgorithm;
use crate::models::{DownloadCredentials, Headers, RemoteFileInfo, RequestOptions, StreamFormat};
use crate::services::file_merger::{self, FileMerger as RealMerger, Plan, PlanChunk};
use crate::traits::{ConnectionSpec, DownloadConnection, DownloadManager as _};

// ---------------------------------------------------------------- fakes --

struct FakeSettings {
    current: Mutex<Arc<DownloadSettings>>,
    hub: EventHub<Arc<DownloadSettings>>,
}

impl SettingsService for FakeSettings {
    fn current(&self) -> Arc<DownloadSettings> {
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

#[derive(Default)]
struct FakeRepository {
    items: Mutex<Vec<DownloadItem>>,
    last_saved: Mutex<Vec<DownloadItem>>,
}

impl DownloadRepository for FakeRepository {
    fn load_all(&self) -> Vec<DownloadItem> {
        self.items.lock().clone()
    }
    fn save_all(&self, items: &[DownloadItem]) {
        *self.last_saved.lock() = items.to_vec();
    }
}

struct FakeInfoProvider(Arc<Mutex<RemoteFileInfo>>);

#[async_trait]
impl RemoteFileInfoProvider for FakeInfoProvider {
    async fn get_file_info(&self, _url: &str, _options: &RequestOptions, _cancel: &CancellationToken) -> Result<RemoteFileInfo> {
        Ok(self.0.lock().clone())
    }
}

/// A connection that writes its own slice of a known payload into its chunk
/// file, so the pool, the resume offsets and the merge can be checked against
/// real bytes without a server.
struct ChunkWritingFactory {
    content: Mutex<Arc<Vec<u8>>>,
    /// How much of each range to write; below 1.0 leaves the segment short.
    write_fraction: Mutex<f64>,
    created: AtomicI32,
    resume_offsets: Mutex<Vec<i64>>,
    urls: Mutex<Vec<String>>,
}

impl Default for ChunkWritingFactory {
    fn default() -> Self {
        Self {
            content: Mutex::new(Arc::new(Vec::new())),
            write_fraction: Mutex::new(1.0),
            created: AtomicI32::new(0),
            resume_offsets: Mutex::new(Vec::new()),
            urls: Mutex::new(Vec::new()),
        }
    }
}

impl ConnectionFactory for ChunkWritingFactory {
    fn create(&self, spec: ConnectionSpec) -> Arc<dyn DownloadConnection> {
        self.created.fetch_add(1, Ordering::SeqCst);
        self.resume_offsets.lock().push(spec.already_downloaded);
        self.urls.lock().push(spec.url.clone());
        Arc::new(ChunkWritingConnection {
            id: spec.connection_id,
            start: spec.range_start,
            end: spec.range_end,
            path: spec.chunk_file_path,
            bytes: Mutex::new(spec.already_downloaded),
            status: Mutex::new(ConnectionStatus::Idle),
            content: self.content.lock().clone(),
            fraction: *self.write_fraction.lock(),
        })
    }
}

struct ChunkWritingConnection {
    id: i32,
    start: i64,
    end: i64,
    path: String,
    bytes: Mutex<i64>,
    status: Mutex<ConnectionStatus>,
    content: Arc<Vec<u8>>,
    fraction: f64,
}

#[async_trait]
impl DownloadConnection for ChunkWritingConnection {
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
        *self.bytes.lock()
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
    async fn run(&self, _cancel: CancellationToken) -> Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let length = self.end - self.start + 1;
        let target = (length as f64 * self.fraction) as i64;
        let have = *self.bytes.lock();
        if have < target {
            std::fs::create_dir_all(Path::new(&self.path).parent().unwrap())?;
            let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(&self.path)?;
            file.seek(SeekFrom::Start(have as u64))?;
            let from = (self.start + have) as usize;
            file.write_all(&self.content[from..from + (target - have) as usize])?;
            *self.bytes.lock() = target;
        }
        *self.status.lock() = if *self.bytes.lock() >= length { ConnectionStatus::Finished } else { ConnectionStatus::Failed };
        Ok(())
    }
}

struct Fixture {
    _root: TempFolder,
    downloads: String,
    temp: String,
    settings: Arc<FakeSettings>,
    repository: Arc<FakeRepository>,
    info: Arc<Mutex<RemoteFileInfo>>,
    factory: Arc<ChunkWritingFactory>,
    manager: Arc<DownloadManager>,
}

impl Fixture {
    fn new() -> Self {
        let root = TempFolder::new();
        let downloads = root.file("downloads");
        let temp = root.file("temp");
        std::fs::create_dir_all(&downloads).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        let settings = Arc::new(FakeSettings {
            current: Mutex::new(Arc::new(DownloadSettings {
                default_download_folder: downloads.clone(),
                temp_folder: temp.clone(),
                max_retries: 0,
                retry_delay_milliseconds: 0,
                ..Default::default()
            })),
            hub: EventHub::default(),
        });
        let repository = Arc::new(FakeRepository::default());
        let info = Arc::new(Mutex::new(RemoteFileInfo {
            content_length: 1000,
            content_type: "application/octet-stream".into(),
            ..Default::default()
        }));
        let factory = Arc::new(ChunkWritingFactory::default());
        let manager = DownloadManager::new(
            repository.clone(),
            settings.clone(),
            Arc::new(FakeInfoProvider(info.clone())),
            factory.clone(),
            Arc::new(RealMerger::new()),
            Arc::new(NoopDispatcher),
            true,
            Handle::current(),
        );
        Self { _root: root, downloads, temp, settings, repository, info, factory, manager }
    }

    fn request(&self, file_name: &str, connections: i32, start: bool) -> DownloadRequest {
        let mut request =
            DownloadRequest::new("https://example.com/file.bin", self.info.lock().clone(), self.downloads.clone(), file_name, connections);
        request.start_immediately = start;
        request
    }

    async fn add(&self, file_name: &str, connections: i32, start: bool) -> DownloadItem {
        self.manager.add_download(self.request(file_name, connections, start)).await.unwrap()
    }

    async fn add_stream(&self, file_name: &str) -> DownloadItem {
        let mut request = self.request(file_name, 4, false);
        request.url = "https://example.com/master.m3u8".into();
        request.stream = Some(StreamSource {
            format: StreamFormat::Hls,
            main_playlist_url: Some("https://example.com/1080.m3u8".into()),
            audio_playlist_url: Some("https://example.com/audio.m3u8".into()),
            audio_extension: Some(".m4a".into()),
            audio_file_name: Some("ignored.m4a".into()),
            ..Default::default()
        });
        self.manager.add_download(request).await.unwrap()
    }

    fn get(&self, id: Uuid) -> DownloadItem {
        self.manager.get(id).unwrap()
    }

    fn set_status(&self, id: Uuid, status: DownloadStatus) {
        self.manager.update(id, |i| i.status = status);
    }

    fn load(&self, items: Vec<DownloadItem>) {
        *self.repository.items.lock() = items;
        self.manager.load_persisted_downloads();
    }

    fn content(&self, bytes: Vec<u8>) {
        self.info.lock().content_length = bytes.len() as i64;
        self.info.lock().supports_range_requests = true;
        *self.factory.content.lock() = Arc::new(bytes);
    }
}

fn pattern(len: usize, f: impl Fn(usize) -> u8) -> Vec<u8> {
    (0..len).map(f).collect()
}

const ABC_SHA256: &str = "039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81";

// ---------------------------------------------------------------- tests --

#[tokio::test]
async fn anything_persisted_as_in_flight_comes_back_paused() {
    // Otherwise the list shows downloads as running that no longer are.
    let f = Fixture::new();
    f.load(
        [DownloadStatus::Downloading, DownloadStatus::Connecting, DownloadStatus::Merging, DownloadStatus::Completed, DownloadStatus::Failed]
            .into_iter()
            .map(|status| DownloadItem { status, ..Default::default() })
            .collect(),
    );
    let count = |s: DownloadStatus| f.manager.downloads().iter().filter(|i| i.status == s).count();
    assert_eq!(count(DownloadStatus::Paused), 3);
    assert_eq!(count(DownloadStatus::Completed), 1);
    assert_eq!(count(DownloadStatus::Failed), 1);
}

#[tokio::test]
async fn a_download_interrupted_while_verifying_comes_back_complete_but_unchecked() {
    // The file is whole and in place and its chunks are gone: resuming it would
    // fetch every byte again.
    let f = Fixture::new();
    f.load(vec![DownloadItem { status: DownloadStatus::Verifying, checksum_result: ChecksumResult::Match, ..Default::default() }]);
    let item = f.manager.downloads().pop().unwrap();
    assert_eq!(item.status, DownloadStatus::Completed);
    assert_eq!(item.checksum_result, ChecksumResult::NotChecked);
}

#[tokio::test]
async fn a_checksum_given_when_adding_is_stored_with_the_download() {
    let f = Fixture::new();
    let mut request = f.request("f.bin", 1, false);
    request.expected_checksum = Some(ExpectedChecksum::new(ChecksumAlgorithm::Sha256, ABC_SHA256));
    let item = f.manager.add_download(request).await.unwrap();
    assert_eq!(item.expected_checksum.as_deref(), Some(&*format!("sha256:{ABC_SHA256}")));
}

#[tokio::test]
async fn a_checksum_given_after_the_fact_is_checked_kept_and_announced() {
    let f = Fixture::new();
    std::fs::write(Path::new(&f.downloads).join("done.bin"), [1, 2, 3]).unwrap();
    let item = DownloadItem { file_name: "done.bin".into(), save_folder: f.downloads.clone(), status: DownloadStatus::Completed, ..Default::default() };
    f.load(vec![item.clone()]);
    let events = f.manager.subscribe();

    let result = f.manager.verify_checksum(item.id, file_checksum::parse(Some(ABC_SHA256)).unwrap(), None, CancellationToken::new()).await.unwrap();

    assert_eq!(result, ChecksumResult::Match);
    assert_eq!(f.repository.last_saved.lock()[0].checksum_result, ChecksumResult::Match);
    assert!(events.try_iter().any(|e| matches!(e, DownloadEvent::StatusChanged(s) if s.download_id == item.id)));
}

#[tokio::test]
async fn a_download_with_no_file_to_check_is_not_checked() {
    let f = Fixture::new();
    let item = f.add("file.bin", 4, false).await;
    let result = f.manager.verify_checksum(item.id, file_checksum::parse(Some(ABC_SHA256)).unwrap(), None, CancellationToken::new()).await.unwrap();
    assert_eq!(result, ChecksumResult::NotChecked);
}

#[tokio::test]
async fn adding_a_download_sanitizes_the_name_the_server_gave() {
    let f = Fixture::new();
    assert_eq!(f.add("../../autorun.inf", 4, false).await.file_name, "autorun.inf");
}

#[tokio::test]
async fn adding_a_download_avoids_overwriting_a_file_already_there() {
    let f = Fixture::new();
    std::fs::write(Path::new(&f.downloads).join("a.bin"), "existing").unwrap();
    assert_eq!(f.add("a.bin", 4, false).await.file_name, "a (1).bin");
}

#[tokio::test]
async fn two_downloads_of_the_same_name_are_never_given_the_same_file() {
    // Neither is on disk until it merges, so the file check alone gave both the
    // same path and the second to finish replaced the first. A batch of links that
    // all end in download.php made that routine.
    let f = Fixture::new();
    assert_eq!(f.add("download.php", 4, false).await.file_name, "download.php");
    assert_eq!(f.add("download.php", 4, false).await.file_name, "download (1).php");
    assert_eq!(f.add("DOWNLOAD.php", 4, false).await.file_name, "DOWNLOAD (2).php");
}

#[tokio::test]
async fn a_cancelled_download_does_not_hold_on_to_its_name() {
    let f = Fixture::new();
    let cancelled = f.add("a.bin", 4, false).await;
    f.manager.stop(cancelled.id);
    assert_eq!(f.add("a.bin", 4, false).await.file_name, "a.bin");
}

#[tokio::test]
async fn adding_a_download_clamps_the_connection_count() {
    let f = Fixture::new();
    assert_eq!(f.add("file.bin", 500, false).await.connections_count, DownloadSettings::MAX_CONNECTIONS);
}

#[tokio::test]
async fn a_download_destined_for_a_queue_is_added_and_left_alone() {
    // Starting it here would be the one thing the user asked not to happen by
    // choosing a queue at all.
    let f = Fixture::new();
    let item = f.add("file.bin", 4, false).await;
    assert_eq!(item.status, DownloadStatus::Queued);
    assert_eq!(f.factory.created.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn adding_a_download_claims_a_temp_folder_of_its_own() {
    let f = Fixture::new();
    let item = f.add("file.bin", 4, false).await;
    assert!(item.temp_folder_path.starts_with(&f.temp));
    let name = Path::new(&item.temp_folder_path).file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(name.len(), 32);
}

#[tokio::test]
async fn the_credentials_are_kept_with_the_download() {
    let f = Fixture::new();
    let mut request = f.request("file.bin", 4, false);
    request.credentials = Some(DownloadCredentials::new("alice", "hunter2"));
    let item = f.manager.add_download(request).await.unwrap();
    let credentials = item.credentials.unwrap();
    assert_eq!((credentials.user_name.as_str(), credentials.password.as_str()), ("alice", "hunter2"));
}

#[tokio::test]
async fn adding_a_download_records_the_version_of_the_file_the_probe_described() {
    let f = Fixture::new();
    f.info.lock().etag = Some("\"v1\"".into());
    f.info.lock().final_url = "https://cdn.example.com/a.bin".into();
    let version = f.add("file.bin", 4, false).await.remote_version.unwrap();
    assert_eq!(version.etag.as_deref(), Some("\"v1\""));
    assert_eq!(version.host.as_deref(), Some("cdn.example.com"));
}

#[tokio::test]
async fn the_captured_headers_are_found_whatever_their_case() {
    let f = Fixture::new();
    let mut request = f.request("file.bin", 4, false);
    request.headers = Some(Headers::from([("Cookie".to_string(), "session=abc".to_string())]));
    let item = f.manager.add_download(request).await.unwrap();
    assert_eq!(crate::models::header_get(&item.headers, "cookie"), Some("session=abc"));
}

#[tokio::test]
async fn adding_and_removing_a_download_is_announced() {
    let f = Fixture::new();
    let events = f.manager.subscribe();
    let item = f.add("file.bin", 4, false).await;
    f.manager.remove(item.id, false);

    let changes: Vec<DownloadListChangeType> =
        events.try_iter().filter_map(|e| if let DownloadEvent::ListChanged { change, .. } = e { Some(change) } else { None }).collect();
    assert_eq!(changes, [DownloadListChangeType::Added, DownloadListChangeType::Removed]);
    assert!(f.manager.downloads().is_empty());
}

#[tokio::test]
async fn removing_a_download_can_take_its_file_with_it_or_leave_it() {
    let f = Fixture::new();
    let item = f.add("a.bin", 4, false).await;
    std::fs::write(item.full_path(), "content").unwrap();
    f.manager.remove(item.id, true);
    assert!(!Path::new(&item.full_path()).exists());

    let kept = f.add("b.bin", 4, false).await;
    std::fs::write(kept.full_path(), "content").unwrap();
    f.manager.remove(kept.id, false);
    assert!(Path::new(&kept.full_path()).exists());
}

#[tokio::test]
async fn a_stream_is_run_by_the_stream_engine_with_its_sound_named_after_the_video() {
    let f = Fixture::new();
    let item = f.add_stream("Talk.mp4").await;
    assert!(f.manager.is_stream(item.id));
    assert_eq!(item.stream.as_ref().unwrap().audio_file_name.as_deref(), Some("Talk.audio.m4a"));
    assert_eq!(item.audio_full_path(), Some(join_path(&f.downloads, "Talk.audio.m4a")));
    let plain = f.add("file.bin", 4, false).await;
    assert!(!f.manager.is_stream(plain.id));
}

#[tokio::test]
async fn two_streams_of_the_same_name_never_share_a_video_or_a_sound_file() {
    let f = Fixture::new();
    std::fs::write(Path::new(&f.downloads).join("Talk (1).audio.m4a"), "someone else's").unwrap();
    let first = f.add_stream("Talk.mp4").await;
    let second = f.add_stream("Talk.mp4").await;
    assert_eq!(second.file_name, "Talk (1).mp4");
    assert_eq!(second.stream.as_ref().unwrap().audio_file_name.as_deref(), Some("Talk (1).audio (1).m4a"));
    assert_ne!(first.audio_full_path(), second.audio_full_path());
}

#[tokio::test]
async fn a_file_already_named_like_a_streams_sound_is_not_replaced_by_it() {
    let f = Fixture::new();
    // Another download will write "Clip.audio.m4a" as its own video.
    f.add("Clip.audio.m4a", 4, false).await;
    let stream = f.add_stream("Clip.mp4").await;
    assert_eq!(stream.stream.unwrap().audio_file_name.as_deref(), Some("Clip.audio (1).m4a"));
}

#[tokio::test]
async fn removing_a_stream_can_take_its_sound_with_it() {
    let f = Fixture::new();
    let item = f.add_stream("Talk.mp4").await;
    std::fs::write(item.full_path(), "video").unwrap();
    std::fs::write(item.audio_full_path().unwrap(), "sound").unwrap();
    f.manager.remove(item.id, true);
    assert!(!Path::new(&item.full_path()).exists());
    assert!(!Path::new(&item.audio_full_path().unwrap()).exists());
}

#[tokio::test]
async fn removing_something_that_is_not_there_is_harmless() {
    Fixture::new().manager.remove(Uuid::new_v4(), true);
}

#[tokio::test]
async fn a_speed_limit_set_on_a_download_is_persisted() {
    // So a limit set on a big download survives a pause or a restart rather than
    // quietly reverting to full speed.
    let f = Fixture::new();
    let item = f.add("file.bin", 4, false).await;
    f.manager.set_speed_limit(item.id, 250_000);
    assert_eq!(f.get(item.id).speed_limit_bytes_per_second, 250_000);
    assert_eq!(f.repository.last_saved.lock()[0].speed_limit_bytes_per_second, 250_000);
}

#[tokio::test]
async fn what_happens_on_completion_is_persisted_with_the_download() {
    // Its details window closes the moment the download completes, which is
    // exactly when these are read.
    let f = Fixture::new();
    let item = f.add("file.bin", 4, false).await;
    f.manager.set_completion_actions(item.id, CompletionActions::PLAY_SOUND | CompletionActions(64), PowerAction::Hibernate);
    let saved = f.repository.last_saved.lock()[0].clone();
    assert_eq!(saved.on_completion, CompletionActions::PLAY_SOUND);
    assert_eq!(saved.power_action_when_done, PowerAction::Hibernate);
}

#[tokio::test]
async fn changing_what_happens_on_completion_is_announced() {
    // The details window draws from the view model, which only learns of the
    // change through this event; without it the checkbox snaps straight back.
    let f = Fixture::new();
    let item = f.add("file.bin", 4, false).await;
    let events = f.manager.subscribe();
    f.manager.set_completion_actions(item.id, CompletionActions::OPEN_FOLDER, PowerAction::Sleep);
    let updated = events
        .try_iter()
        .find_map(|e| if let DownloadEvent::ItemUpdated(i) = e { Some(i) } else { None })
        .expect("an ItemUpdated event");
    assert_eq!(updated.id, item.id);
    assert_eq!(updated.on_completion, CompletionActions::OPEN_FOLDER);
    assert_eq!(updated.power_action_when_done, PowerAction::Sleep);
}

#[tokio::test]
async fn turning_a_speed_limit_off_lifts_the_download_s_limiter() {
    let f = Fixture::new();
    let item = f.add("file.bin", 4, false).await;
    let limiter = Arc::clone(&f.manager.services.read()[&item.id].limits.own);
    f.manager.set_speed_limit(item.id, 500 * 1024);
    assert_eq!(limiter.bytes_per_second(), 500 * 1024);
    f.manager.set_speed_limit(item.id, 0);
    assert_eq!(limiter.bytes_per_second(), 0);
    assert_eq!(f.get(item.id).speed_limit_bytes_per_second, 0);
}

#[tokio::test]
async fn the_speed_limiter_tab_can_turn_a_limit_off_again() {
    // Replays the details window's Speed Limiter tab: its copy of the download
    // comes from the view model, which only changes on the manager's events, and
    // it sends a limit only when it differs from that copy (details.rs speed_tab).
    let f = Fixture::new();
    let item = f.add("file.bin", 4, false).await;
    let limiter = Arc::clone(&f.manager.services.read()[&item.id].limits.own);
    let events = f.manager.subscribe();
    let mut window_copy = f.get(item.id);
    let user_sets = |limit: i64, window_copy: &mut DownloadItem| {
        if limit != window_copy.speed_limit_bytes_per_second {
            f.manager.set_speed_limit(item.id, limit);
        }
        for e in events.try_iter() {
            if let DownloadEvent::ItemUpdated(i) = e
                && i.id == item.id
            {
                *window_copy = *i;
            }
        }
    };

    user_sets(500 * 1024, &mut window_copy); // tick the box
    assert_eq!(limiter.bytes_per_second(), 500 * 1024);
    user_sets(0, &mut window_copy); // untick it
    assert_eq!(limiter.bytes_per_second(), 0, "the cap stayed on after the box was unticked");
}

#[tokio::test]
async fn a_negative_speed_limit_is_read_as_unlimited() {
    let f = Fixture::new();
    let item = f.add("file.bin", 4, false).await;
    f.manager.set_speed_limit(item.id, -1);
    assert_eq!(f.get(item.id).speed_limit_bytes_per_second, 0);
}

#[tokio::test]
async fn the_global_speed_limit_follows_the_settings_without_a_restart() {
    // The one setting honoured live: a limit you have to restart the app to apply
    // is one the user will assume is broken.
    let f = Fixture::new();
    f.settings.save(DownloadSettings {
        global_speed_limit_bytes_per_second: 512_000,
        default_download_folder: f.downloads.clone(),
        temp_folder: f.temp.clone(),
        ..Default::default()
    });
    assert_eq!(f.manager.global_speed_limit(), 512_000);
}

#[tokio::test]
async fn the_global_speed_limit_can_also_be_set_directly() {
    let f = Fixture::new();
    f.manager.set_global_speed_limit(64_000);
    assert_eq!(f.manager.global_speed_limit(), 64_000);
}

#[tokio::test]
async fn pause_all_counts_only_what_was_actually_in_flight() {
    let f = Fixture::new();
    let items: Vec<DownloadItem> = [DownloadStatus::Completed, DownloadStatus::Queued, DownloadStatus::Paused]
        .into_iter()
        .map(|status| DownloadItem { status, ..Default::default() })
        .collect();
    f.load(items.clone());

    // Set after the load, which downgrades anything persisted as in-flight —
    // this is the shutdown case, where downloads really are running.
    f.set_status(items[1].id, DownloadStatus::Downloading);
    f.set_status(items[2].id, DownloadStatus::Connecting);

    // The count is what lets shutdown know whether it has anything to wait on.
    assert_eq!(f.manager.pause_all(), 2);
    assert!(f.manager.downloads().iter().all(|i| !download_actions::can_pause(i)));
}

// ------------------------------------------------ orphaned temp folders --

#[tokio::test]
async fn the_sweep_deletes_a_folder_no_live_download_claims() {
    let f = Fixture::new();
    let orphan = join_path(&f.temp, &Uuid::new_v4().simple().to_string());
    std::fs::create_dir_all(&orphan).unwrap();
    assert_eq!(f.manager.cleanup_orphaned_temp_folders().await, 1);
    assert!(!Path::new(&orphan).exists());
}

#[tokio::test]
async fn the_sweep_leaves_a_folder_a_paused_download_is_resuming_from() {
    let f = Fixture::new();
    let item = f.add("file.bin", 4, false).await;
    f.set_status(item.id, DownloadStatus::Paused);
    std::fs::create_dir_all(&item.temp_folder_path).unwrap();
    assert_eq!(f.manager.cleanup_orphaned_temp_folders().await, 0);
    assert!(Path::new(&item.temp_folder_path).exists());
}

#[tokio::test]
async fn the_sweep_collects_a_cancelled_download_s_folder() {
    // Stop means discard, unlike Pause. The item only still carries the path
    // because the delete lost the race with the connections closing.
    let f = Fixture::new();
    let item = f.add("file.bin", 4, false).await;
    f.set_status(item.id, DownloadStatus::Cancelled);
    std::fs::create_dir_all(&item.temp_folder_path).unwrap();
    assert_eq!(f.manager.cleanup_orphaned_temp_folders().await, 1);
    assert!(!Path::new(&item.temp_folder_path).exists());
}

#[tokio::test]
async fn the_sweep_leaves_alone_anything_this_app_did_not_name() {
    // The temp folder may well be shared with other programs.
    let f = Fixture::new();
    let somebody_elses = join_path(&f.temp, "SomeOtherProgram");
    std::fs::create_dir_all(&somebody_elses).unwrap();
    assert_eq!(f.manager.cleanup_orphaned_temp_folders().await, 0);
    assert!(Path::new(&somebody_elses).exists());
}

// ------------------------------------------------------------- end to end --

#[tokio::test]
async fn a_download_runs_through_the_pool_and_the_merger_into_one_file() {
    // The real pool, the real service and the real merger over fake connections
    // that write actual bytes: the split, the chunk files, the ordering and the
    // merge, all the way to the file on disk.
    let f = Fixture::new();
    let content = pattern(5000, |i| (i % 251) as u8);
    f.content(content.clone());

    let item = f.add("whole.bin", 8, true).await;

    assert_eq!(item.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(item.full_path()).unwrap(), content);
    assert_eq!(f.factory.created.load(Ordering::SeqCst), 8);
    assert!(!Path::new(&item.temp_folder_path).exists());
}

#[tokio::test]
async fn a_download_that_is_interrupted_resumes_from_its_chunks() {
    let f = Fixture::new();
    let content = pattern(4000, |i| (i % 251) as u8);
    f.content(content.clone());

    // First pass: every connection writes only half of its range.
    *f.factory.write_fraction.lock() = 0.5;
    let item = f.add("resumed.bin", 4, true).await;
    assert_eq!(item.status, DownloadStatus::Failed);

    // Second pass: the rest arrives, starting from what is already on disk.
    *f.factory.write_fraction.lock() = 1.0;
    f.manager.retry(item.id).await.unwrap();

    let item = f.get(item.id);
    assert_eq!(item.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(item.full_path()).unwrap(), content);
    assert!(f.factory.resume_offsets.lock().iter().any(|&o| o > 0));
}

#[tokio::test]
async fn a_merge_the_app_exited_in_the_middle_of_finishes_on_the_next_launch() {
    // The chunks of an 8-way download, half of them already folded into the first
    // and deleted, then a restart. The next launch must neither re-run the pool
    // over that half-merged set nor produce anything but the file.
    let f = Fixture::new();
    let content = pattern(8000, |i| (i % 251) as u8);
    let temp = join_path(&f.temp, &Uuid::new_v4().simple().to_string());
    std::fs::create_dir_all(&temp).unwrap();
    std::fs::write(Path::new(&temp).join("part0.tmp"), &content[..5000]).unwrap();
    for i in 5..8 {
        std::fs::write(Path::new(&temp).join(format!("part{i}.tmp")), &content[i * 1000..(i + 1) * 1000]).unwrap();
    }
    file_merger::write_plan(
        &temp,
        &Plan {
            destination: join_path(&f.downloads, "restarted.bin"),
            in_place: true,
            chunks: (0..8).map(|i| PlanChunk::new(format!("part{i}.tmp"), 1000)).collect(),
        },
    )
    .unwrap();

    let item = DownloadItem {
        url: "https://example.com/file.bin".into(),
        file_name: "restarted.bin".into(),
        save_folder: f.downloads.clone(),
        temp_folder_path: temp.clone(),
        total_bytes: content.len() as i64,
        downloaded_bytes: content.len() as i64,
        status: DownloadStatus::Merging,
        ..Default::default()
    };
    f.load(vec![item.clone()]);
    f.manager.resume(item.id).await.unwrap();

    let item = f.get(item.id);
    assert_eq!(item.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(item.full_path()).unwrap(), content);
    assert_eq!(f.factory.created.load(Ordering::SeqCst), 0);
    assert!(!Path::new(&temp).exists());
}

#[tokio::test]
async fn a_file_replaced_on_the_server_between_attempts_is_not_merged_with_the_old_one() {
    // Half of version 1 on disk, then the publisher replaces the file. The retry
    // used to append version 2's tail to version 1's head and call the result
    // complete.
    let f = Fixture::new();
    let version1 = pattern(4000, |i| (i % 251) as u8);
    let version2 = pattern(4000, |i| (250 - i % 251) as u8);
    f.content(version1);
    f.info.lock().etag = Some("\"v1\"".into());

    *f.factory.write_fraction.lock() = 0.5;
    let item = f.add("replaced.bin", 4, true).await;
    assert_eq!(item.status, DownloadStatus::Failed);

    *f.factory.content.lock() = Arc::new(version2.clone());
    f.info.lock().etag = Some("\"v2\"".into());
    *f.factory.write_fraction.lock() = 1.0;
    f.manager.retry(item.id).await.unwrap();

    let item = f.get(item.id);
    assert_eq!(item.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(item.full_path()).unwrap(), version2);
}

#[tokio::test]
async fn a_download_whose_link_expired_carries_on_from_its_chunks_at_a_new_address() {
    let f = Fixture::new();
    let content = pattern(4000, |i| (i % 251) as u8);
    f.content(content.clone());
    *f.factory.write_fraction.lock() = 0.5;
    let item = f.add("expired.bin", 4, true).await;
    assert_eq!(item.status, DownloadStatus::Failed);

    let changed = f.manager.change_address(
        item.id,
        AddressChange {
            url: "https://example.com/file.bin?sig=renewed".into(),
            file_info: f.info.lock().clone(),
            credentials: None,
            headers: None,
            discard_progress: false,
        },
    );
    *f.factory.write_fraction.lock() = 1.0;
    f.manager.resume(item.id).await.unwrap();

    assert!(changed);
    let item = f.get(item.id);
    assert_eq!(item.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(item.full_path()).unwrap(), content);
    assert!(f.factory.resume_offsets.lock().iter().skip(4).any(|&o| o > 0));
    assert!(f.factory.urls.lock().iter().skip(4).all(|u| u.ends_with("sig=renewed")));
    assert!(f.repository.last_saved.lock().iter().any(|s| s.url.ends_with("sig=renewed")));
}

#[tokio::test]
async fn an_address_change_for_a_download_that_does_not_exist_changes_nothing() {
    let f = Fixture::new();
    let change = AddressChange { url: "https://x/y".into(), file_info: f.info.lock().clone(), credentials: None, headers: None, discard_progress: false };
    assert!(!f.manager.change_address(Uuid::new_v4(), change));
}

#[tokio::test]
async fn progress_is_saved_on_a_debounce_and_flush_writes_it_now() {
    // MIGRATION.md §4.5: progress no longer writes the whole list every tick.
    let f = Fixture::new();
    let item = f.add("file.bin", 4, false).await;
    f.manager.update(item.id, |i| i.downloaded_bytes = 123);
    f.manager.flush();
    assert_eq!(f.repository.last_saved.lock()[0].downloaded_bytes, 123);
}
