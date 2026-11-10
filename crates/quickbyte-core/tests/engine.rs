//! The engine end to end against local servers: dynamic segmentation and the
//! per-connection commands over real connections (the end-to-end halves of
//! `DynamicSegmentationTests.cs`), a download paused by the C# build resumed
//! from its chunk folder by this one (MIGRATION.md §5.7), and streaming videos
//! (`StreamDownloadServiceTests.cs`).

mod support;

#[path = "engine/streams.rs"]
mod streams;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use quickbyte_core::enums::{ConnectionCommand, ConnectionStatus, DownloadStatus};
use quickbyte_core::events::NoopDispatcher;
use quickbyte_core::models::{DownloadItem, DownloadSettings, RemoteFileInfo};
use quickbyte_core::net::download_connection::HttpConnectionFactory;
use quickbyte_core::net::remote_file_info_provider::RemoteFileInfoProvider;
use quickbyte_core::services::connection_pool_manager::ConnectionPoolManager;
use quickbyte_core::services::download_manager::DownloadManager;
use quickbyte_core::services::download_repository::DownloadRepository;
use quickbyte_core::services::file_merger::FileMerger;
use quickbyte_core::services::settings_service::SettingsService;
use quickbyte_core::traits::{ConnectionFactory, ConnectionPoolManager as _, ConnectionSpec, DownloadConnection, DownloadManager as _, FileMerger as _};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::support::{TempFolder, direct_client};

const MIB: usize = 1024 * 1024;
const ETAG: &str = "\"v1\"";
const LAST_MODIFIED: &str = "Wed, 21 Oct 2015 07:28:00 GMT";

pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

// ---------------------------------------------------------------------------
// A server that serves ranges of a byte array, slowly from `slow_from` onwards
// ---------------------------------------------------------------------------

/// Always streams to the end of the requested range, the way a real server does
/// — so a connection shortened mid-transfer is still being sent bytes past its
/// new end. Answers HEAD and a `bytes=0-0` probe like an origin that serves ranges.
struct RangeServer {
    port: u16,
    stop: CancellationToken,
    ranges: Arc<Mutex<Vec<String>>>,
}

impl RangeServer {
    async fn start(content: Arc<Vec<u8>>, slow_from: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = CancellationToken::new();
        let ranges = Arc::new(Mutex::new(Vec::new()));
        {
            let (stop, ranges) = (stop.clone(), Arc::clone(&ranges));
            tokio::spawn(async move {
                loop {
                    let accepted = tokio::select! {
                        _ = stop.cancelled() => return,
                        a = listener.accept() => a,
                    };
                    let Ok((stream, _)) = accepted else { continue };
                    tokio::spawn(serve_ranges(stream, Arc::clone(&content), slow_from, Arc::clone(&ranges)));
                }
            });
        }
        Self { port, stop, ranges }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/file.bin", self.port)
    }
}

impl Drop for RangeServer {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn serve_ranges(mut stream: TcpStream, content: Arc<Vec<u8>>, slow_from: usize, ranges: Arc<Mutex<Vec<String>>>) {
    loop {
        let mut head = Vec::new();
        let mut one = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read(&mut one).await {
                Ok(1) => head.push(one[0]),
                _ => return,
            }
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        let method = head.split(' ').next().unwrap_or("").to_string();
        let range = head
            .lines()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("range")).map(|(_, v)| v.trim().to_string()));

        let total = content.len();
        let common = format!("Accept-Ranges: bytes\r\nETag: {ETAG}\r\nLast-Modified: {LAST_MODIFIED}\r\nContent-Type: application/octet-stream\r\n");
        let (status, from, to) = match &range {
            Some(spec) => {
                ranges.lock().push(spec.clone());
                let spec = spec.trim_start_matches("bytes=");
                let (a, b) = spec.split_once('-').unwrap_or((spec, ""));
                let from: usize = a.parse().unwrap_or(0);
                let to: usize = b.parse().map(|t: usize| t.min(total - 1)).unwrap_or(total - 1);
                (206, from, to)
            }
            None => (200, 0, total - 1),
        };
        let mut text = if status == 206 {
            format!("HTTP/1.1 206 Partial Content\r\n{common}Content-Range: bytes {from}-{to}/{total}\r\n")
        } else {
            format!("HTTP/1.1 200 OK\r\n{common}")
        };
        text.push_str(&format!("Content-Length: {}\r\n\r\n", to - from + 1));
        if stream.write_all(text.as_bytes()).await.is_err() {
            return;
        }
        if method == "HEAD" {
            continue;
        }
        let mut position = from;
        while position <= to {
            if position >= slow_from {
                tokio::time::sleep(Duration::from_millis(8)).await;
            }
            let end = (position + 64 * 1024).min(to + 1);
            if stream.write_all(&content[position..end]).await.is_err() {
                return;
            }
            position = end;
        }
    }
}

/// Real HTTP connections, counted.
struct CountingFactory {
    inner: HttpConnectionFactory,
    created: AtomicUsize,
}

impl CountingFactory {
    fn new() -> Arc<Self> {
        Arc::new(Self { inner: HttpConnectionFactory::with_client(direct_client()), created: AtomicUsize::new(0) })
    }
}

impl ConnectionFactory for CountingFactory {
    fn create(&self, spec: ConnectionSpec) -> Arc<dyn DownloadConnection> {
        self.created.fetch_add(1, Ordering::SeqCst);
        self.inner.create(spec)
    }
}

fn item(folder: &TempFolder, url: &str, connections: i32) -> DownloadItem {
    DownloadItem { url: url.into(), connections_count: connections, temp_folder_path: folder.path.to_string_lossy().into_owned(), ..Default::default() }
}

fn info(length: usize) -> RemoteFileInfo {
    RemoteFileInfo { content_length: length as i64, supports_range_requests: true, ..Default::default() }
}

fn settings(dynamic: bool) -> Arc<DownloadSettings> {
    Arc::new(DownloadSettings {
        dynamic_segmentation: dynamic,
        max_retries: 0,
        retry_delay_milliseconds: 0,
        stream_buffer_size_bytes: 64 * 1024,
        ..Default::default()
    })
}

async fn merge_and_hash(folder: &TempFolder, pool: &ConnectionPoolManager) -> Vec<u8> {
    let destination = folder.file("merged.bin");
    FileMerger::new()
        .merge(&folder.path.to_string_lossy(), &pool.ordered_chunk_paths(), &destination, 64 * 1024, None, &CancellationToken::new())
        .await
        .unwrap();
    Sha256::digest(std::fs::read(destination).unwrap()).to_vec()
}

// ---------------------------------------------------------------------------
// Dynamic segmentation, end to end
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_connections_split_mid_transfer_still_merge_into_exactly_the_file() {
    // The first half is served at full speed and the second deliberately slowly,
    // so the fast connection finishes while the slow one still has megabytes to
    // go — and the pool splits it, possibly more than once, with the old
    // connection's response still streaming past the new end.
    let folder = TempFolder::new();
    let content = Arc::new(random_bytes(6 * MIB));
    let server = RangeServer::start(Arc::clone(&content), 3 * MIB).await;
    let factory = CountingFactory::new();
    let pool = ConnectionPoolManager::new(factory.clone(), None);

    let finished = pool.run(&item(&folder, &server.url(), 2), &info(content.len()), settings(true), CancellationToken::new()).await.unwrap();

    assert!(finished);
    assert!(factory.created.load(Ordering::SeqCst) > 2, "the slow connection was never split");
    assert_eq!(merge_and_hash(&folder, &pool).await, Sha256::digest(&*content).to_vec());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_connections_paused_resumed_and_restarted_mid_transfer_still_merge_into_exactly_the_file() {
    // The per-connection commands stop a real connection with its chunk open and
    // its response half read. Resume carries on from the chunk's length on disk
    // and Restart rewrites it from nothing; either getting an offset wrong is a
    // corrupt file and no error anywhere.
    let folder = TempFolder::new();
    let content = Arc::new(random_bytes(4 * MIB));
    let server = RangeServer::start(Arc::clone(&content), 0).await;
    let factory = CountingFactory::new();
    let pool = Arc::new(ConnectionPoolManager::new(factory.clone(), None));

    let running = {
        let (pool, item, info) = (Arc::clone(&pool), item(&folder, &server.url(), 2), info(content.len()));
        tokio::spawn(async move { pool.run(&item, &info, settings(false), CancellationToken::new()).await })
    };

    let bytes_of = |id: i32| pool.snapshot().iter().find(|c| c.connection_id == id).map_or(0, |c| c.bytes_downloaded);
    let status_of = |id: i32| pool.snapshot().iter().find(|c| c.connection_id == id).map(|c| c.status);
    async fn until(condition: impl Fn() -> bool) {
        for _ in 0..600 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(condition());
    }

    until(|| bytes_of(1) > 256 * 1024 && bytes_of(0) > 256 * 1024).await;
    assert!(pool.control_connection(1, ConnectionCommand::Pause));
    assert!(pool.control_connection(0, ConnectionCommand::Restart));

    until(|| status_of(1) == Some(ConnectionStatus::Paused)).await;
    let paused_at = bytes_of(1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(bytes_of(1), paused_at);

    assert!(pool.control_connection(1, ConnectionCommand::Resume));
    assert!(running.await.unwrap().unwrap());
    assert_eq!(factory.created.load(Ordering::SeqCst), 4);

    assert_eq!(merge_and_hash(&folder, &pool).await, Sha256::digest(&*content).to_vec());
}

// ---------------------------------------------------------------------------
// A download the C# build paused, resumed by this one
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_paused_by_the_c_sharp_build_resumes_from_its_chunks_and_layout() {
    let profile = TempFolder::new();
    let downloads = profile.file("Downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let content = Arc::new(random_bytes(MIB));
    let server = RangeServer::start(Arc::clone(&content), usize::MAX).await;

    // The chunk folder exactly as QuickByte 1.26 leaves it: `temp\<guid N>`, the
    // layout after one split (chunk 1 shortened, chunk 2 its back half), and
    // three partly written chunks.
    let id = uuid::Uuid::new_v4();
    let temp = profile.path.join("temp").join(id.simple().to_string());
    std::fs::create_dir_all(&temp).unwrap();
    std::fs::write(
        temp.join("segments.json"),
        "{\"TotalBytes\":1048576,\"Segments\":[{\"Chunk\":0,\"Start\":0,\"End\":349999,\"Length\":350000},\
         {\"Chunk\":1,\"Start\":350000,\"End\":524999,\"Length\":175000},\
         {\"Chunk\":2,\"Start\":525000,\"End\":1048575,\"Length\":523576}],\"NextChunk\":3}",
    )
    .unwrap();
    std::fs::write(temp.join("part0.tmp"), &content[..100_000]).unwrap();
    std::fs::write(temp.join("part1.tmp"), &content[350_000..400_000]).unwrap();
    std::fs::write(temp.join("part2.tmp"), &content[525_000..525_010]).unwrap();

    // downloads.json as the C# build wrote it: PascalCase, enums as numbers,
    // the computed properties included, the download Paused.
    let host = format!("127.0.0.1:{}", server.port);
    let json = format!(
        r#"[
  {{
    "Id": "{id}",
    "Url": {url},
    "FileName": "resumed.bin",
    "SaveFolder": {folder},
    "ContentType": "application/octet-stream",
    "TotalBytes": 1048576,
    "DownloadedBytes": 150010,
    "ConnectionsCount": 2,
    "SupportsResume": true,
    "Credentials": null,
    "Headers": {{}},
    "SpeedLimitBytesPerSecond": 0,
    "OnCompletion": 0,
    "PowerActionWhenDone": 0,
    "ExpectedChecksum": null,
    "ChecksumResult": 0,
    "Status": 3,
    "ErrorMessage": null,
    "CurrentSpeedBytesPerSecond": 0,
    "EstimatedTimeRemaining": null,
    "MergeProgressPercentage": 0,
    "CreatedAt": "2026-11-09T10:08:25.1128086+00:00",
    "CompletedAt": null,
    "FullPath": {full},
    "Stream": null,
    "AudioFullPath": null,
    "TempFolderPath": {temp},
    "BypassCache": false,
    "RemoteVersion": {{ "ETag": "\"v1\"", "LastModified": "2015-10-21T07:28:00+00:00", "Host": "{host}", "HasValidator": true, "HasStrongETag": true }},
    "IfRangeUnsupported": false,
    "ProgressPercentage": 14.306,
    "Category": 5
  }}
]"#,
        url = serde_json::to_string(&server.url()).unwrap(),
        folder = serde_json::to_string(&downloads).unwrap(),
        full = serde_json::to_string(&Path::new(&downloads).join("resumed.bin").to_string_lossy()).unwrap(),
        temp = serde_json::to_string(&temp.to_string_lossy()).unwrap(),
    );
    std::fs::write(profile.path.join("downloads.json"), json).unwrap();

    let settings_service = Arc::new(SettingsService::new(Some(&profile.path)));
    quickbyte_core::traits::SettingsService::save(&*settings_service, DownloadSettings { default_download_folder: downloads.clone(), ..Default::default() });
    let manager = DownloadManager::new(
        Arc::new(DownloadRepository::new(Some(&profile.path))),
        settings_service,
        Arc::new(RemoteFileInfoProvider::with_client(direct_client())),
        CountingFactory::new(),
        Arc::new(FileMerger::new()),
        Arc::new(NoopDispatcher),
        false,
        tokio::runtime::Handle::current(),
    );
    manager.load_persisted_downloads();
    assert_eq!(manager.get(id).unwrap().status, DownloadStatus::Paused);

    manager.resume(id).await.unwrap();

    let item = manager.get(id).unwrap();
    assert_eq!(item.status, DownloadStatus::Completed, "{:?}", item.error_message);
    assert_eq!(std::fs::read(item.full_path()).unwrap(), *content);
    assert!(!temp.exists());

    // Every range carried on from what its chunk already held.
    let ranges = server.ranges.lock().clone();
    for wanted in ["bytes=100000-349999", "bytes=400000-524999", "bytes=525010-1048575"] {
        assert!(ranges.iter().any(|r| r == wanted), "{wanted} not in {ranges:?}");
    }
    assert!(!ranges.iter().any(|r| r == "bytes=0-349999"));

    // And the list is written back in the same file format.
    manager.flush();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(profile.path.join("downloads.json")).unwrap()).unwrap();
    assert_eq!(saved[0]["Status"], 5);
    assert_eq!(saved[0]["Id"], id.to_string());
}
