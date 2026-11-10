//! The processor half of `CommandTests.cs`: a request from `qb` carried out
//! through the same managers the windows use.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::runtime::Handle;

use super::*;
use crate::enums::{CompletionActions, ConnectionCommand, PowerAction};
use crate::events::{DownloadEvent, EventCallback, EventHub, EventStream, NoopDispatcher};
use crate::helpers::disk_space::scratch::TempFolder;
use crate::helpers::file_checksum::ChecksumResult;
use crate::models::{AddressChange, ConnectionInfo, DownloadSettings, StreamFormat};
use crate::services::queue_manager::QueueManager as RealQueues;
use crate::traits::{ProgressFn, QueueRepository};

#[derive(Default)]
struct FakeProbe {
    info: Mutex<RemoteFileInfo>,
    throw: Mutex<Option<CoreError>>,
    last_options: Mutex<Option<RequestOptions>>,
}

#[async_trait]
impl RemoteFileInfoProvider for FakeProbe {
    async fn get_file_info(&self, url: &str, options: &RequestOptions, _cancel: &CancellationToken) -> Result<RemoteFileInfo> {
        *self.last_options.lock() = Some(options.clone());
        if let Some(e) = self.throw.lock().clone() {
            return Err(e);
        }
        let info = self.info.lock().clone();
        let final_url = if info.final_url.is_empty() { url.to_string() } else { info.final_url.clone() };
        Ok(RemoteFileInfo { final_url, ..info })
    }
}

struct FakeStreams;

#[async_trait]
impl StreamResolver for FakeStreams {
    async fn describe(&self, url: &str, _options: &RequestOptions, _cancel: &CancellationToken) -> Result<StreamDescription> {
        let choice = |label: &str, ext: &str| StreamChoice {
            label: label.into(),
            bandwidth: 1,
            source: StreamSource { format: StreamFormat::Hls, main_playlist_url: Some(url.into()), quality: label.into(), ..Default::default() },
            extension: ext.into(),
            content_type: "video/mp4".into(),
        };
        Ok(StreamDescription {
            format: StreamFormat::Hls,
            choices: vec![
                choice("1080p · 5 Mbps", ".mp4"),
                choice("720p · 2.5 Mbps", ".mp4"),
                choice("360p · 800 kbps", ".mp4"),
                choice("Audio only · 128 kbps", ".m4a"),
            ],
        })
    }
}

struct FakeSettings(Arc<DownloadSettings>, EventHub<Arc<DownloadSettings>>);

impl SettingsService for FakeSettings {
    fn current(&self) -> Arc<DownloadSettings> {
        self.0.clone()
    }
    fn subscribe(&self) -> EventStream<Arc<DownloadSettings>> {
        self.1.subscribe()
    }
    fn load(&self) {}
    fn save(&self, _settings: DownloadSettings) {}
}

struct MemoryQueues;

impl QueueRepository for MemoryQueues {
    fn load_all(&self) -> Vec<DownloadQueue> {
        Vec::new()
    }
    fn try_load_all(&self) -> Option<Vec<DownloadQueue>> {
        Some(Vec::new())
    }
    fn save_all(&self, _queues: &[DownloadQueue]) {}
}

#[derive(Default)]
struct FakeDownloads {
    items: Mutex<Vec<DownloadItem>>,
    started: Mutex<Vec<Uuid>>,
    paused: Mutex<Vec<Uuid>>,
    hub: EventHub<DownloadEvent>,
}

impl FakeDownloads {
    fn add(&self, item: DownloadItem) -> DownloadItem {
        self.items.lock().push(item.clone());
        item
    }
}

#[async_trait]
impl DownloadManager for FakeDownloads {
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
        self.started.lock().push(id);
        Ok(())
    }
    fn pause(&self, id: Uuid) {
        self.paused.lock().push(id);
    }
    fn pause_all(&self) -> usize {
        self.items.lock().iter().filter(|i| download_actions::can_pause(i)).count()
    }
    async fn resume(&self, id: Uuid) -> Result<()> {
        self.start(id).await
    }
    fn stop(&self, _id: Uuid) {}
    async fn retry(&self, _id: Uuid) -> Result<()> {
        Ok(())
    }
    fn remove(&self, _id: Uuid, _delete_file: bool) {}
    fn change_address(&self, _id: Uuid, _change: AddressChange) -> bool {
        false
    }
    fn control_connection(&self, _id: Uuid, _c: i32, _command: ConnectionCommand) -> bool {
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
    fn set_queue_speed_limit(&self, _id: Uuid, _bps: i64) {}
    fn set_global_speed_limit(&self, _bps: i64) {}
    fn load_persisted_downloads(&self) {}
    async fn cleanup_orphaned_temp_folders(&self) -> usize {
        0
    }
    fn flush(&self) {}
}

struct Fixture {
    folder: TempFolder,
    downloads: Arc<FakeDownloads>,
    queues: Arc<RealQueues>,
    probe: Arc<FakeProbe>,
    added: Arc<Mutex<Vec<DownloadRequest>>>,
    processor: CommandProcessor,
}

impl Fixture {
    fn new() -> Self {
        let folder = TempFolder::new();
        let downloads = Arc::new(FakeDownloads::default());
        let queues = RealQueues::new(Arc::new(MemoryQueues), downloads.clone(), Arc::new(NoopDispatcher), Handle::current());
        traits_load(&queues);
        let probe = Arc::new(FakeProbe {
            info: Mutex::new(RemoteFileInfo { file_name: "file.zip".into(), supports_range_requests: true, ..Default::default() }),
            ..Default::default()
        });
        let settings = Arc::new(FakeSettings(
            Arc::new(DownloadSettings { default_download_folder: folder.path_str(), default_connections_count: 8, ..Default::default() }),
            EventHub::default(),
        ));
        let added = Arc::new(Mutex::new(Vec::new()));
        let (sink, store) = (Arc::clone(&added), Arc::clone(&downloads));
        let add: AddFn = Arc::new(move |request: DownloadRequest| {
            sink.lock().push(request.clone());
            let item = store.add(DownloadItem {
                url: request.url.clone(),
                file_name: request.file_name.clone(),
                save_folder: request.save_folder.clone(),
                status: DownloadStatus::Queued,
                ..Default::default()
            });
            Box::pin(async move { Ok(item) })
        });
        let processor = CommandProcessor::new(downloads.clone(), queues.clone(), probe.clone(), Some(Arc::new(FakeStreams)), settings, Some(add), "1.22.0");
        Self { folder, downloads, queues, probe, added, processor }
    }

    async fn run(&self, request: CommandRequest) -> CommandResponse {
        let response = self.processor.execute(request, &CancellationToken::new()).await;
        // Let the fire-and-forget starts land.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        response
    }

    fn started(&self) -> Vec<Uuid> {
        self.downloads.started.lock().clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        crate::traits::QueueManager::shutdown(&*self.queues);
    }
}

fn traits_load(queues: &RealQueues) {
    crate::traits::QueueManager::load(queues);
}

fn add(urls: &[&str]) -> CommandRequest {
    CommandRequest { verb: CommandVerb::Add, urls: urls.iter().map(|u| u.to_string()).collect(), ..Default::default() }
}

fn verb(verb: CommandVerb) -> CommandRequest {
    CommandRequest::new(verb)
}

fn item(id: &str, name: &str, status: DownloadStatus) -> DownloadItem {
    DownloadItem { id: Uuid::parse_str(id).unwrap(), file_name: name.into(), status, ..Default::default() }
}

#[tokio::test]
async fn an_added_link_is_probed_named_placed_as_options_says_and_started() {
    let f = Fixture::new();
    *f.probe.info.lock() = RemoteFileInfo { file_name: "ubuntu.iso".into(), supports_range_requests: true, content_length: 100, ..Default::default() };

    let response = f.run(add(&["https://example.com/get?id=1"])).await;

    assert!(response.ok, "{:?}", response.error);
    let added = f.added.lock().clone();
    assert_eq!(added.len(), 1);
    let request = &added[0];
    assert_eq!(
        (request.url.as_str(), request.file_name.as_str(), request.save_folder.clone(), request.connections_count),
        ("https://example.com/get?id=1", "ubuntu.iso", f.folder.path_str(), 8)
    );
    assert!(!request.start_immediately); // started separately, so the command returns at once
    assert_eq!(f.started(), [f.downloads.downloads()[0].id]);
    assert_eq!(response.message.as_deref(), Some("Added 1 download, started."));
}

#[tokio::test]
async fn a_login_in_the_link_is_lifted_out_of_the_address_and_carried_as_credentials() {
    let f = Fixture::new();
    f.run(add(&["ftp://alice:s3cret@files.example.com/a.bin"])).await;

    let request = f.added.lock()[0].clone();
    assert!(!request.url.contains("s3cret"));
    let credentials = request.credentials.unwrap();
    assert_eq!((credentials.user_name.as_str(), credentials.password.as_str()), ("alice", "s3cret"));
    assert_eq!(f.probe.last_options.lock().as_ref().unwrap().credentials.as_ref().unwrap().user_name, "alice");
}

#[tokio::test]
async fn a_server_that_will_not_split_the_file_gets_one_connection_whatever_was_asked() {
    let f = Fixture::new();
    *f.probe.info.lock() = RemoteFileInfo { file_name: "a.bin".into(), supports_range_requests: false, ..Default::default() };
    f.run(CommandRequest { connections: Some(16), ..add(&["https://example.com/a.bin"]) }).await;
    assert_eq!(f.added.lock()[0].connections_count, 1);
}

#[tokio::test]
async fn links_for_a_queue_join_it_and_wait_and_the_queue_is_created_if_missing() {
    let f = Fixture::new();
    let response = f.run(CommandRequest { queue: Some("Night".into()), ..add(&["https://x.example.com/1.zip", "https://x.example.com/2.zip"]) }).await;

    let queues = crate::traits::QueueManager::queues(&*f.queues);
    assert_eq!(queues.len(), 1);
    assert_eq!(queues[0].name, "Night");
    let mut in_queue = queues[0].item_ids.clone();
    in_queue.sort();
    let mut ids: Vec<Uuid> = f.downloads.downloads().iter().map(|d| d.id).collect();
    ids.sort();
    assert_eq!(in_queue, ids);
    assert!(f.started().is_empty());
    assert_eq!(response.message.as_deref(), Some("Added 2 downloads, in queue \"Night\"."));

    f.run(CommandRequest { queue: Some("NIGHT".into()), ..add(&["https://x.example.com/3.zip"]) }).await;
    let queues = crate::traits::QueueManager::queues(&*f.queues);
    assert_eq!(queues.len(), 1);
    assert_eq!(queues[0].item_ids.len(), 3);
}

#[tokio::test]
async fn a_paused_add_is_not_started() {
    let f = Fixture::new();
    f.run(CommandRequest { paused: true, ..add(&["https://x.example.com/1.zip"]) }).await;
    assert_eq!(f.added.lock().len(), 1);
    assert!(f.started().is_empty());
}

#[tokio::test]
async fn one_bad_link_fails_the_command_without_losing_the_good_ones() {
    let f = Fixture::new();
    let response = f.run(add(&["https://x.example.com/1.zip", "file:///C:/secret.txt"])).await;
    assert!(!response.ok);
    assert_eq!(response.downloads.unwrap().len(), 1);
    assert!(response.error.unwrap().contains("file:///C:/secret.txt"));
}

#[tokio::test]
async fn a_link_that_needs_a_login_says_how_to_give_one() {
    let f = Fixture::new();
    *f.probe.throw.lock() = Some(CoreError::auth("needs login", false));
    let response = f.run(add(&["https://x.example.com/private.zip"])).await;
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("user:password@"));
}

#[tokio::test]
async fn a_checksum_quickbyte_cannot_read_refuses_the_whole_add() {
    let f = Fixture::new();
    let response = f.run(CommandRequest { checksum: Some("not a hash".into()), ..add(&["https://x.example.com/1.zip"]) }).await;
    assert!(!response.ok);
    assert!(f.added.lock().is_empty());
}

#[tokio::test]
async fn a_folder_that_is_not_a_full_path_is_refused() {
    let f = Fixture::new();
    let response = f.run(CommandRequest { folder: Some(r"relative\dir".into()), ..add(&["https://x.example.com/1.zip"]) }).await;
    assert!(!response.ok);
    assert!(f.added.lock().is_empty());
}

#[tokio::test]
async fn a_streaming_video_is_added_in_the_quality_asked_for() {
    for (quality, expected) in [
        (None, "1080p · 5 Mbps"),
        (Some("best"), "1080p · 5 Mbps"),
        (Some("worst"), "360p · 800 kbps"),
        (Some("720"), "720p · 2.5 Mbps"),
        (Some("audio"), "Audio only · 128 kbps"),
    ] {
        let f = Fixture::new();
        *f.probe.info.lock() = RemoteFileInfo {
            file_name: "master.m3u8".into(),
            content_type: "application/vnd.apple.mpegurl".into(),
            final_url: "https://cdn.example.com/talks/keynote/master.m3u8".into(),
            ..Default::default()
        };
        let response = f
            .run(CommandRequest { quality: quality.map(str::to_string), ..add(&["https://cdn.example.com/talks/keynote/master.m3u8"]) })
            .await;
        assert!(response.ok, "{:?}", response.error);
        let request = f.added.lock()[0].clone();
        assert_eq!(request.stream.unwrap().quality, expected);
        assert!(request.file_name.starts_with("keynote."), "{}", request.file_name);
    }
}

#[tokio::test]
async fn a_quality_the_video_does_not_have_is_refused_with_the_ones_it_does() {
    let f = Fixture::new();
    *f.probe.info.lock() =
        RemoteFileInfo { file_name: "master.m3u8".into(), final_url: "https://cdn.example.com/v/master.m3u8".into(), ..Default::default() };
    let response = f.run(CommandRequest { quality: Some("4K".into()), ..add(&["https://cdn.example.com/v/master.m3u8"]) }).await;
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("720p"));
}

#[tokio::test]
async fn a_download_is_found_by_the_start_of_its_id_and_only_when_that_is_unambiguous() {
    let f = Fixture::new();
    let first = f.downloads.add(item("aaaa1111-0000-0000-0000-000000000000", "one.zip", DownloadStatus::Downloading));
    f.downloads.add(item("aaaa2222-0000-0000-0000-000000000000", "two.zip", DownloadStatus::Downloading));

    let pause = |target: &str| CommandRequest { target: Some(target.into()), ..verb(CommandVerb::Pause) };
    assert!(f.run(pause("aaaa")).await.error.unwrap().to_lowercase().contains("more than one"));
    assert!(f.run(pause("aa")).await.error.unwrap().contains("at least"));
    assert!(f.run(pause("bbbb")).await.error.unwrap().contains("No download"));

    let paused = f.run(pause("AAAA1")).await;
    assert!(paused.ok);
    assert_eq!(*f.downloads.paused.lock(), [first.id]);
}

#[tokio::test]
async fn pausing_or_resuming_what_is_not_in_that_state_says_so() {
    let f = Fixture::new();
    f.downloads.add(item("cccc0000-0000-0000-0000-000000000000", "done.zip", DownloadStatus::Completed));
    f.downloads.add(item("dddd0000-0000-0000-0000-000000000000", "bad.zip", DownloadStatus::Failed));

    let with = |v: CommandVerb, target: &str| CommandRequest { target: Some(target.into()), ..verb(v) };
    assert!(f.run(with(CommandVerb::Pause, "cccc")).await.error.unwrap().contains("nothing to pause"));
    assert!(f.run(with(CommandVerb::Resume, "cccc")).await.error.unwrap().contains("nothing to resume"));
    // A failed download resumes, as its Resume button does in the window.
    assert!(f.run(with(CommandVerb::Resume, "dddd")).await.ok);
}

#[tokio::test]
async fn resume_all_resumes_only_what_can_be() {
    let f = Fixture::new();
    let paused = f.downloads.add(DownloadItem { status: DownloadStatus::Paused, ..Default::default() });
    f.downloads.add(DownloadItem { status: DownloadStatus::Completed, ..Default::default() });
    f.downloads.add(DownloadItem { status: DownloadStatus::Downloading, ..Default::default() });

    let response = f.run(CommandRequest { target: Some("all".into()), ..verb(CommandVerb::Resume) }).await;
    assert_eq!(f.started(), [paused.id]);
    assert_eq!(response.message.as_deref(), Some("Resumed 1 download."));
}

#[tokio::test]
async fn a_queue_is_started_and_stopped_by_its_name_or_the_start_of_it() {
    let f = Fixture::new();
    crate::traits::QueueManager::create(&*f.queues, "Nightly films");
    crate::traits::QueueManager::create(&*f.queues, "Work");

    let started = f.run(CommandRequest { queue: Some("nightly".into()), ..verb(CommandVerb::QueueStart) }).await;
    assert!(started.ok, "{:?}", started.error);
    assert_eq!(started.message.as_deref(), Some("Started queue \"Nightly films\"."));

    let missing = f.run(CommandRequest { queue: Some("Weekend".into()), ..verb(CommandVerb::QueueStop) }).await;
    assert!(!missing.ok);
    assert!(missing.error.unwrap().contains("no queue called"));
}

#[tokio::test]
async fn the_list_filters_by_status_and_the_status_counts_everything() {
    let f = Fixture::new();
    f.downloads.add(DownloadItem { file_name: "a".into(), status: DownloadStatus::Downloading, current_speed_bytes_per_second: 1000.0, ..Default::default() });
    f.downloads.add(DownloadItem { file_name: "b".into(), status: DownloadStatus::Paused, ..Default::default() });
    f.downloads.add(DownloadItem { file_name: "c".into(), status: DownloadStatus::Failed, error_message: Some("404 (Not Found)".into()), ..Default::default() });
    f.downloads.add(DownloadItem { file_name: "d".into(), status: DownloadStatus::Completed, ..Default::default() });

    let failed = f.run(CommandRequest { filter: Some("failed".into()), ..verb(CommandVerb::List) }).await.downloads.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].error.as_deref(), Some("404 (Not Found)"));

    let status = f.run(verb(CommandVerb::Status)).await.status.unwrap();
    assert_eq!(
        (status.downloads, status.active, status.paused, status.failed, status.completed, status.bytes_per_second, status.version.as_str()),
        (4, 1, 1, 1, 1, 1000.0, "1.22.0")
    );
}

#[test]
fn percentages_round_to_one_place_as_dotnet_does() {
    assert_eq!(round_half_even_1(33.333), 33.3);
    assert_eq!(round_half_even_1(12.25), 12.2);
    assert_eq!(round_half_even_1(100.0), 100.0);
}
