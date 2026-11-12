//! `--demo`: in-memory fakes of every Core service the UI codes against, with a
//! small simulated engine ticking on the tokio runtime.
//!
//! It exists so the UI can be run, looked at and screenshotted before the real
//! engine lands (and afterwards without touching anyone's downloads). Nothing
//! here reads or writes the disk, the registry or `%APPDATA%\QuickByte`, and no
//! network request is made. The engine is a toy: connections advance at a
//! jittered speed, finish, the file "merges", and the completion flow runs —
//! enough to exercise every window and every event the real manager publishes.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use quickbyte_core::enums::{
    CompletionActions, ConnectionCommand, ConnectionStatus, DownloadStatus, PowerAction,
    QueueState, ScheduleDays,
};
use quickbyte_core::error::{CoreError, Result};
use quickbyte_core::events::{
    ConnectionsSnapshot, Dispatcher, DownloadEvent, DownloadListChangeType, DownloadProgress,
    DownloadStatusChanged, EventHub, EventStream, QueueChangeType, QueueEvent, VirusTotalEvent,
};
use quickbyte_core::helpers::file_checksum::{ChecksumResult, ExpectedChecksum};
use quickbyte_core::models::{
    AddressChange, ConnectionInfo, DownloadItem, DownloadQueue, DownloadRequest, DownloadSettings,
    RemoteFileInfo, RequestOptions, StreamFormat, StreamSource, UpdateCheckResult,
    UpdateDownloadProgress, UpdateManifest, VirusTotalReport, VirusTotalSource, VirusTotalStep,
    VirusTotalVerdict,
};
use quickbyte_core::serde_compat::{DateTimeOffset, TimeSpan};
use quickbyte_core::streaming::{StreamChoice, StreamDescription};
use quickbyte_core::traits::{
    BrowserEvent, BrowserIntegrationService, DownloadManager, ProgressFn, QueueManager,
    RemoteFileInfoProvider, SettingsService, StreamResolver, UpdateService, VirusTotalService,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::services::AppServices;

const MB: i64 = 1024 * 1024;
const TICK: Duration = Duration::from_millis(100);

/// Builds the whole set of demo services and starts the simulated engine.
pub fn services(dispatcher: Arc<dyn Dispatcher>, runtime: tokio::runtime::Handle) -> AppServices {
    let settings = Arc::new(DemoSettings::new(dispatcher.clone()));
    let downloads = Arc::new(DemoDownloads::new(dispatcher.clone()));
    let queues = Arc::new(DemoQueues::new(dispatcher.clone(), downloads.clone()));
    downloads.seed(&queues);
    DemoDownloads::start_engine(downloads.clone(), queues.clone(), &runtime);
    let virus_total = Arc::new(DemoVirusTotal::new(dispatcher.clone(), downloads.clone()));
    AppServices {
        downloads,
        queues,
        settings,
        file_info: Arc::new(DemoFileInfo),
        streams: Arc::new(DemoStreams),
        updates: Arc::new(DemoUpdates::default()),
        browser: Arc::new(DemoBrowser::new(dispatcher)),
        virus_total,
        runtime,
    }
}

/// A tiny deterministic PRNG (xorshift) — the demo does not need `rand`.
struct Rng(u64);
impl Rng {
    fn next_f64(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

// --------------------------------------------------------------------------
// Settings
// --------------------------------------------------------------------------

pub struct DemoSettings {
    current: Mutex<Arc<DownloadSettings>>,
    hub: EventHub<Arc<DownloadSettings>>,
}

impl DemoSettings {
    fn new(dispatcher: Arc<dyn Dispatcher>) -> Self {
        let settings = DownloadSettings {
            browser_integration_token: "demo-token".into(),
            ..DownloadSettings::default()
        };
        Self {
            current: Mutex::new(Arc::new(settings)),
            hub: EventHub::new(Some(dispatcher)),
        }
    }
}

impl SettingsService for DemoSettings {
    fn current(&self) -> Arc<DownloadSettings> {
        self.current.lock().clone()
    }
    fn subscribe(&self) -> EventStream<Arc<DownloadSettings>> {
        self.hub.subscribe()
    }
    fn load(&self) {}
    fn save(&self, settings: DownloadSettings) {
        let settings = Arc::new(settings);
        *self.current.lock() = settings.clone();
        self.hub.publish(settings);
    }
}

// --------------------------------------------------------------------------
// Downloads
// --------------------------------------------------------------------------

struct Sim {
    item: DownloadItem,
    connections: Vec<ConnectionInfo>,
    /// Per-connection nominal speed, bytes/s.
    speeds: Vec<f64>,
    /// When a Connecting download starts receiving.
    connecting_until: Option<Instant>,
}

struct DownloadsState {
    sims: Vec<Sim>,
    rng: Rng,
}

pub struct DemoDownloads {
    state: Mutex<DownloadsState>,
    hub: EventHub<DownloadEvent>,
    global_limit: AtomicI64,
}

fn connection_layout(
    total: i64,
    count: i32,
    downloaded: i64,
    rng: &mut Rng,
    status: ConnectionStatus,
) -> Vec<ConnectionInfo> {
    if total <= 0 || count <= 0 {
        return Vec::new();
    }
    let n = count as i64;
    let size = total / n;
    let mut left = downloaded;
    (0..n)
        .map(|i| {
            let start = i * size;
            let end = if i == n - 1 {
                total - 1
            } else {
                start + size - 1
            };
            let span = end - start + 1;
            // Spread what is downloaded unevenly, as real connections are.
            let share = ((downloaded as f64 / n as f64) * (0.6 + rng.next_f64() * 0.8)) as i64;
            let got = share.min(span).min(left).max(0);
            left -= got;
            let st = if got >= span {
                ConnectionStatus::Finished
            } else {
                status
            };
            ConnectionInfo {
                connection_id: i as i32,
                range_start: start,
                range_end: end,
                bytes_downloaded: got,
                status: st,
                retry_count: 0,
                last_error: None,
            }
        })
        .collect()
}

impl DemoDownloads {
    fn new(dispatcher: Arc<dyn Dispatcher>) -> Self {
        Self {
            state: Mutex::new(DownloadsState {
                sims: Vec::new(),
                rng: Rng(0x9E37_79B9_7F4A_7C15),
            }),
            hub: EventHub::new(Some(dispatcher)),
            global_limit: AtomicI64::new(0),
        }
    }

    /// The twelve downloads of the website screenshots, in every status.
    fn seed(&self, queues: &DemoQueues) {
        let folder = quickbyte_core::models::default_download_folder();
        let days_ago = |d: i64, h: u32| {
            let now = chrono::Local::now().fixed_offset();
            let t = now - chrono::TimeDelta::days(d) - chrono::TimeDelta::hours(h as i64);
            DateTimeOffset(t)
        };
        #[allow(clippy::type_complexity)]
        let rows: [(&str, &str, i64, f64, DownloadStatus, &str, i32); 12] = [
            (
                "game-assets-bundle.zip",
                "https://cdn.example.com/games/game-assets-bundle.zip",
                1485 * MB,
                0.72,
                DownloadStatus::Downloading,
                "application/zip",
                8,
            ),
            (
                "mountain-flight-4k.mp4",
                "https://media.example.net/video/mountain-flight-4k.mp4",
                1843 * MB,
                0.22,
                DownloadStatus::Downloading,
                "video/mp4",
                8,
            ),
            (
                "fedora-workstation-43.iso",
                "http://downloads.example.org/images/fedora-workstation-43.iso",
                2355 * MB,
                0.17,
                DownloadStatus::Downloading,
                "application/octet-stream",
                8,
            ),
            (
                "sample-textures-pack.zip",
                "https://assets.example.com/packs/sample-textures-pack.zip",
                3185 * MB,
                0.0,
                DownloadStatus::Queued,
                "application/octet-stream",
                8,
            ),
            (
                "ubuntu-26.04-desktop-amd64.iso",
                "https://releases.example.org/26.04/ubuntu-26.04-desktop-amd64.iso",
                6123 * MB,
                0.0,
                DownloadStatus::Queued,
                "application/octet-stream",
                16,
            ),
            (
                "design-systems-talk-1080p.mp4",
                "https://talks.example.dev/2026/design-systems-talk-1080p.mp4",
                736 * MB,
                0.0,
                DownloadStatus::Queued,
                "application/octet-stream",
                4,
            ),
            (
                "camera-originals-2026.7z",
                "https://files.example.com/share/camera-originals-2026.7z",
                8417 * MB,
                0.62,
                DownloadStatus::Paused,
                "application/octet-stream",
                8,
            ),
            (
                "photo-archive.tar.gz",
                "https://backup.example.net/photo-archive.tar.gz",
                1290 * MB,
                0.34,
                DownloadStatus::Failed,
                "application/octet-stream",
                8,
            ),
            (
                "LibreOffice_26.8_Win_x86-64.msi",
                "https://download.example.org/libreoffice/26.8/LibreOffice_26.8_Win_x86-64.msi",
                348 * MB,
                1.0,
                DownloadStatus::Completed,
                "application/x-msi",
                8,
            ),
            (
                "annual-report-2026.pdf",
                "https://intranet.example.com/reports/annual-report-2026.pdf",
                18 * MB + 400 * 1024,
                1.0,
                DownloadStatus::Completed,
                "application/pdf",
                2,
            ),
            (
                "field-recordings-vol-3.flac",
                "https://audio.example.org/field-recordings-vol-3.flac",
                512 * MB,
                1.0,
                DownloadStatus::Completed,
                "audio/flac",
                8,
            ),
            (
                "vscode-setup-x64.exe",
                "https://update.example.com/vscode/vscode-setup-x64.exe",
                118 * MB,
                1.0,
                DownloadStatus::Completed,
                "application/x-msdownload",
                8,
            ),
        ];

        let mut st = self.state.lock();
        let mut queued_ids = Vec::new();
        for (index, (name, url, total, fraction, status, content_type, connections)) in
            rows.into_iter().enumerate()
        {
            let downloaded = (total as f64 * fraction) as i64;
            let mut item = DownloadItem {
                url: url.into(),
                file_name: name.into(),
                save_folder: folder.clone(),
                content_type: content_type.into(),
                total_bytes: total,
                downloaded_bytes: downloaded,
                connections_count: connections,
                supports_resume: index != 7,
                status,
                created_at: days_ago(index as i64 / 3, index as u32),
                ..DownloadItem::default()
            };
            if status == DownloadStatus::Failed {
                item.error_message = Some(
                    "The server refused the connection (HTTP 503 Service Unavailable).".into(),
                );
                // Captured from a page: Refresh Address offers to open it again.
                item.headers.insert(
                    "Referer".into(),
                    "https://backup.example.net/archive/".into(),
                );
            }
            if status == DownloadStatus::Completed {
                item.completed_at = Some(days_ago(index as i64 - 7, 2));
                if index == 8 {
                    item.expected_checksum = Some(
                        "sha256:9f2c4e1a7b3d5f60812a4c6e8b0d2f4a6c8e0b2d4f6a8c0e2b4d6f8a0c2e4b6d"
                            .into(),
                    );
                    item.checksum_result = ChecksumResult::Match;
                }
            }
            if index == 6 {
                item.speed_limit_bytes_per_second = 2 * MB;
            }
            if status == DownloadStatus::Queued {
                queued_ids.push(item.id);
            }
            let conn_status = match status {
                DownloadStatus::Downloading => ConnectionStatus::ReceivingData,
                DownloadStatus::Paused => ConnectionStatus::Paused,
                DownloadStatus::Failed => ConnectionStatus::Failed,
                _ => ConnectionStatus::Idle,
            };
            let mut conns = if matches!(
                status,
                DownloadStatus::Downloading | DownloadStatus::Paused | DownloadStatus::Failed
            ) {
                connection_layout(total, connections, downloaded, &mut st.rng, conn_status)
            } else {
                Vec::new()
            };
            item.downloaded_bytes = if conns.is_empty() {
                downloaded
            } else {
                conns.iter().map(|c| c.bytes_downloaded).sum()
            };
            if status == DownloadStatus::Failed
                && let Some(c) = conns.first_mut()
            {
                c.last_error = Some("HTTP 503".into());
            }
            let speeds = conns
                .iter()
                .map(|_| (1.2 + st.rng.next_f64() * 1.6) * MB as f64)
                .collect();
            st.sims.push(Sim {
                item,
                connections: conns,
                speeds,
                connecting_until: None,
            });
        }
        drop(st);
        queues.seed(queued_ids);
    }

    fn publish_status(&self, item: &DownloadItem, old: DownloadStatus) {
        self.hub
            .publish(DownloadEvent::StatusChanged(DownloadStatusChanged {
                download_id: item.id,
                old_status: old,
                new_status: item.status,
                error_message: item.error_message.clone(),
            }));
    }

    fn with_sim<R>(&self, id: Uuid, f: impl FnOnce(&mut Sim, &mut Rng) -> R) -> Option<R> {
        let mut st = self.state.lock();
        let DownloadsState { sims, rng } = &mut *st;
        sims.iter_mut().find(|s| s.item.id == id).map(|s| f(s, rng))
    }

    /// Moves a download into Connecting (or leaves it Queued when every slot is taken).
    fn begin(&self, id: Uuid) {
        let max = 3;
        let changed = {
            let mut st = self.state.lock();
            let active = st.sims.iter().filter(|s| s.item.status.is_active()).count();
            let DownloadsState { sims, rng } = &mut *st;
            let Some(sim) = sims.iter_mut().find(|s| s.item.id == id) else {
                return;
            };
            let old = sim.item.status;
            if old.is_active() {
                return;
            }
            sim.item.error_message = None;
            sim.item.status = if active >= max {
                DownloadStatus::Queued
            } else {
                DownloadStatus::Connecting
            };
            sim.connecting_until = Some(Instant::now() + Duration::from_millis(700));
            if sim.connections.is_empty() {
                sim.connections = connection_layout(
                    sim.item.total_bytes,
                    sim.item.connections_count,
                    sim.item.downloaded_bytes,
                    rng,
                    ConnectionStatus::SendingRequest,
                );
                sim.speeds = sim
                    .connections
                    .iter()
                    .map(|_| (1.2 + rng.next_f64() * 1.6) * MB as f64)
                    .collect();
            }
            for c in &mut sim.connections {
                if c.status != ConnectionStatus::Finished {
                    c.status = ConnectionStatus::SendingRequest;
                    c.last_error = None;
                }
            }
            (old != sim.item.status)
                .then(|| (sim.item.clone(), old, Arc::new(sim.connections.clone())))
        };
        if let Some((item, old, conns)) = changed {
            self.publish_status(&item, old);
            self.hub
                .publish(DownloadEvent::Connections(ConnectionsSnapshot {
                    download_id: item.id,
                    connections: conns,
                }));
        }
    }

    fn start_engine(this: Arc<Self>, queues: Arc<DemoQueues>, runtime: &tokio::runtime::Handle) {
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(TICK);
            loop {
                interval.tick().await;
                this.tick(&queues);
            }
        });
    }

    /// One engine sample: every running connection advances, a finished file
    /// merges and completes, a Connecting one starts receiving.
    fn tick(&self, queues: &DemoQueues) {
        let mut events = Vec::new();
        let mut finished_any = false;
        {
            let mut st = self.state.lock();
            let global = self.global_limit.load(Ordering::Relaxed);
            let DownloadsState { sims, rng } = &mut *st;
            let now = Instant::now();
            for sim in sims.iter_mut() {
                let item = &mut sim.item;
                match item.status {
                    DownloadStatus::Connecting if sim.connecting_until.is_none_or(|t| now >= t) => {
                        let old = item.status;
                        item.status = DownloadStatus::Downloading;
                        for c in &mut sim.connections {
                            if c.status == ConnectionStatus::SendingRequest {
                                c.status = ConnectionStatus::ReceivingData;
                            }
                        }
                        events.push(status_event(item, old));
                    }
                    DownloadStatus::Downloading => {
                        let mut speed_total = 0.0;
                        let cap = [item.speed_limit_bytes_per_second, global]
                            .into_iter()
                            .filter(|l| *l > 0)
                            .min();
                        let running = sim
                            .connections
                            .iter()
                            .filter(|c| c.status == ConnectionStatus::ReceivingData)
                            .count()
                            .max(1);
                        for (c, nominal) in sim.connections.iter_mut().zip(sim.speeds.iter()) {
                            if c.status != ConnectionStatus::ReceivingData {
                                continue;
                            }
                            let mut speed = nominal * (0.75 + rng.next_f64() * 0.5);
                            if let Some(cap) = cap {
                                speed = speed.min(cap as f64 / running as f64);
                            }
                            let step = (speed * TICK.as_secs_f64()) as i64;
                            c.bytes_downloaded = (c.bytes_downloaded + step).min(c.total_bytes());
                            if c.bytes_downloaded >= c.total_bytes() {
                                c.status = ConnectionStatus::Finished;
                            }
                            speed_total += speed;
                        }
                        item.downloaded_bytes =
                            sim.connections.iter().map(|c| c.bytes_downloaded).sum();
                        item.current_speed_bytes_per_second = speed_total;
                        let remaining = (item.total_bytes - item.downloaded_bytes).max(0);
                        item.estimated_time_remaining = (speed_total > 1.0)
                            .then(|| TimeSpan::from_secs_f64(remaining as f64 / speed_total));
                        events.push(DownloadEvent::Progress(progress_of(item, None)));
                        events.push(DownloadEvent::Connections(ConnectionsSnapshot {
                            download_id: item.id,
                            connections: Arc::new(sim.connections.clone()),
                        }));
                        if sim
                            .connections
                            .iter()
                            .all(|c| c.status == ConnectionStatus::Finished)
                        {
                            let old = item.status;
                            item.status = DownloadStatus::Merging;
                            item.merge_progress_percentage = 0.0;
                            item.current_speed_bytes_per_second = 0.0;
                            events.push(status_event(item, old));
                        }
                    }
                    DownloadStatus::Merging => {
                        item.merge_progress_percentage =
                            (item.merge_progress_percentage + 9.0).min(100.0);
                        events.push(DownloadEvent::Progress(progress_of(
                            item,
                            Some(item.merge_progress_percentage),
                        )));
                        if item.merge_progress_percentage >= 100.0 {
                            let old = item.status;
                            item.status = DownloadStatus::Completed;
                            item.completed_at = Some(DateTimeOffset::now());
                            item.estimated_time_remaining = None;
                            sim.connections.clear();
                            events.push(status_event(item, old));
                            finished_any = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        for e in events {
            self.hub.publish(e);
        }
        if finished_any {
            // A freed slot goes to the next queued download, as the semaphore would.
            let next = self
                .state
                .lock()
                .sims
                .iter()
                .find(|s| s.item.status == DownloadStatus::Queued && s.connecting_until.is_some())
                .map(|s| s.item.id);
            if let Some(id) = next {
                self.begin(id);
            }
            queues.on_download_finished();
        }
    }
}

fn progress_of(item: &DownloadItem, merge: Option<f64>) -> DownloadProgress {
    DownloadProgress {
        download_id: item.id,
        downloaded_bytes: item.downloaded_bytes,
        total_bytes: item.total_bytes,
        speed_bytes_per_second: item.current_speed_bytes_per_second,
        estimated_time_remaining: item.estimated_time_remaining,
        merge_percentage: merge,
    }
}

fn status_event(item: &DownloadItem, old: DownloadStatus) -> DownloadEvent {
    DownloadEvent::StatusChanged(DownloadStatusChanged {
        download_id: item.id,
        old_status: old,
        new_status: item.status,
        error_message: item.error_message.clone(),
    })
}

#[async_trait]
impl DownloadManager for DemoDownloads {
    fn downloads(&self) -> Vec<DownloadItem> {
        self.state
            .lock()
            .sims
            .iter()
            .map(|s| s.item.clone())
            .collect()
    }
    fn get(&self, download_id: Uuid) -> Option<DownloadItem> {
        self.state
            .lock()
            .sims
            .iter()
            .find(|s| s.item.id == download_id)
            .map(|s| s.item.clone())
    }
    fn subscribe(&self) -> EventStream<DownloadEvent> {
        self.hub.subscribe()
    }
    fn connections(&self, download_id: Uuid) -> Vec<ConnectionInfo> {
        self.state
            .lock()
            .sims
            .iter()
            .find(|s| s.item.id == download_id)
            .map(|s| s.connections.clone())
            .unwrap_or_default()
    }

    async fn add_download(&self, request: DownloadRequest) -> Result<DownloadItem> {
        let item = DownloadItem {
            url: request.url.clone(),
            file_name: request.file_name.clone(),
            save_folder: request.save_folder.clone(),
            content_type: request.file_info.content_type.clone(),
            total_bytes: request.file_info.content_length,
            connections_count: request.connections_count,
            supports_resume: request.file_info.supports_range_requests,
            credentials: request.credentials.clone(),
            headers: request.headers.clone().unwrap_or_default(),
            expected_checksum: request.expected_checksum.as_ref().map(|c| c.to_string()),
            stream: request.stream.clone(),
            ..DownloadItem::default()
        };
        self.state.lock().sims.insert(
            0,
            Sim {
                item: item.clone(),
                connections: Vec::new(),
                speeds: Vec::new(),
                connecting_until: None,
            },
        );
        self.hub.publish(DownloadEvent::ListChanged {
            change: DownloadListChangeType::Added,
            item: Box::new(item.clone()),
        });
        if request.start_immediately {
            self.begin(item.id);
        }
        Ok(item)
    }

    async fn start(&self, download_id: Uuid) -> Result<()> {
        self.begin(download_id);
        Ok(())
    }

    fn pause(&self, download_id: Uuid) {
        let changed = self.with_sim(download_id, |sim, _| {
            let old = sim.item.status;
            if !matches!(
                old,
                DownloadStatus::Connecting | DownloadStatus::Downloading | DownloadStatus::Queued
            ) {
                return None;
            }
            sim.item.status = DownloadStatus::Paused;
            sim.item.current_speed_bytes_per_second = 0.0;
            sim.item.estimated_time_remaining = None;
            sim.connecting_until = None;
            for c in &mut sim.connections {
                if c.status != ConnectionStatus::Finished {
                    c.status = ConnectionStatus::Paused;
                }
            }
            Some((sim.item.clone(), old, Arc::new(sim.connections.clone())))
        });
        if let Some(Some((item, old, conns))) = changed {
            self.hub
                .publish(DownloadEvent::Connections(ConnectionsSnapshot {
                    download_id,
                    connections: conns,
                }));
            self.publish_status(&item, old);
        }
    }

    fn pause_all(&self) -> usize {
        let ids: Vec<Uuid> = self
            .state
            .lock()
            .sims
            .iter()
            .filter(|s| s.item.status.is_active())
            .map(|s| s.item.id)
            .collect();
        for id in &ids {
            self.pause(*id);
        }
        ids.len()
    }

    async fn resume(&self, download_id: Uuid) -> Result<()> {
        self.begin(download_id);
        Ok(())
    }

    fn stop(&self, download_id: Uuid) {
        let changed = self.with_sim(download_id, |sim, _| {
            let old = sim.item.status;
            sim.item.status = DownloadStatus::Cancelled;
            sim.item.downloaded_bytes = 0;
            sim.item.current_speed_bytes_per_second = 0.0;
            sim.connections.clear();
            (sim.item.clone(), old)
        });
        if let Some((item, old)) = changed {
            self.hub
                .publish(DownloadEvent::Progress(progress_of(&item, None)));
            self.publish_status(&item, old);
        }
    }

    async fn retry(&self, download_id: Uuid) -> Result<()> {
        self.with_sim(download_id, |sim, _| {
            sim.item.downloaded_bytes = 0;
            sim.connections.clear();
        });
        if let Some(item) = self.get(download_id) {
            self.hub
                .publish(DownloadEvent::Progress(progress_of(&item, None)));
        }
        self.begin(download_id);
        Ok(())
    }

    fn remove(&self, download_id: Uuid, _delete_file: bool) {
        let removed = {
            let mut st = self.state.lock();
            let pos = st.sims.iter().position(|s| s.item.id == download_id);
            pos.map(|p| st.sims.remove(p).item)
        };
        if let Some(item) = removed {
            self.hub.publish(DownloadEvent::ListChanged {
                change: DownloadListChangeType::Removed,
                item: Box::new(item),
            });
        }
    }

    fn change_address(&self, download_id: Uuid, change: AddressChange) -> bool {
        // As the real manager: refused while it runs; clears the stale error.
        let item = self.with_sim(download_id, |sim, _| {
            if sim.item.status.is_active() {
                return None;
            }
            sim.item.url = change.url.clone();
            sim.item.error_message = None;
            if change.discard_progress {
                sim.item.downloaded_bytes = 0;
                sim.connections.clear();
            }
            Some(sim.item.clone())
        });
        if let Some(Some(item)) = item {
            self.hub.publish(DownloadEvent::ItemUpdated(Box::new(item)));
            true
        } else {
            false
        }
    }

    fn control_connection(
        &self,
        download_id: Uuid,
        connection_id: i32,
        command: ConnectionCommand,
    ) -> bool {
        let snapshot = self.with_sim(download_id, |sim, _| {
            let c = sim
                .connections
                .iter_mut()
                .find(|c| c.connection_id == connection_id)?;
            match command {
                ConnectionCommand::Pause => c.status = ConnectionStatus::Paused,
                ConnectionCommand::Resume => c.status = ConnectionStatus::ReceivingData,
                ConnectionCommand::Restart => {
                    c.bytes_downloaded = 0;
                    c.status = ConnectionStatus::ReceivingData;
                }
            }
            Some(Arc::new(sim.connections.clone()))
        });
        match snapshot {
            Some(Some(connections)) => {
                self.hub
                    .publish(DownloadEvent::Connections(ConnectionsSnapshot {
                        download_id,
                        connections,
                    }));
                true
            }
            _ => false,
        }
    }

    /// Reads the file in ~2.5 s with progress; a hash starting `0000` mismatches.
    async fn verify_checksum(
        &self,
        download_id: Uuid,
        expected: ExpectedChecksum,
        progress: Option<ProgressFn<i64>>,
        cancel: CancellationToken,
    ) -> Result<ChecksumResult> {
        let total = self.get(download_id).map_or(0, |i| i.total_bytes).max(1);
        for step in 1..=25 {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                _ = cancel.cancelled() => return Err(CoreError::Cancelled),
            }
            if let Some(report) = &progress {
                report(total * step / 25);
            }
        }
        let result = if expected.hex.starts_with("0000") {
            ChecksumResult::Mismatch
        } else {
            ChecksumResult::Match
        };
        let item = self.with_sim(download_id, |sim, _| {
            sim.item.expected_checksum = Some(expected.to_string());
            sim.item.checksum_result = result;
            sim.item.clone()
        });
        if let Some(item) = item {
            self.hub.publish(DownloadEvent::ItemUpdated(Box::new(item)));
        }
        Ok(result)
    }

    fn global_speed_limit(&self) -> i64 {
        self.global_limit.load(Ordering::Relaxed)
    }

    fn set_speed_limit(&self, download_id: Uuid, bytes_per_second: i64) {
        if let Some(item) = self.with_sim(download_id, |sim, _| {
            sim.item.speed_limit_bytes_per_second = bytes_per_second.max(0);
            sim.item.clone()
        }) {
            self.hub.publish(DownloadEvent::ItemUpdated(Box::new(item)));
        }
    }

    fn set_completion_actions(
        &self,
        download_id: Uuid,
        actions: CompletionActions,
        power: PowerAction,
    ) {
        if let Some(item) = self.with_sim(download_id, |sim, _| {
            sim.item.on_completion = actions;
            sim.item.power_action_when_done = power;
            sim.item.clone()
        }) {
            self.hub.publish(DownloadEvent::ItemUpdated(Box::new(item)));
        }
    }

    fn set_queue_speed_limit(&self, _download_id: Uuid, _bytes_per_second: i64) {}

    fn set_global_speed_limit(&self, bytes_per_second: i64) {
        self.global_limit
            .store(bytes_per_second.max(0), Ordering::Relaxed);
    }

    fn load_persisted_downloads(&self) {}

    async fn cleanup_orphaned_temp_folders(&self) -> usize {
        0
    }

    fn flush(&self) {}
}

// --------------------------------------------------------------------------
// Queues
// --------------------------------------------------------------------------

struct QueuesState {
    queues: Vec<DownloadQueue>,
    running: Vec<Uuid>,
}

pub struct DemoQueues {
    state: Mutex<QueuesState>,
    hub: EventHub<QueueEvent>,
    downloads: Arc<DemoDownloads>,
}

impl DemoQueues {
    fn new(dispatcher: Arc<dyn Dispatcher>, downloads: Arc<DemoDownloads>) -> Self {
        Self {
            state: Mutex::new(QueuesState {
                queues: Vec::new(),
                running: Vec::new(),
            }),
            hub: EventHub::new(Some(dispatcher)),
            downloads,
        }
    }

    fn seed(&self, items: Vec<Uuid>) {
        let mut queue = DownloadQueue {
            name: "Overnight".into(),
            concurrent_downloads: 2,
            item_ids: items,
            ..DownloadQueue::default()
        };
        queue.schedule.enabled = true;
        queue.schedule.days = ScheduleDays::EVERY_DAY;
        queue.schedule.start_time = TimeSpan::from_minutes(90);
        queue.schedule.stop_at_enabled = true;
        queue.schedule.wake_computer = true;
        self.state.lock().queues.push(queue);
    }

    fn changed(&self, change: QueueChangeType, queue: Option<DownloadQueue>) {
        self.hub.publish(QueueEvent::QueuesChanged {
            change,
            queue: queue.map(Box::new),
        });
    }

    fn on_download_finished(&self) {
        let drained: Vec<Uuid> = {
            let st = self.state.lock();
            st.running
                .iter()
                .copied()
                .filter(|q| {
                    st.queues.iter().find(|x| x.id == *q).is_some_and(|queue| {
                        queue.item_ids.iter().all(|id| {
                            self.downloads
                                .get(*id)
                                .is_none_or(|i| i.status == DownloadStatus::Completed)
                        })
                    })
                })
                .collect()
        };
        for q in drained {
            self.state.lock().running.retain(|x| *x != q);
            self.hub.publish(QueueEvent::StateChanged {
                queue_id: q,
                state: QueueState::Idle,
                drained: true,
            });
        }
    }
}

impl QueueManager for DemoQueues {
    fn queues(&self) -> Vec<DownloadQueue> {
        self.state.lock().queues.clone()
    }
    fn subscribe(&self) -> EventStream<QueueEvent> {
        self.hub.subscribe()
    }
    fn load(&self) {}
    fn create(&self, name: &str) -> DownloadQueue {
        let queue = DownloadQueue {
            name: name.chars().take(DownloadQueue::MAX_NAME_LENGTH).collect(),
            ..DownloadQueue::default()
        };
        self.state.lock().queues.push(queue.clone());
        self.changed(QueueChangeType::Added, Some(queue.clone()));
        queue
    }
    fn update(&self, edited: DownloadQueue) {
        let mut st = self.state.lock();
        if let Some(q) = st.queues.iter_mut().find(|q| q.id == edited.id) {
            *q = edited.clone();
            drop(st);
            self.changed(QueueChangeType::Updated, Some(edited));
        }
    }
    fn delete(&self, queue_id: Uuid) {
        self.state.lock().queues.retain(|q| q.id != queue_id);
        self.changed(QueueChangeType::Removed, None);
    }
    fn find(&self, queue_id: Uuid) -> Option<DownloadQueue> {
        self.state
            .lock()
            .queues
            .iter()
            .find(|q| q.id == queue_id)
            .cloned()
    }
    fn queue_of(&self, download_id: Uuid) -> Option<DownloadQueue> {
        self.state
            .lock()
            .queues
            .iter()
            .find(|q| q.item_ids.contains(&download_id))
            .cloned()
    }
    fn queue_id_of(&self, download_id: Uuid) -> Option<Uuid> {
        self.queue_of(download_id).map(|q| q.id)
    }
    fn add_to_queue(&self, queue_id: Uuid, download_ids: &[Uuid]) {
        {
            let mut st = self.state.lock();
            for q in st.queues.iter_mut() {
                q.item_ids.retain(|id| !download_ids.contains(id));
            }
            if let Some(q) = st.queues.iter_mut().find(|q| q.id == queue_id) {
                q.item_ids.extend_from_slice(download_ids);
            }
        }
        self.changed(QueueChangeType::MembershipChanged, self.find(queue_id));
    }
    fn remove_from_queues(&self, download_ids: &[Uuid]) {
        for q in self.state.lock().queues.iter_mut() {
            q.item_ids.retain(|id| !download_ids.contains(id));
        }
        self.changed(QueueChangeType::MembershipChanged, None);
    }
    fn move_item(&self, queue_id: Uuid, download_id: Uuid, offset: i32) -> bool {
        let moved = {
            let mut st = self.state.lock();
            let Some(q) = st.queues.iter_mut().find(|q| q.id == queue_id) else {
                return false;
            };
            let Some(pos) = q.item_ids.iter().position(|id| *id == download_id) else {
                return false;
            };
            let to = pos as i64 + offset as i64;
            if to < 0 || to >= q.item_ids.len() as i64 {
                return false;
            }
            let id = q.item_ids.remove(pos);
            q.item_ids.insert(to as usize, id);
            q.clone()
        };
        self.changed(QueueChangeType::MembershipChanged, Some(moved));
        true
    }
    fn start(&self, queue_id: Uuid) {
        let Some(queue) = self.find(queue_id) else {
            return;
        };
        self.state.lock().running.push(queue_id);
        self.hub.publish(QueueEvent::StateChanged {
            queue_id,
            state: QueueState::Running,
            drained: false,
        });
        for id in queue
            .item_ids
            .iter()
            .take(queue.clamp_concurrency() as usize)
        {
            self.downloads.begin(*id);
        }
    }
    fn start_from_scheduler(&self, queue_id: Uuid) {
        self.start(queue_id);
    }
    fn stop(&self, queue_id: Uuid) {
        let was_running = {
            let mut st = self.state.lock();
            let before = st.running.len();
            st.running.retain(|q| *q != queue_id);
            before != st.running.len()
        };
        if was_running {
            self.hub.publish(QueueEvent::StateChanged {
                queue_id,
                state: QueueState::Idle,
                drained: false,
            });
        }
    }
    fn state_of(&self, queue_id: Uuid) -> QueueState {
        if self.state.lock().running.contains(&queue_id) {
            QueueState::Running
        } else {
            QueueState::Idle
        }
    }
    fn next_run_at(&self, queue_id: Uuid) -> Option<chrono::NaiveDateTime> {
        self.find(queue_id)
            .and_then(|q| q.next_run_at(chrono::Local::now().naive_local()))
    }
    fn has_scheduled_queues(&self) -> bool {
        self.state.lock().queues.iter().any(|q| q.schedule.enabled)
    }
    fn shutdown(&self) {}
}

// --------------------------------------------------------------------------
// Probing, streams, updates, browser bridge
// --------------------------------------------------------------------------

/// A streaming video's playlist: the Add window grows a Quality row.
pub const HLS_URL: &str = "https://media.example.org/talks/keynote-2026/master.m3u8";
/// A live stream: refused with the resolver's message.
pub const LIVE_URL: &str = "https://media.example.org/live/channel-4/index.m3u8";
/// A server that wants a login (any user name with the password `demo` signs in).
pub const AUTH_URL: &str = "https://private.example.com/reports/q3-figures.xlsx";
pub const AUTH_MESSAGE: &str = "private.example.com wants a user name and password (HTTP 401).";
/// What Batch Download's `--open batch` starts with: a pattern and two plain links.
pub const BATCH_LINKS: &str = "https://photos.example.com/trip/IMG_[0001-0006].jpg\n\
https://downloads.example.org/tools/portable-editor.zip\n\
https://private.example.com/reports/q3-figures.xlsx\n\
https://mirror.example.net/broken/missing-file.iso";

/// The release `--open update` offers.
pub fn update_manifest() -> UpdateManifest {
    UpdateManifest {
        version: "1.28.0".into(),
        download_url: "https://quickbyte-app.pages.dev/releases/QuickByte-1.28.0-Setup.exe".into(),
        release_notes: Some(
            "• Batch Download checks six links at a time and files them into a new queue.\n\
             • Refresh Download Address catches the link when you click it again in the browser.\n\
             • Options > Connection: a custom User-Agent for servers that refuse downloaders.\n\
             • Faster start-up and lower memory use.\n\
             • Fixes a rare stall when a server closed a connection mid-segment."
                .into(),
        ),
        release_date: Some(DateTimeOffset(
            chrono::Local::now().fixed_offset() - chrono::TimeDelta::days(3),
        )),
        file_size_bytes: 9 * MB + 412 * 1024,
        sha256: None,
    }
}

pub struct DemoFileInfo;

#[async_trait]
impl RemoteFileInfoProvider for DemoFileInfo {
    /// Answers after ~0.45 s. `private.` hosts want a login (password `demo`),
    /// `broken/` paths are a 404, `.m3u8`/`.mpd` are playlists, `.jpg` files are
    /// small, and everything else is a 214 MB resumable file.
    async fn get_file_info(
        &self,
        url: &str,
        options: &RequestOptions,
        cancel: &CancellationToken,
    ) -> Result<RemoteFileInfo> {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(450)) => {}
            _ = cancel.cancelled() => return Err(CoreError::Cancelled),
        }
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        if host.starts_with("private.") {
            match options.credentials.as_ref() {
                None => return Err(CoreError::auth(AUTH_MESSAGE, false)),
                Some(c) if c.password != "demo" => {
                    return Err(CoreError::auth(
                        "private.example.com rejected that user name or password (HTTP 401).",
                        true,
                    ));
                }
                Some(_) => {}
            }
        }
        if url.contains("/broken/") {
            return Err(CoreError::http(
                Some(404),
                "The server says the file does not exist (HTTP 404 Not Found).",
            ));
        }
        let name = crate::actions::file_name_from_url(url);
        let lower = url.to_ascii_lowercase();
        let (content_type, length, ranges) = if lower.contains(".m3u8") {
            ("application/vnd.apple.mpegurl", 1_204, false)
        } else if lower.contains(".mpd") {
            ("application/dash+xml", 3_870, false)
        } else if lower.ends_with(".jpg") {
            ("image/jpeg", 3 * MB + (name.len() as i64 * 77_123), true)
        } else if lower.ends_with(".xlsx") {
            (
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                2 * MB,
                false,
            )
        } else {
            ("application/octet-stream", 214 * MB, true)
        };
        Ok(RemoteFileInfo {
            file_name: name,
            content_length: length,
            content_type: content_type.into(),
            supports_range_requests: ranges,
            server_claims_range_support: ranges,
            final_url: url.to_string(),
            ..RemoteFileInfo::default()
        })
    }
}

pub struct DemoStreams;

fn hls_choice(label: &str, bandwidth: i64, playlist: &str, audio: Option<&str>) -> StreamChoice {
    let mut source = StreamSource {
        format: StreamFormat::Hls,
        main_playlist_url: Some(playlist.into()),
        audio_playlist_url: audio.map(str::to_string),
        audio_extension: audio.map(|_| ".m4a".to_string()),
        quality: label.into(),
        ..StreamSource::default()
    };
    source.refresh_derived();
    StreamChoice {
        label: label.into(),
        bandwidth,
        source,
        extension: ".ts".into(),
        content_type: "video/mp2t".into(),
    }
}

#[async_trait]
impl StreamResolver for DemoStreams {
    /// Four qualities, best first, for any playlist; a `live/` one is refused.
    async fn describe(
        &self,
        url: &str,
        _options: &RequestOptions,
        cancel: &CancellationToken,
    ) -> Result<StreamDescription> {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(350)) => {}
            _ = cancel.cancelled() => return Err(CoreError::Cancelled),
        }
        if url.contains("/live/") {
            return Err(CoreError::StreamNotSupported(
                quickbyte_core::streaming::stream_plan::LIVE_MESSAGE.into(),
            ));
        }
        let base = url.rsplit_once('/').map_or(url, |(b, _)| b);
        let mut audio_only = hls_choice(
            "Sound only · 128 kbps",
            128_000,
            &format!("{base}/audio.m3u8"),
            None,
        );
        audio_only.extension = ".aac".into();
        audio_only.content_type = "audio/aac".into();
        Ok(StreamDescription {
            format: StreamFormat::Hls,
            choices: vec![
                hls_choice(
                    "1080p · 5.2 Mbps",
                    5_200_000,
                    &format!("{base}/1080p.m3u8"),
                    None,
                ),
                hls_choice(
                    "720p · 2.8 Mbps",
                    2_800_000,
                    &format!("{base}/720p.m3u8"),
                    None,
                ),
                hls_choice(
                    "480p · 1.2 Mbps",
                    1_200_000,
                    &format!("{base}/480p.m3u8"),
                    None,
                ),
                audio_only,
            ],
        })
    }
}

/// The startup check finds nothing (so every demo run does not open with an
/// update window); Help > Check for Updates — the second check — finds
/// [`update_manifest`]. Downloading it takes ~4 s with progress, and
/// "running" setup then fails, as the demo starts nothing.
#[derive(Default)]
pub struct DemoUpdates {
    checks: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl UpdateService for DemoUpdates {
    fn manifest_url(&self) -> &str {
        "https://quickbyte-app.pages.dev/releases/latest.json"
    }
    async fn check_for_update(
        &self,
        current_version: &str,
        _cancel: &CancellationToken,
    ) -> Result<UpdateCheckResult> {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if self.checks.fetch_add(1, Ordering::Relaxed) == 0 {
            return Ok(UpdateCheckResult::up_to_date(current_version, None));
        }
        Ok(UpdateCheckResult::available(
            current_version,
            update_manifest(),
        ))
    }
    async fn download_installer(
        &self,
        manifest: &UpdateManifest,
        progress: Option<ProgressFn<UpdateDownloadProgress>>,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let total = manifest.file_size_bytes.max(MB);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(600)) => {}
            _ = cancel.cancelled() => return Err(CoreError::Cancelled),
        }
        for step in 1..=40 {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                _ = cancel.cancelled() => return Err(CoreError::Cancelled),
            }
            if let Some(report) = &progress {
                report(UpdateDownloadProgress {
                    bytes_received: total * step / 40,
                    total_bytes: total,
                    speed_bytes_per_second: 2.4 * MB as f64,
                });
            }
        }
        Ok(r"C:\demo\QuickByte-Setup.exe".into())
    }
    async fn cleanup_downloaded_installers(&self) -> usize {
        0
    }
}

pub struct DemoBrowser {
    hub: EventHub<BrowserEvent>,
    token: Mutex<String>,
}

impl DemoBrowser {
    fn new(dispatcher: Arc<dyn Dispatcher>) -> Self {
        Self {
            hub: EventHub::new(Some(dispatcher)),
            token: Mutex::new("q7Rk2mVx9pLw4TzN8bYc3HdF".into()),
        }
    }
}

impl BrowserIntegrationService for DemoBrowser {
    fn subscribe(&self) -> EventStream<BrowserEvent> {
        self.hub.subscribe()
    }
    fn is_running(&self) -> bool {
        false
    }
    fn port(&self) -> u16 {
        DownloadSettings::DEFAULT_BROWSER_INTEGRATION_PORT as u16
    }
    fn token(&self) -> String {
        self.token.lock().clone()
    }
    fn last_error(&self) -> Option<String> {
        Some("the demo does not open a socket.".into())
    }
    fn start(&self) {}
    fn stop(&self) {}
    fn regenerate_token(&self) -> String {
        // Deterministic but visibly new.
        let mut token = self.token.lock();
        *token = token.chars().rev().collect();
        token.clone()
    }
}

// --------------------------------------------------------------------------
// VirusTotal
// --------------------------------------------------------------------------

/// Sends nothing anywhere. A key "checks out" when it has a key's shape, and an
/// answer to a question is carried out on the demo's own downloads.
pub struct DemoVirusTotal {
    hub: EventHub<VirusTotalEvent>,
    downloads: Arc<dyn DownloadManager>,
}

impl DemoVirusTotal {
    fn new(dispatcher: Arc<dyn Dispatcher>, downloads: Arc<dyn DownloadManager>) -> Self {
        Self {
            hub: EventHub::new(Some(dispatcher)),
            downloads,
        }
    }
}

#[async_trait]
impl VirusTotalService for DemoVirusTotal {
    fn subscribe(&self) -> EventStream<VirusTotalEvent> {
        self.hub.subscribe()
    }
    fn start(&self) {}
    fn resolve(&self, download_id: Uuid, keep: bool) {
        if !keep {
            self.downloads.remove(download_id, false);
        }
    }
    async fn check_api_key(&self, api_key: &str, _cancel: &CancellationToken) -> Result<()> {
        tokio::time::sleep(Duration::from_millis(600)).await;
        if quickbyte_core::helpers::virus_total::looks_like_api_key(api_key) {
            Ok(())
        } else {
            Err(CoreError::auth(
                quickbyte_core::net::virus_total::KEY_REJECTED,
                true,
            ))
        }
    }
}

/// A flagged report, for `--open virustotal` screenshots.
pub fn flagged_report() -> VirusTotalReport {
    VirusTotalReport {
        verdict: VirusTotalVerdict::Malicious,
        step: VirusTotalStep::Url,
        source: Some(VirusTotalSource::Url),
        malicious: 7,
        suspicious: 2,
        engines: 71,
        permalink: Some("https://www.virustotal.com/gui/home/url".into()),
        ..Default::default()
    }
}
