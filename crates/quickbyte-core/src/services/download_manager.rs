//! Port of `src/QuickByte.Core/Services/DownloadManager.cs`.
//!
//! Composition-root facade: owns every download service, persists the download
//! list, throttles how many downloads run concurrently (queueing the rest), and
//! re-publishes each service's events on one manager-level hub. Every window
//! (main + any number of detail windows) subscribes only to this manager — never
//! to each other — which is what keeps them synchronized with no duplicate
//! polling and no risk of missed updates between windows.
//!
//! **Persistence** (MIGRATION.md §4.5): an immediate save on every status change
//! (each service's persist callback) and on every setting changed here, and a
//! one-second debounced save for everything else — progress no longer writes the
//! whole list on every tick. [`traits::DownloadManager::flush`] writes whatever
//! is pending, on exit.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use tokio::runtime::Handle;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::enums::{CompletionActions, ConnectionCommand, DownloadStatus, PowerAction};
use crate::error::Result;
use crate::events::{
    DownloadEvent, DownloadListChangeType, DownloadStatusChanged, Dispatcher, EventCallback, EventHub, EventStream,
};
use crate::helpers::bandwidth_limiter::{BandwidthLimiter, CompositeBandwidthLimiter};
use crate::helpers::completion_plan;
use crate::helpers::download_actions;
use crate::helpers::file_checksum::{self, ChecksumResult, ExpectedChecksum};
use crate::helpers::file_name_helper::{self, stem_of};
use crate::models::{AddressChange, ConnectionInfo, DownloadItem, DownloadRequest, DownloadSettings, StreamSource, join_path};
use crate::net::http_client::HttpClient;
use crate::services::connection_pool_manager::ConnectionPoolManager;
use crate::services::download_service::{DownloadService, PersistFn, ServiceOptions, SpeedLimits};
use crate::services::file_merger::same_path;
use crate::services::stream_download_service::{StreamDownloadService, StreamServiceOptions};
use crate::traits::{
    self, ConnectionFactory, DownloadRepository, FileMerger, ProgressFn, RemoteFileInfoProvider, SettingsService,
};

/// How long a change that is not a status change waits before the list is written.
const SAVE_DEBOUNCE: Duration = Duration::from_secs(1);

struct Entry {
    service: Arc<dyn traits::DownloadService>,
    limits: SpeedLimits,
    is_stream: bool,
}

pub struct DownloadManager {
    me: Weak<DownloadManager>,
    repository: Arc<dyn DownloadRepository>,
    settings: Arc<dyn SettingsService>,
    file_info_provider: Arc<dyn RemoteFileInfoProvider>,
    connection_factory: Arc<dyn ConnectionFactory>,
    merger: Arc<dyn FileMerger>,
    mark_downloads_from_internet: bool,
    runtime: Handle,
    /// Streaming videos fetch their pieces through the same shared client the
    /// connections use; settable only so a test can hand them a local server's.
    stream_client: Mutex<Arc<HttpClient>>,

    services: RwLock<HashMap<Uuid, Entry>>,
    concurrency_gate: Semaphore,

    // Bandwidth throttling sits here for the same reason the concurrency gate
    // does: it is a budget shared across downloads, and the manager is the only
    // component that can see all of them. One limiter per download, one for its
    // queue, and one for the whole app; each pool gets a composite of the three.
    global_speed_limit: Arc<BandwidthLimiter>,

    /// Held while a new download's file name is chosen and the download is
    /// registered, so two adds cannot both see the same name as free.
    add_lock: Mutex<()>,

    /// Serialises saves, so a later one never writes an older list over a newer one.
    save_lock: Mutex<()>,
    dirty: AtomicBool,
    save_scheduled: AtomicBool,

    hub: EventHub<DownloadEvent>,
}

impl DownloadManager {
    /// `mark_downloads_from_internet`: whether finished files are tagged with the
    /// Mark of the Web — see `mark_of_the_web`. A parameter rather than a
    /// setting: the only reason to turn it off is the Windows policy that turns
    /// it off for the browsers, and reading that is the shell's business.
    ///
    /// `runtime` runs the debounced saves and the temp-folder sweep.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repository: Arc<dyn DownloadRepository>,
        settings: Arc<dyn SettingsService>,
        file_info_provider: Arc<dyn RemoteFileInfoProvider>,
        connection_factory: Arc<dyn ConnectionFactory>,
        merger: Arc<dyn FileMerger>,
        dispatcher: Arc<dyn Dispatcher>,
        mark_downloads_from_internet: bool,
        runtime: Handle,
    ) -> Arc<Self> {
        let current = settings.current();
        let manager = Arc::new_cyclic(|me| Self {
            me: me.clone(),
            repository,
            settings: Arc::clone(&settings),
            file_info_provider,
            connection_factory,
            merger,
            mark_downloads_from_internet,
            runtime,
            stream_client: Mutex::new(HttpClient::shared()),
            services: RwLock::new(HashMap::new()),
            concurrency_gate: Semaphore::new(current.max_concurrent_downloads.max(1) as usize),
            global_speed_limit: Arc::new(BandwidthLimiter::new(current.global_speed_limit_bytes_per_second)),
            add_lock: Mutex::new(()),
            save_lock: Mutex::new(()),
            dirty: AtomicBool::new(false),
            save_scheduled: AtomicBool::new(false),
            hub: EventHub::new(Some(dispatcher)),
        });

        // The one setting that is honoured live rather than snapshotted. A speed
        // limit you have to restart the app to apply is not a speed limit, and the
        // limiter is a mutable object, so keeping it in step costs a single
        // assignment. Everything else in DownloadSettings still takes effect on the
        // next download, by design.
        let global = Arc::clone(&manager.global_speed_limit);
        settings.subscribe_fn(Arc::new(move |s: &Arc<DownloadSettings>| global.set_bytes_per_second(s.global_speed_limit_bytes_per_second)));
        manager
    }

    /// The client stream downloads registered from now on fetch through.
    pub fn set_stream_client(&self, client: Arc<HttpClient>) {
        *self.stream_client.lock() = client;
    }

    /// The service running a download (`GetService`), for a details window that
    /// wants its connection-level events directly.
    pub fn service(&self, download_id: Uuid) -> Option<Arc<dyn traits::DownloadService>> {
        self.services.read().get(&download_id).map(|e| Arc::clone(&e.service))
    }

    /// Whether a download is run by the stream engine rather than the connection pool.
    pub fn is_stream(&self, download_id: Uuid) -> bool {
        self.services.read().get(&download_id).is_some_and(|e| e.is_stream)
    }

    fn all_services(&self) -> Vec<Arc<dyn traits::DownloadService>> {
        self.services.read().values().map(|e| Arc::clone(&e.service)).collect()
    }

    fn items(&self) -> Vec<DownloadItem> {
        self.all_services().iter().map(|s| s.item()).collect()
    }

    fn register(&self, item: DownloadItem) {
        let id = item.id;
        let own = Arc::new(BandwidthLimiter::new(item.speed_limit_bytes_per_second));
        // Starts unlimited and stays that way unless a queue claims the download:
        // an unset limiter short-circuits before taking a lock, so the tier costs
        // nothing for the downloads that are in no queue.
        let queue = Arc::new(BandwidthLimiter::new(0));
        let limits = SpeedLimits { own: Arc::clone(&own), queue: Arc::clone(&queue) };
        let limiter: Arc<dyn traits::BandwidthLimiter> =
            Arc::new(CompositeBandwidthLimiter::new(vec![own, queue, Arc::clone(&self.global_speed_limit) as Arc<dyn traits::BandwidthLimiter>]));

        let me = self.me.clone();
        let persist: PersistFn = Arc::new(move |_item: &DownloadItem| {
            if let Some(manager) = me.upgrade() {
                manager.persist_all();
            }
        });

        let is_stream = item.stream.is_some();
        let service: Arc<dyn traits::DownloadService> = if is_stream {
            StreamDownloadService::new(
                item,
                Arc::clone(&self.stream_client.lock()),
                Arc::clone(&self.merger),
                Arc::clone(&self.settings),
                persist,
                StreamServiceOptions {
                    bandwidth_limiter: Some(limiter),
                    mark_from_internet: self.mark_downloads_from_internet,
                    speed_limits: Some(limits.clone()),
                },
            )
        } else {
            DownloadService::new(
                item,
                Arc::new(ConnectionPoolManager::new(Arc::clone(&self.connection_factory), Some(limiter))),
                Arc::clone(&self.merger),
                Arc::clone(&self.file_info_provider),
                Arc::clone(&self.settings),
                persist,
                ServiceOptions { mark_from_internet: self.mark_downloads_from_internet, available_bytes: None, speed_limits: Some(limits.clone()) },
            )
        };

        let me = self.me.clone();
        service.subscribe_fn(Arc::new(move |event: &DownloadEvent| {
            if let Some(manager) = me.upgrade() {
                if matches!(event, DownloadEvent::Progress(_)) {
                    manager.schedule_save();
                }
                manager.hub.publish(event.clone());
            }
        }));

        self.services.write().insert(id, Entry { service, limits, is_stream });
    }

    /// Writes the whole list now.
    fn persist_all(&self) {
        let _save = self.save_lock.lock();
        self.dirty.store(false, Ordering::SeqCst);
        let items = self.items();
        self.repository.save_all(&items);
    }

    /// Writes the list within [`SAVE_DEBOUNCE`], however many changes arrive before then.
    fn schedule_save(&self) {
        self.dirty.store(true, Ordering::SeqCst);
        if self.save_scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.me.clone();
        self.runtime.spawn(async move {
            tokio::time::sleep(SAVE_DEBOUNCE).await;
            let Some(manager) = me.upgrade() else { return };
            manager.save_scheduled.store(false, Ordering::SeqCst);
            if manager.dirty.load(Ordering::SeqCst) {
                let _ = tokio::task::spawn_blocking(move || manager.persist_all()).await;
            }
        });
    }

    /// Changes an item in place and tells subscribers, so the view model (and
    /// every window drawn from it) shows the change rather than its old copy.
    fn update(&self, download_id: Uuid, mut f: impl FnMut(&mut DownloadItem)) -> bool {
        match self.service(download_id) {
            Some(service) => {
                service.update_item(&mut f);
                self.hub.publish(DownloadEvent::ItemUpdated(Box::new(service.item())));
                true
            }
            None => false,
        }
    }

    /// Whether an unfinished download will write to `path`. A completed one has
    /// already written it, so the file check covers those; a cancelled one never will.
    fn is_reserved_by_a_download(items: &[DownloadItem], path: &str) -> bool {
        items.iter().filter(|i| !matches!(i.status, DownloadStatus::Completed | DownloadStatus::Cancelled)).any(|existing| {
            same_path(&existing.full_path(), path) || existing.audio_full_path().is_some_and(|audio| same_path(&audio, path))
        })
    }

    /// Names a stream's separate sound after its video — "Talk.mp4" and
    /// "Talk.audio.m4a" — under the same rule as the video's own name, so it can
    /// neither replace a file nor be written by two downloads. Called under the
    /// add lock.
    fn name_audio_file(items: &[DownloadItem], mut source: StreamSource, video_path: &str) -> StreamSource {
        if !source.has_separate_audio() {
            source.audio_file_name = None;
            return source;
        }
        let folder = Path::new(video_path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let extension = source.audio_extension.clone().filter(|e| e.len() > 1 && e.starts_with('.')).unwrap_or_else(|| ".m4a".into());
        let video_name = Path::new(video_path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let name = file_name_helper::sanitize_file_name(&format!("{}.audio{extension}", stem_of(&video_name)));
        let audio_path = file_name_helper::get_available_file_path_with(&folder, &name, |p| {
            same_path(p, video_path) || Self::is_reserved_by_a_download(items, p)
        });
        source.audio_file_name = Path::new(&audio_path).file_name().map(|n| n.to_string_lossy().into_owned());
        source
    }

    fn is_temp_folder_in_use(&self, path: &str) -> bool {
        self.items().iter().any(|item| {
            // A cancelled download keeps its temp folder on the item but has
            // already disowned the chunks — Stop means discard, unlike Pause. Its
            // folder only still exists because the delete lost the race with the
            // connections closing, so it is exactly what this sweep is for.
            !item.temp_folder_path.is_empty() && item.status != DownloadStatus::Cancelled && same_path(&item.temp_folder_path, path)
        })
    }

    fn sweep(&self) -> usize {
        let root = self.settings.current().temp_folder.clone();
        if root.trim().is_empty() {
            return 0;
        }
        let Ok(entries) = std::fs::read_dir(&root) else { return 0 };
        let mut deleted = 0;
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            // Only folders this app names. The temp folder may be shared with
            // other programs, so the sweep is deliberately restricted to the
            // Guid("N") shape add_download and DownloadService create — anything
            // else in there belongs to someone else.
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.len() != 32 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let path = entry.path().to_string_lossy().into_owned();
            // Re-checked per folder rather than against one up-front snapshot: a
            // download added while the sweep runs must not lose its chunks.
            if self.is_temp_folder_in_use(&path) {
                continue;
            }
            if std::fs::remove_dir_all(&path).is_ok() {
                deleted += 1;
            }
        }
        deleted
    }
}

#[async_trait]
impl traits::DownloadManager for DownloadManager {
    fn downloads(&self) -> Vec<DownloadItem> {
        let mut items = self.items();
        items.sort_by_key(|i| std::cmp::Reverse(i.created_at.0));
        items
    }

    fn get(&self, download_id: Uuid) -> Option<DownloadItem> {
        self.service(download_id).map(|s| s.item())
    }

    fn subscribe(&self) -> EventStream<DownloadEvent> {
        self.hub.subscribe()
    }

    fn subscribe_fn(&self, callback: EventCallback<DownloadEvent>) {
        self.hub.subscribe_fn(callback);
    }

    fn connections(&self, download_id: Uuid) -> Vec<ConnectionInfo> {
        self.service(download_id).map(|s| s.connections()).unwrap_or_default()
    }

    async fn add_download(&self, request: DownloadRequest) -> Result<DownloadItem> {
        std::fs::create_dir_all(&request.save_folder)?;
        let safe_name = file_name_helper::sanitize_file_name(&request.file_name);
        let settings = self.settings.current();

        let item = {
            let _add = self.add_lock.lock();
            let items = self.items();
            // A file that is not on disk yet can still be spoken for: every
            // unfinished download in the list will write its name when it merges.
            let final_path = file_name_helper::get_available_file_path_with(&request.save_folder, &safe_name, |p| {
                Self::is_reserved_by_a_download(&items, p)
            });
            let stream = request.stream.clone().map(|s| Self::name_audio_file(&items, s, &final_path));
            let is_stream = stream.is_some();

            let item = DownloadItem {
                url: request.url.clone(),
                file_name: Path::new(&final_path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(safe_name),
                save_folder: request.save_folder.clone(),
                content_type: request.file_info.content_type.clone(),
                // A stream's probe described its playlist, not the video: the size
                // is known once the pieces are down, and every piece resumes.
                total_bytes: if is_stream { 0 } else { request.file_info.content_length },
                connections_count: settings.clamp_connections(request.connections_count),
                supports_resume: is_stream || request.file_info.supports_range_requests,
                bypass_cache: !is_stream && request.file_info.bypass_cache,
                remote_version: (!is_stream).then(|| request.file_info.version()),
                expected_checksum: request.expected_checksum.as_ref().map(ToString::to_string),
                stream,
                status: DownloadStatus::Queued,
                temp_folder_path: join_path(&settings.temp_folder, &Uuid::new_v4().simple().to_string()),
                credentials: request.credentials.clone(),
                headers: request.headers.clone().unwrap_or_default(),
                ..Default::default()
            };
            self.register(item.clone());
            item
        };
        self.persist_all();
        self.hub.publish(DownloadEvent::ListChanged { change: DownloadListChangeType::Added, item: Box::new(item.clone()) });

        // A download destined for a queue is added and left alone: the queue
        // decides when it runs, and starting it here would be the one thing the
        // user asked not to happen by choosing a queue at all.
        if request.start_immediately {
            self.start(item.id).await?;
        }
        Ok(self.get(item.id).unwrap_or(item))
    }

    async fn start(&self, download_id: Uuid) -> Result<()> {
        let Some(service) = self.service(download_id) else { return Ok(()) };
        let _permit = self.concurrency_gate.acquire().await.map_err(|_| crate::CoreError::Cancelled)?;
        service.start().await
    }

    fn pause(&self, download_id: Uuid) {
        if let Some(service) = self.service(download_id) {
            service.pause();
        }
    }

    fn pause_all(&self) -> usize {
        let mut paused = 0;
        for service in self.all_services() {
            // Ask the item, not the service: pause() is a no-op for anything that
            // isn't in flight, and the count is what lets a caller (shutdown) know
            // whether it has anything to wait on.
            if !download_actions::can_pause(&service.item()) {
                continue;
            }
            service.pause();
            paused += 1;
        }
        paused
    }

    async fn resume(&self, download_id: Uuid) -> Result<()> {
        self.start(download_id).await
    }

    fn stop(&self, download_id: Uuid) {
        if let Some(service) = self.service(download_id) {
            service.stop();
        }
    }

    async fn retry(&self, download_id: Uuid) -> Result<()> {
        let Some(service) = self.service(download_id) else { return Ok(()) };
        let _permit = self.concurrency_gate.acquire().await.map_err(|_| crate::CoreError::Cancelled)?;
        service.retry().await
    }

    fn remove(&self, download_id: Uuid, delete_file: bool) {
        let Some(entry) = self.services.write().remove(&download_id) else { return };
        entry.service.stop();
        let item = entry.service.item();
        if delete_file {
            // Best-effort.
            let _ = std::fs::remove_file(item.full_path());
            if let Some(audio) = item.audio_full_path() {
                let _ = std::fs::remove_file(audio);
            }
        }
        self.persist_all();
        self.hub.publish(DownloadEvent::ListChanged { change: DownloadListChangeType::Removed, item: Box::new(item) });
    }

    fn change_address(&self, download_id: Uuid, change: AddressChange) -> bool {
        self.service(download_id).is_some_and(|s| s.change_address(change))
    }

    fn control_connection(&self, download_id: Uuid, connection_id: i32, command: ConnectionCommand) -> bool {
        self.service(download_id).is_some_and(|s| s.control_connection(connection_id, command))
    }

    async fn verify_checksum(
        &self,
        download_id: Uuid,
        expected: ExpectedChecksum,
        progress: Option<ProgressFn<i64>>,
        cancel: CancellationToken,
    ) -> Result<ChecksumResult> {
        let Some(item) = self.get(download_id).filter(download_actions::can_verify_checksum) else {
            return Ok(ChecksumResult::NotChecked);
        };
        let result = file_checksum::verify(&item.full_path(), &expected, progress, &cancel).await?;
        self.update(download_id, |i| {
            i.expected_checksum = Some(expected.to_string());
            i.checksum_result = result;
        });
        self.persist_all();

        // Same status, fresh event — the row's description carries the result.
        self.hub.publish(DownloadEvent::StatusChanged(DownloadStatusChanged {
            download_id,
            old_status: item.status,
            new_status: item.status,
            error_message: None,
        }));
        Ok(result)
    }

    fn global_speed_limit(&self) -> i64 {
        self.global_speed_limit.bytes_per_second()
    }

    fn set_speed_limit(&self, download_id: Uuid, bytes_per_second: i64) {
        let bytes_per_second = bytes_per_second.max(0);
        let limiter = self.services.read().get(&download_id).map(|e| Arc::clone(&e.limits.own));
        if let Some(limiter) = limiter {
            limiter.set_bytes_per_second(bytes_per_second);
        }
        if self.update(download_id, |i| i.speed_limit_bytes_per_second = bytes_per_second) {
            self.persist_all();
        }
    }

    fn set_completion_actions(&self, download_id: Uuid, actions: CompletionActions, power: PowerAction) {
        let actions = CompletionActions(actions.0 & (CompletionActions::OPEN_FOLDER.0 | CompletionActions::PLAY_SOUND.0));
        let power = completion_plan::defined(power);
        if self.update(download_id, |i| {
            i.on_completion = actions;
            i.power_action_when_done = power;
        }) {
            self.persist_all();
        }
    }

    fn set_queue_speed_limit(&self, download_id: Uuid, bytes_per_second: i64) {
        let limiter = self.services.read().get(&download_id).map(|e| Arc::clone(&e.limits.queue));
        if let Some(limiter) = limiter {
            limiter.set_bytes_per_second(bytes_per_second.max(0));
        }
    }

    fn set_global_speed_limit(&self, bytes_per_second: i64) {
        self.global_speed_limit.set_bytes_per_second(bytes_per_second);
    }

    fn load_persisted_downloads(&self) {
        for mut item in self.repository.load_all() {
            // Anything that was mid-flight when the app last closed resumes as Paused.
            if matches!(item.status, DownloadStatus::Downloading | DownloadStatus::Connecting | DownloadStatus::Merging) {
                item.status = DownloadStatus::Paused;
            }
            // Interrupted while checking its checksum: the file is already whole
            // and in place, and its chunks are gone, so resuming would fetch it all
            // again. It is complete; the check simply did not finish, and says so.
            if item.status == DownloadStatus::Verifying {
                item.status = DownloadStatus::Completed;
                item.checksum_result = ChecksumResult::NotChecked;
            }
            self.register(item);
        }
    }

    async fn cleanup_orphaned_temp_folders(&self) -> usize {
        let Some(me) = self.me.upgrade() else { return 0 };
        self.runtime.spawn_blocking(move || me.sweep()).await.unwrap_or(0)
    }

    fn flush(&self) {
        self.persist_all();
    }

    fn set_virus_total(&self, download_id: Uuid, report: crate::models::VirusTotalReport) {
        if self.update(download_id, |i| i.virus_total = Some(report.clone())) {
            self.persist_all();
        }
    }
}

impl Drop for DownloadManager {
    fn drop(&mut self) {
        // The list as it stood, should nothing have flushed it.
        if self.dirty.load(Ordering::SeqCst) {
            let items: Vec<DownloadItem> = self.services.read().values().map(|e| e.service.item()).collect();
            self.repository.save_all(&items);
        }
    }
}

#[cfg(test)]
mod tests;
