//! Streams downloaded end to end, against playlists, pieces and keys served from
//! a local server, and checked against the exact bytes the pieces were made
//! from. A file that is merely the right length proves nothing here: a piece
//! decrypted with the wrong IV, joined out of order or resumed at the wrong
//! offset is exactly as long. (`StreamDownloadServiceTests.cs`.)

use std::collections::HashMap;
use std::sync::Arc;

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockEncryptMut, KeyIvInit};
use async_trait::async_trait;
use parking_lot::Mutex;
use quickbyte_core::CoreError;
use quickbyte_core::enums::DownloadStatus;
use quickbyte_core::events::{DownloadEvent, EventHub, EventStream};
use quickbyte_core::models::{DownloadItem, DownloadSettings, StreamFormat, StreamSource};
use quickbyte_core::services::file_merger::FileMerger;
use quickbyte_core::services::stream_download_service::{StreamDownloadService, StreamServiceOptions};
use quickbyte_core::streaming::stream_plan::LIVE_MESSAGE;
use quickbyte_core::traits::{self, DownloadService as _, ProgressFn};
use tokio_util::sync::CancellationToken;

use super::random_bytes;
use crate::support::{Asked, Reply, TempFolder, TestServer, direct_client};

#[derive(Clone)]
struct Resource {
    body: Vec<u8>,
    content_type: String,
    honours_ranges: bool,
    cut_at: Option<usize>,
}

/// The C# `StubWeb`: resources by path, a 404 for anything else, ranges when the
/// resource honours them, and a one-shot "send less than promised".
struct Web {
    server: TestServer,
    resources: Arc<Mutex<HashMap<String, Resource>>>,
}

impl Web {
    async fn start() -> Self {
        let resources: Arc<Mutex<HashMap<String, Resource>>> = Arc::new(Mutex::new(HashMap::new()));
        let shared = Arc::clone(&resources);
        let server = TestServer::start(move |asked: &Asked, _| respond(&shared, asked)).await;
        Self { server, resources }
    }

    fn root(&self) -> String {
        self.server.url("/show/")
    }

    fn add(&self, name: &str, body: &[u8], content_type: &str, honours_ranges: bool) {
        self.resources.lock().insert(
            format!("/show/{name}"),
            Resource { body: body.to_vec(), content_type: content_type.into(), honours_ranges, cut_at: None },
        );
    }
    fn add_bytes(&self, name: &str, body: &[u8]) {
        self.add(name, body, "application/octet-stream", true);
    }
    fn add_text(&self, name: &str, text: &str) {
        self.add(name, text.as_bytes(), "application/vnd.apple.mpegurl", true);
    }
    fn remove(&self, name: &str) {
        self.resources.lock().remove(&format!("/show/{name}"));
    }
    fn cut_short(&self, name: &str, bytes: usize) {
        if let Some(r) = self.resources.lock().get_mut(&format!("/show/{name}")) {
            r.cut_at = Some(bytes);
        }
    }
    fn requests_for(&self, name: &str) -> Vec<Asked> {
        let path = format!("/show/{name}");
        self.server.requests().into_iter().filter(|r| r.path == path).collect()
    }
}

fn respond(resources: &Mutex<HashMap<String, Resource>>, asked: &Asked) -> Reply {
    let mut resources = resources.lock();
    let Some(resource) = resources.get_mut(&asked.path) else { return Reply::status(404) };
    let total = resource.body.len();
    let (mut from, mut to, mut partial) = (0, total.saturating_sub(1), false);
    if let Some(range) = asked.header("Range").filter(|_| resource.honours_ranges) {
        let spec = range.trim_start_matches("bytes=");
        let (a, b) = spec.split_once('-').unwrap_or((spec, ""));
        from = a.parse().unwrap_or(0);
        to = b.parse::<usize>().map(|t| t.min(total - 1)).unwrap_or(total - 1);
        partial = true;
    }
    let slice = resource.body[from..=to].to_vec();
    let sent = match resource.cut_at.take() {
        Some(cut) => slice[..cut.min(slice.len())].to_vec(),
        None => slice.clone(),
    };
    let mut reply = Reply::status(if partial { 206 } else { 200 }).with_body(&sent).header("Content-Type", &resource.content_type);
    if partial {
        reply = reply.header("Content-Range", &format!("bytes {from}-{to}/{total}"));
    }
    // Promised in full, even when cut short.
    reply.declared(slice.len() as u64)
}

struct FakeSettings(Arc<DownloadSettings>, EventHub<Arc<DownloadSettings>>);

impl traits::SettingsService for FakeSettings {
    fn current(&self) -> Arc<DownloadSettings> {
        self.0.clone()
    }
    fn subscribe(&self) -> EventStream<Arc<DownloadSettings>> {
        self.1.subscribe()
    }
    fn load(&self) {}
    fn save(&self, _settings: DownloadSettings) {}
}

/// The real merger, which can be told to refuse once.
#[derive(Default)]
struct FlakyMerger {
    inner: FileMerger,
    fail_next_merge: Mutex<bool>,
}

#[async_trait]
impl traits::FileMerger for FlakyMerger {
    async fn merge(
        &self,
        temp_folder: &str,
        ordered_chunk_paths: &[String],
        destination_file_path: &str,
        buffer_size: usize,
        bytes_merged: Option<ProgressFn<i64>>,
        cancel: &CancellationToken,
    ) -> quickbyte_core::Result<()> {
        if std::mem::take(&mut *self.fail_next_merge.lock()) {
            return Err(CoreError::Io("Access to the destination is denied.".into()));
        }
        self.inner.merge(temp_folder, ordered_chunk_paths, destination_file_path, buffer_size, bytes_merged, cancel).await
    }
    fn has_interrupted_merge(&self, temp_folder: &str) -> bool {
        self.inner.has_interrupted_merge(temp_folder)
    }
    fn discard_interrupted_merge(&self, temp_folder: &str, destination_file_path: &str) {
        self.inner.discard_interrupted_merge(temp_folder, destination_file_path);
    }
    fn cleanup_chunks(&self, chunk_paths: &[String], temp_folder: &str) {
        self.inner.cleanup_chunks(chunk_paths, temp_folder);
    }
}

struct Fixture {
    _root: TempFolder,
    web: Web,
    settings: Arc<FakeSettings>,
    merger: Arc<FlakyMerger>,
    statuses: Arc<Mutex<Vec<DownloadStatus>>>,
}

impl Fixture {
    async fn new(max_retries: i32) -> Self {
        let root = TempFolder::new();
        let settings = DownloadSettings {
            temp_folder: root.file("temp"),
            default_download_folder: root.file("downloads"),
            max_retries,
            retry_delay_milliseconds: 1,
            progress_update_interval_milliseconds: 50,
            ..Default::default()
        };
        std::fs::create_dir_all(&settings.default_download_folder).unwrap();
        Self {
            _root: root,
            web: Web::start().await,
            settings: Arc::new(FakeSettings(Arc::new(settings), EventHub::default())),
            merger: Arc::new(FlakyMerger::default()),
            statuses: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn create(&self, source: StreamSource, file_name: &str, url: Option<String>) -> Arc<StreamDownloadService> {
        let item = DownloadItem {
            url: url.unwrap_or_else(|| source.main_playlist_url.clone().unwrap_or_default()),
            file_name: file_name.into(),
            save_folder: self.settings.0.default_download_folder.clone(),
            connections_count: 4,
            supports_resume: true,
            stream: Some(source),
            ..Default::default()
        };
        let service = StreamDownloadService::new(
            item,
            direct_client(),
            self.merger.clone(),
            self.settings.clone(),
            Arc::new(|_| {}),
            StreamServiceOptions { mark_from_internet: false, ..Default::default() },
        );
        let statuses = Arc::clone(&self.statuses);
        service.subscribe_fn(Arc::new(move |e| {
            if let DownloadEvent::StatusChanged(s) = e {
                statuses.lock().push(s.new_status);
            }
        }));
        service
    }

    fn hls(&self, playlist: &str) -> StreamSource {
        StreamSource { format: StreamFormat::Hls, main_playlist_url: Some(format!("{}{playlist}", self.web.root())), ..Default::default() }
    }

    fn add_ts_playlist(&self, name: &str, pieces: &[Vec<u8>]) {
        let mut text = String::from("#EXTM3U\n");
        for (i, piece) in pieces.iter().enumerate() {
            self.web.add_bytes(&format!("p{i}.ts"), piece);
            text.push_str(&format!("#EXTINF:4,\np{i}.ts\n"));
        }
        text.push_str("#EXT-X-ENDLIST\n");
        self.web.add_text(name, &text);
    }
}

fn pieces(count: usize, size: usize) -> Vec<Vec<u8>> {
    (0..count).map(|i| random_bytes(size + (i * 37) % 300)).collect()
}

fn encrypt(plain: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
    cbc::Encryptor::<aes::Aes128>::new(key.into(), iv.into()).encrypt_padded_vec_mut::<Pkcs7>(plain)
}

fn sequence_iv(sequence: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&sequence.to_be_bytes());
    iv
}

fn key() -> [u8; 16] {
    random_bytes(16).try_into().unwrap()
}

fn read(path: &str) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encrypted_hls_video_and_its_separate_sound_are_saved_decrypted_and_in_order() {
    let f = Fixture::new(0).await;
    let key = key();
    let explicit_iv: [u8; 16] = random_bytes(16).try_into().unwrap();
    f.web.add_bytes("key.bin", &key);

    // Pieces 0 and 1 under the explicit IV; 2 under its sequence number (12).
    let video = pieces(3, 5000);
    f.web.add_bytes("v0.ts", &encrypt(&video[0], &key, &explicit_iv));
    f.web.add_bytes("v1.ts", &encrypt(&video[1], &key, &explicit_iv));
    f.web.add_bytes("v2.ts", &encrypt(&video[2], &key, &sequence_iv(12)));
    let hex: String = explicit_iv.iter().map(|b| format!("{b:02X}")).collect();
    f.web.add_text(
        "video.m3u8",
        &format!(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x{hex}\n#EXTINF:4,\nv0.ts\n#EXTINF:4,\nv1.ts\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4,\nv2.ts\n#EXT-X-ENDLIST\n"
        ),
    );

    let init = b"ftyp-init-section".to_vec();
    let audio = pieces(2, 3000);
    f.web.add_bytes("a-init.mp4", &init);
    f.web.add_bytes("a0.m4s", &audio[0]);
    f.web.add_bytes("a1.m4s", &audio[1]);
    f.web.add_text("audio.m3u8", "#EXTM3U\n#EXT-X-MAP:URI=\"a-init.mp4\"\n#EXTINF:4,\na0.m4s\n#EXTINF:4,\na1.m4s\n#EXT-X-ENDLIST\n");

    let source = StreamSource {
        audio_playlist_url: Some(format!("{}audio.m3u8", f.web.root())),
        audio_file_name: Some("Talk.audio.m4a".into()),
        ..f.hls("video.m3u8")
    };
    let service = f.create(source, "Talk.ts", None);
    service.start().await.unwrap();

    assert_eq!(*f.statuses.lock(), [DownloadStatus::Connecting, DownloadStatus::Downloading, DownloadStatus::Merging, DownloadStatus::Completed]);
    let item = service.item();
    assert_eq!(read(&item.full_path()), video.concat());
    assert_eq!(read(&item.audio_full_path().unwrap()), [vec![init], audio].concat().concat());
    let sizes = std::fs::metadata(item.full_path()).unwrap().len() + std::fs::metadata(item.audio_full_path().unwrap()).unwrap().len();
    assert_eq!(item.total_bytes as u64, sizes);
    assert!(!std::path::Path::new(&item.temp_folder_path).exists());

    // One key request however many pieces share the key.
    assert_eq!(f.web.requests_for("key.bin").len(), 1);
}

#[tokio::test]
async fn a_retry_fetches_only_the_pieces_that_are_missing() {
    let f = Fixture::new(0).await;
    let all = pieces(4, 2000);
    f.add_ts_playlist("index.m3u8", &all);
    f.web.remove("p2.ts");

    let service = f.create(f.hls("index.m3u8"), "video.ts", None);
    service.start().await.unwrap();
    assert_eq!(service.item().status, DownloadStatus::Failed);
    assert!(service.item().error_message.unwrap().contains("404"));

    f.web.add_bytes("p2.ts", &all[2]);
    service.retry().await.unwrap();

    assert_eq!(service.item().status, DownloadStatus::Completed);
    assert_eq!(read(&service.item().full_path()), all.concat());
    for i in [0, 1, 3] {
        assert_eq!(f.web.requests_for(&format!("p{i}.ts")).len(), 1);
    }
}

#[tokio::test]
async fn a_piece_whose_connection_ends_early_carries_on_from_what_arrived() {
    let f = Fixture::new(2).await;
    let all = pieces(2, 10_000);
    f.add_ts_playlist("index.m3u8", &all);
    f.web.cut_short("p1.ts", 4096);

    let service = f.create(f.hls("index.m3u8"), "video.ts", None);
    service.start().await.unwrap();

    assert_eq!(service.item().status, DownloadStatus::Completed, "{:?}", service.item().error_message);
    assert_eq!(read(&service.item().full_path()), all.concat());
    assert!(f.web.requests_for("p1.ts").iter().any(|r| r.header("Range").as_deref() == Some("bytes=4096-")));
}

#[tokio::test]
async fn an_encrypted_piece_whose_connection_ends_early_starts_again_rather_than_resuming() {
    // What is on disk is plaintext; carrying on would decrypt the rest of the
    // piece against the wrong CBC block and write noise into the video.
    let f = Fixture::new(2).await;
    let key = key();
    let all = pieces(2, 10_000);
    f.web.add_bytes("key.bin", &key);
    f.web.add_bytes("p0.ts", &encrypt(&all[0], &key, &sequence_iv(0)));
    f.web.add_bytes("p1.ts", &encrypt(&all[1], &key, &sequence_iv(1)));
    f.web.add_text("index.m3u8", "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4,\np0.ts\n#EXTINF:4,\np1.ts\n#EXT-X-ENDLIST\n");
    f.web.cut_short("p1.ts", 4096);

    let service = f.create(f.hls("index.m3u8"), "video.ts", None);
    service.start().await.unwrap();

    assert_eq!(service.item().status, DownloadStatus::Completed, "{:?}", service.item().error_message);
    assert_eq!(read(&service.item().full_path()), all.concat());
    assert!(f.web.requests_for("p1.ts").iter().all(|r| r.header("Range").is_none()));
}

#[tokio::test]
async fn byte_range_pieces_are_cut_correctly_even_from_a_server_that_ignores_ranges() {
    let f = Fixture::new(0).await;
    let whole = random_bytes(9000);
    f.web.add("all.ts", &whole, "application/octet-stream", false);
    f.web.add_text("index.m3u8", "#EXTM3U\n#EXTINF:4,\n#EXT-X-BYTERANGE:3000@1000\nall.ts\n#EXTINF:4,\n#EXT-X-BYTERANGE:2500\nall.ts\n#EXT-X-ENDLIST\n");

    let service = f.create(f.hls("index.m3u8"), "video.ts", None);
    service.start().await.unwrap();

    assert_eq!(service.item().status, DownloadStatus::Completed, "{:?}", service.item().error_message);
    assert_eq!(read(&service.item().full_path()), whole[1000..6500]);
}

#[tokio::test]
async fn a_dash_video_and_its_sound_are_saved_from_their_templates() {
    let f = Fixture::new(0).await;
    let video = pieces(2, 4000);
    let audio = pieces(2, 1000);
    f.web.add_bytes("v/init.mp4", b"video-init");
    f.web.add_bytes("v/1.m4s", &video[0]);
    f.web.add_bytes("v/2.m4s", &video[1]);
    f.web.add_bytes("a/init.mp4", b"audio-init");
    f.web.add_bytes("a/1.m4s", &audio[0]);
    f.web.add_bytes("a/2.m4s", &audio[1]);
    f.web.add(
        "manifest.mpd",
        br#"<MPD mediaPresentationDuration="PT8S"><Period>
  <AdaptationSet mimeType="video/mp4">
    <SegmentTemplate duration="4" initialization="$RepresentationID$/init.mp4" media="$RepresentationID$/$Number$.m4s"/>
    <Representation id="v" bandwidth="1"/>
  </AdaptationSet>
  <AdaptationSet mimeType="audio/mp4">
    <SegmentTemplate duration="4" initialization="$RepresentationID$/init.mp4" media="$RepresentationID$/$Number$.m4s"/>
    <Representation id="a" bandwidth="1"/>
  </AdaptationSet>
</Period></MPD>"#,
        "application/dash+xml",
        true,
    );

    let source = StreamSource {
        format: StreamFormat::Dash,
        main_representation: Some("v".into()),
        audio_representation: Some("a".into()),
        audio_file_name: Some("clip.audio.m4a".into()),
        ..Default::default()
    };
    let service = f.create(source, "clip.mp4", Some(format!("{}manifest.mpd", f.web.root())));
    service.start().await.unwrap();

    let item = service.item();
    assert_eq!(item.status, DownloadStatus::Completed, "{:?}", item.error_message);
    assert_eq!(read(&item.full_path()), [b"video-init".to_vec(), video.concat()].concat());
    assert_eq!(read(&item.audio_full_path().unwrap()), [b"audio-init".to_vec(), audio.concat()].concat());
}

#[tokio::test]
async fn pieces_from_a_playlist_that_has_since_changed_shape_are_thrown_away() {
    let f = Fixture::new(0).await;
    let before = pieces(3, 1500);
    f.add_ts_playlist("index.m3u8", &before);
    f.web.remove("p2.ts");

    let service = f.create(f.hls("index.m3u8"), "video.ts", None);
    service.start().await.unwrap();
    assert_eq!(service.item().status, DownloadStatus::Failed);

    // A re-cut of the video: two pieces, different bytes, same names.
    let after = pieces(2, 1800);
    f.add_ts_playlist("index.m3u8", &after);
    service.retry().await.unwrap();

    assert_eq!(service.item().status, DownloadStatus::Completed);
    assert_eq!(read(&service.item().full_path()), after.concat());
}

#[tokio::test]
async fn a_download_whose_pieces_are_all_down_finishes_without_its_playlist() {
    // The merge failing (here, for want of access) must not send the next run
    // back to a playlist that may well have expired by then.
    let f = Fixture::new(0).await;
    let all = pieces(3, 1200);
    f.add_ts_playlist("index.m3u8", &all);
    *f.merger.fail_next_merge.lock() = true;

    let service = f.create(f.hls("index.m3u8"), "video.ts", None);
    service.start().await.unwrap();
    assert_eq!(service.item().status, DownloadStatus::Failed);

    f.web.remove("index.m3u8");
    service.retry().await.unwrap();

    assert_eq!(service.item().status, DownloadStatus::Completed, "{:?}", service.item().error_message);
    assert_eq!(read(&service.item().full_path()), all.concat());
    assert_eq!(f.web.requests_for("index.m3u8").len(), 1);
}

#[tokio::test]
async fn a_key_server_that_answers_with_something_other_than_a_key_fails_the_download_plainly() {
    let f = Fixture::new(3).await;
    f.web.add("key.bin", b"<html>sign in</html>", "text/html", true);
    f.web.add_bytes("p0.ts", &[0u8; 32]);
    f.web.add_text("index.m3u8", "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4,\np0.ts\n#EXT-X-ENDLIST\n");

    let service = f.create(f.hls("index.m3u8"), "video.ts", None);
    service.start().await.unwrap();

    assert_eq!(service.item().status, DownloadStatus::Failed);
    assert!(service.item().error_message.unwrap().contains("decryption key"));
    assert_eq!(f.web.requests_for("key.bin").len(), 1); // not retried
}

#[tokio::test]
async fn a_live_stream_fails_with_the_reason_rather_than_downloading_forever() {
    let f = Fixture::new(0).await;
    f.web.add_text("index.m3u8", "#EXTM3U\n#EXTINF:4,\np0.ts\n");

    let service = f.create(f.hls("index.m3u8"), "video.ts", None);
    service.start().await.unwrap();

    assert_eq!(service.item().status, DownloadStatus::Failed);
    assert_eq!(service.item().error_message.as_deref(), Some(LIVE_MESSAGE));
}

#[tokio::test]
async fn pieces_are_fetched_with_the_downloads_cookies() {
    let f = Fixture::new(0).await;
    f.add_ts_playlist("index.m3u8", &pieces(2, 100));

    let service = f.create(f.hls("index.m3u8"), "video.ts", None);
    service.update_item(&mut |i| {
        i.headers.insert("Cookie".into(), "session=7".into());
    });
    service.start().await.unwrap();

    assert_eq!(service.item().status, DownloadStatus::Completed);
    let requests = f.web.server.requests();
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|r| r.header("Cookie").as_deref() == Some("session=7")));
}
