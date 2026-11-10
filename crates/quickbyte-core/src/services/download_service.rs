//! Port of `src/QuickByte.Core/Services/DownloadService.cs`.
//!
//! Orchestrates one download's full lifecycle: Queued → Connecting →
//! Downloading → Merging → (Verifying →) Completed (or Paused / Failed /
//! Cancelled along the way). Delegates the actual byte-fetching to the
//! [`ConnectionPoolManager`] and the final assembly to the [`FileMerger`], and
//! re-publishes their progress under a stable set of events that both the main
//! window and any open download details window subscribe to — that shared source
//! of truth is what keeps every open window in sync.
//!
//! The item lives behind this service's own lock (MIGRATION.md §4.2): the
//! manager and the windows read snapshots, and nothing outside writes it except
//! through [`traits::DownloadService::update_item`].

use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::enums::{ConnectionCommand, DownloadStatus};
use crate::error::{CoreError, Result};
use crate::events::{ConnectionsSnapshot, DownloadEvent, DownloadProgress, DownloadStatusChanged, EventCallback, EventHub, EventStream};
use crate::helpers::bandwidth_limiter::BandwidthLimiter;
use crate::helpers::file_checksum::{self, ChecksumResult, ExpectedChecksum};
use crate::helpers::file_version_check::{self, FileVersionVerdict};
use crate::helpers::{connection_actions, disk_space, download_actions, mark_of_the_web};
use crate::models::{AddressChange, ConnectionInfo, DownloadItem, RemoteFileInfo, header_get, join_path};
use crate::serde_compat::{DateTimeOffset, TimeSpan};
use crate::services::file_merger::AvailableBytesFn;
use crate::traits::{self, ConnectionPoolManager, FileMerger, PoolEvent, ProgressFn, RemoteFileInfoProvider, SettingsService};

/// How a service hands its item to whoever persists the list — called on every
/// status change (the C# `Action<DownloadItem> persist`).
pub type PersistFn = Arc<dyn Fn(&DownloadItem) + Send + Sync>;

const TEMP_DELETE_ATTEMPTS: u32 = 10;
const TEMP_DELETE_RETRY_DELAY: Duration = Duration::from_millis(250);

/// How many times one run starts over for a file that changed on the server.
const MAX_SERVER_CHANGE_RESTARTS: i32 = 3;

const GENERIC_FAILURE: &str = "One or more connections failed after exhausting retries.";

/// A download's own limiter and its queue's, which the manager composes with the
/// global one; kept here so [`traits::DownloadService::set_speed_limits`] can
/// change them live.
#[derive(Clone)]
pub struct SpeedLimits {
    pub own: Arc<BandwidthLimiter>,
    pub queue: Arc<BandwidthLimiter>,
}

/// The parts of the C# constructor that have defaults.
#[derive(Clone)]
pub struct ServiceOptions {
    /// Whether finished files get the Mark of the Web (see `mark_of_the_web`).
    pub mark_from_internet: bool,
    /// Free space on a volume; `disk_space::available_bytes` when `None`.
    pub available_bytes: Option<AvailableBytesFn>,
    pub speed_limits: Option<SpeedLimits>,
}

impl Default for ServiceOptions {
    fn default() -> Self {
        Self { mark_from_internet: true, available_bytes: None, speed_limits: None }
    }
}

pub struct DownloadService {
    me: Weak<DownloadService>,
    id: Uuid,
    item: Mutex<DownloadItem>,
    pool: Arc<dyn ConnectionPoolManager>,
    merger: Arc<dyn FileMerger>,
    provider: Arc<dyn RemoteFileInfoProvider>,
    settings: Arc<dyn SettingsService>,
    persist: PersistFn,
    mark_from_internet: bool,
    available_bytes: AvailableBytesFn,
    speed_limits: Option<SpeedLimits>,

    lifecycle: tokio::sync::Mutex<()>,
    cts: Mutex<Option<CancellationToken>>,
    last_file_info: Mutex<Option<RemoteFileInfo>>,
    hub: EventHub<DownloadEvent>,
}

impl DownloadService {
    pub fn new(
        item: DownloadItem,
        pool: Arc<dyn ConnectionPoolManager>,
        merger: Arc<dyn FileMerger>,
        provider: Arc<dyn RemoteFileInfoProvider>,
        settings: Arc<dyn SettingsService>,
        persist: PersistFn,
        options: ServiceOptions,
    ) -> Arc<Self> {
        let service = Arc::new_cyclic(|me: &Weak<DownloadService>| Self {
            me: me.clone(),
            id: item.id,
            item: Mutex::new(item),
            pool,
            merger,
            provider,
            settings,
            persist,
            mark_from_internet: options.mark_from_internet,
            available_bytes: options.available_bytes.unwrap_or_else(|| Arc::new(disk_space::available_bytes)),
            speed_limits: options.speed_limits,
            lifecycle: tokio::sync::Mutex::new(()),
            cts: Mutex::new(None),
            last_file_info: Mutex::new(None),
            hub: EventHub::default(),
        });

        let me = Arc::downgrade(&service);
        service.pool.subscribe_fn(Arc::new(move |event: &PoolEvent| {
            if let Some(service) = me.upgrade() {
                service.on_pool_event(event);
            }
        }));
        service
    }

    fn snapshot(&self) -> DownloadItem {
        self.item.lock().clone()
    }

    fn with_item<R>(&self, f: impl FnOnce(&mut DownloadItem) -> R) -> R {
        f(&mut self.item.lock())
    }

    async fn probe(&self, cancel: &CancellationToken) -> Result<RemoteFileInfo> {
        let (url, options) = self.with_item(|i| (i.url.clone(), i.to_request_options()));
        self.provider.get_file_info(&url, &options, cancel).await
    }

    fn adopt_file_info(&self, info: &RemoteFileInfo) {
        self.with_item(|item| {
            if info.has_known_size() {
                item.total_bytes = info.content_length;
            }
            item.supports_resume = info.supports_range_requests;
            item.content_type = info.content_type.clone();

            // Taken from the fresh probe, before the pool builds its connections:
            // a Retry re-resolves the file, and if a cache in front of the server
            // is what makes ranges work, the connections have to learn that from
            // this run rather than from whatever the item was added with. The
            // probe seeds the flag from the options it was handed, so an item
            // that already knew keeps knowing.
            item.bypass_cache = info.bypass_cache;

            // Whatever the server describes now is the version this run's chunks
            // belong to: either it agreed with the old one, or the old chunks are
            // already gone.
            item.remote_version = Some(info.version());
        });
    }

    fn changed(&self, probed: &RemoteFileInfo) -> bool {
        let (total, known) = self.with_item(|i| (i.total_bytes, i.remote_version.clone()));
        file_version_check::compare(total, known.as_ref(), probed.content_length, Some(&probed.version())) == FileVersionVerdict::Changed
    }

    async fn run_lifecycle(&self, cancel: &CancellationToken) -> Result<()> {
        // Interrupted in the merge — typically by the app exiting half way
        // through a big one. Every byte is already on disk, so this needs no
        // server: probing first would fail a finished download over a link that
        // has since expired. And the pool must not run, because a merge in
        // progress has already folded chunks into the first one.
        let temp = self.with_item(|i| i.temp_folder_path.clone());
        if self.merger.has_interrupted_merge(&temp) {
            return self.merge(Vec::new(), cancel).await;
        }

        let known = self.last_file_info.lock().clone();
        let info = match known {
            Some(info) => info,
            None => {
                let info = self.probe(cancel).await?;
                // Every fresh probe is held against the version the chunks came
                // from. For a download that never ran there are no chunks and it
                // costs nothing; for a resume after a restart, or a Retry, it is
                // the moment the server has had to replace the file.
                if self.changed(&info) {
                    self.discard_progress();
                }
                *self.last_file_info.lock() = Some(info.clone());
                info
            }
        };
        self.adopt_file_info(&info);
        self.run_until_the_file_holds_still(cancel).await
    }

    /// Runs the download, starting over if a connection reports that the file on
    /// the server is no longer the one the chunks came from.
    ///
    /// The report is checked before anything is thrown away. A connection's
    /// `If-Range` getting a 200 is a strong signal, but a server that does not
    /// evaluate the condition, or whose edges mint ETags of their own, sends the
    /// same answer for a file that never changed — and discarding a download on
    /// its word would cost hours for nothing. So the file is probed again: if the
    /// probe describes a different file, the chunks go; if it describes the same
    /// one, it is the condition that cannot be trusted, and the connections stop
    /// sending it.
    async fn run_until_the_file_holds_still(&self, cancel: &CancellationToken) -> Result<()> {
        let mut restarts = 0;
        loop {
            let Some(info) = self.last_file_info.lock().clone() else { return Err(CoreError::other("The file was never probed.")) };
            match self.run_download_cycle(&info, cancel).await {
                Err(CoreError::RemoteFileChanged(_)) => {
                    if cancel.is_cancelled() {
                        return Err(CoreError::Cancelled);
                    }
                    self.set_status(DownloadStatus::Connecting, None);

                    let fresh = self.probe(cancel).await?;
                    if !self.changed(&fresh) {
                        self.with_item(|i| i.if_range_unsupported = true);
                    } else {
                        restarts += 1;
                        if restarts > MAX_SERVER_CHANGE_RESTARTS {
                            // A file regenerated on every request cannot be
                            // downloaded in pieces at all; saying so beats
                            // restarting forever.
                            return Err(CoreError::Io("The file on the server kept changing while it was being downloaded.".into()));
                        }
                        self.discard_progress();
                    }
                    *self.last_file_info.lock() = Some(fresh.clone());
                    self.adopt_file_info(&fresh);
                }
                other => return other,
            }
        }
    }

    /// Throws away every chunk, so the next run starts the file from zero. Used
    /// only once the server is known to hold a different version: bytes from two
    /// versions of a file merge into a file that is neither.
    fn discard_progress(&self) {
        let (temp, full) = self.with_item(|i| (i.temp_folder_path.clone(), i.full_path()));
        self.merger.discard_interrupted_merge(&temp, &full);
        self.pool.discard_chunks(&temp);
        self.with_item(|i| {
            i.downloaded_bytes = 0;
            i.merge_progress_percentage = 0.0;
        });
    }

    async fn run_download_cycle(&self, info: &RemoteFileInfo, cancel: &CancellationToken) -> Result<()> {
        // A pause that landed while the probe was answering has already set the
        // status; announcing Downloading over it would un-pause the row.
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        self.ensure_room_to_finish(info)?;
        self.set_status(DownloadStatus::Downloading, None);

        let item = self.snapshot();
        let completed = self.pool.run(&item, info, self.settings.current(), cancel.clone()).await?;
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }

        if !completed {
            // Prefer what actually went wrong. The pool's ConnectionFailed records
            // the first connection's own message, and the Connecting status
            // cleared the field at the top of this run, so anything in it belongs
            // to this attempt. "404 (Not Found)" is a message a user can act on;
            // the generic line is one they can only report.
            let message = self.with_item(|i| i.error_message.clone()).unwrap_or_else(|| GENERIC_FAILURE.into());
            self.set_status(DownloadStatus::Failed, Some(message));
            return Ok(());
        }

        self.merge(self.pool.ordered_chunk_paths(), cancel).await
    }

    /// Refuses to start a transfer that cannot finish for want of disk. The
    /// chunks fill the temp volume over hours and the merge then needs room at
    /// the destination too, so a download that would run out used to find out at
    /// 100%. The figures were known before the first byte.
    fn ensure_room_to_finish(&self, info: &RemoteFileInfo) -> Result<()> {
        if !info.has_known_size() {
            return Ok(());
        }
        let (temp, folder, connections, downloaded) =
            self.with_item(|i| (i.temp_folder_path.clone(), i.save_folder.clone(), i.connections_count, i.downloaded_bytes));

        // The same count the pool will split with, so the merge estimate matches
        // the chunks that will actually exist.
        let chunks = if info.supports_range_requests {
            (self.settings.current().clamp_connections(connections) as i64).min(info.content_length)
        } else {
            1
        };
        let largest_chunk = (info.content_length + chunks - 1) / chunks;
        let needs = disk_space::still_needed(&temp, &folder, info.content_length, downloaded, largest_chunk, chunks as i32);
        let available = Arc::clone(&self.available_bytes);
        match disk_space::find_shortfall(needs, |p| available(p)) {
            Some(shortfall) => Err(CoreError::InsufficientDiskSpace(shortfall)),
            None => Ok(()),
        }
    }

    /// A progress callback that reports on the merge channel: the bar is pinned
    /// at the total and the percentage travels in `merge_percentage`.
    ///
    /// Inline, on the reporting thread, in order. Merge progress routed back
    /// through `downloaded_bytes` is what used to make the bar visibly rewind and
    /// climb again.
    fn merge_channel(&self, size: impl Fn(&DownloadItem) -> i64 + Send + Sync + 'static) -> ProgressFn<i64> {
        let me = self.me.clone();
        Arc::new(move |done: i64| {
            let Some(service) = me.upgrade() else { return };
            let progress = service.with_item(|item| {
                let size = size(item);
                let percentage = if size > 0 { (done as f64 * 100.0 / size as f64).clamp(0.0, 100.0) } else { 0.0 };
                item.merge_progress_percentage = percentage;
                DownloadProgress {
                    download_id: item.id,
                    downloaded_bytes: item.downloaded_bytes,
                    total_bytes: item.total_bytes,
                    speed_bytes_per_second: 0.0,
                    estimated_time_remaining: Some(TimeSpan(0)),
                    merge_percentage: Some(percentage),
                }
            });
            service.hub.publish(DownloadEvent::Progress(progress));
        })
    }

    /// Assembles the finished file — or finishes assembling it, when
    /// `chunk_paths` is empty and the merger holds an interrupted merge — then
    /// marks it and announces the download complete.
    async fn merge(&self, chunk_paths: Vec<String>, cancel: &CancellationToken) -> Result<()> {
        // Every byte is already on disk before the merge starts, so the overall
        // progress is pinned at 100% for the whole merge.
        self.with_item(|i| {
            if i.total_bytes > 0 {
                i.downloaded_bytes = i.total_bytes;
            }
            i.merge_progress_percentage = 0.0;
        });
        self.set_status(DownloadStatus::Merging, None);

        let (temp, destination) = self.with_item(|i| (i.temp_folder_path.clone(), i.full_path()));
        let progress = self.merge_channel(|item| item.total_bytes);
        self.merger
            .merge(&temp, &chunk_paths, &destination, self.settings.current().clamp_buffer_size(), Some(progress), cancel)
            .await?;

        // The merge deletes each chunk as it lands; this collects any a scanner
        // held open at the time, the pool's record of the layout, and then the
        // folder itself.
        self.pool.forget_layout(&temp);
        self.merger.cleanup_chunks(&chunk_paths, &temp);

        // Before Completed is announced: the completion window offers to open the
        // file, and it has to open as a download from the internet, not as a file
        // SmartScreen and Protected View have never heard of.
        if self.mark_from_internet {
            let (url, referrer) = self.with_item(|i| (i.url.clone(), referrer_of(i)));
            mark_of_the_web::apply(&destination, Some(&url), referrer.as_deref());
        }

        let expected = self.with_item(|i| file_checksum::from_stored(i.expected_checksum.as_deref()));
        if let Some(expected) = expected
            && !self.verify(&destination, &expected, cancel).await?
        {
            return Ok(());
        }

        self.with_item(|i| {
            i.merge_progress_percentage = 100.0;
            i.downloaded_bytes = i.total_bytes;
            i.completed_at = Some(DateTimeOffset::now());
        });
        self.set_status(DownloadStatus::Completed, None);
        Ok(())
    }

    /// Checks the assembled file against the checksum it was added with.
    /// Progress travels on the merge channel — the bar is full, and the text
    /// under it says which step is running. A mismatch fails the download rather
    /// than completing it: a file that is not the one published must not open
    /// from a "Download complete" window, or trigger the power action queued
    /// behind it. The file is left in place for the user to look at.
    ///
    /// Returns whether the download may be announced complete.
    async fn verify(&self, destination: &str, expected: &ExpectedChecksum, cancel: &CancellationToken) -> Result<bool> {
        self.with_item(|i| i.merge_progress_percentage = 0.0);
        self.set_status(DownloadStatus::Verifying, None);

        let size = std::fs::metadata(destination).map(|m| m.len() as i64).unwrap_or(0).max(1);
        let progress = self.merge_channel(move |_| size);
        let result = file_checksum::verify(destination, expected, Some(progress), cancel).await?;
        self.with_item(|i| i.checksum_result = result);
        if result == ChecksumResult::Match {
            return Ok(true);
        }
        self.set_status(
            DownloadStatus::Failed,
            Some(format!(
                "The finished file does not match its {} checksum. It is damaged or not the file that was published.",
                file_checksum::name(expected.algorithm)
            )),
        );
        Ok(false)
    }

    /// Deletes the download's chunk folder, retrying for a couple of seconds.
    /// `stop` cancels and returns immediately, but the connection tasks are
    /// still unwinding at that point and still hold their part files open, so a
    /// delete issued inline loses the race every time — which is how a stopped
    /// download used to leave its chunks occupying disk. Bails out if the
    /// download comes back to life in the meantime (Retry), so a fresh set of
    /// chunks can't be deleted out from under it.
    fn schedule_temp_folder_deletion(&self) {
        let path = self.with_item(|i| i.temp_folder_path.clone());
        if path.is_empty() {
            return;
        }
        let me = self.me.clone();
        schedule_folder_deletion(path, move || me.upgrade().is_some_and(|s| s.with_item(|i| i.status) == DownloadStatus::Cancelled));
    }

    fn on_pool_event(&self, event: &PoolEvent) {
        match event {
            PoolEvent::Progress(progress) => {
                self.with_item(|item| {
                    item.downloaded_bytes = progress.downloaded_bytes;
                    if progress.total_bytes > 0 {
                        item.total_bytes = progress.total_bytes;
                    }
                    item.current_speed_bytes_per_second = progress.speed_bytes_per_second;
                    item.estimated_time_remaining = progress.estimated_time_remaining;
                });
                self.hub.publish(DownloadEvent::Progress(progress.clone()));
            }
            PoolEvent::Connections(connections) => {
                self.hub.publish(DownloadEvent::Connections(ConnectionsSnapshot {
                    download_id: self.id,
                    connections: Arc::new(connections.clone()),
                }));
            }
            PoolEvent::ConnectionFailed(message) => self.with_item(|i| i.error_message = Some(message.clone())),
        }
    }

    fn set_status(&self, new_status: DownloadStatus, error_message: Option<String>) {
        let (old, snapshot) = self.with_item(|item| {
            let old = item.status;
            item.status = new_status;
            item.error_message = error_message.clone();
            (old, item.clone())
        });
        // Outside the item's lock: whoever persists reads every item, this one included.
        (self.persist)(&snapshot);
        self.hub.publish(DownloadEvent::StatusChanged(DownloadStatusChanged {
            download_id: self.id,
            old_status: old,
            new_status,
            error_message,
        }));
    }
}

/// The page the download was captured from, if the browser said. Looked up
/// without regard to case: a list loaded from downloads.json keeps whatever case
/// the header was captured in.
pub(crate) fn referrer_of(item: &DownloadItem) -> Option<String> {
    header_get(&item.headers, "Referer").map(str::to_string)
}

/// The retrying folder delete shared with the stream service: a thread of its
/// own (ten tries a quarter of a second apart, so it ends by itself), asking
/// `still_wanted` before each try.
pub(crate) fn schedule_folder_deletion(path: String, still_wanted: impl Fn() -> bool + Send + 'static) {
    let _ = std::thread::Builder::new().name("quickbyte-cleanup".into()).spawn(move || {
        for _ in 0..TEMP_DELETE_ATTEMPTS {
            if !still_wanted() {
                return;
            }
            if !Path::new(&path).exists() {
                return;
            }
            if std::fs::remove_dir_all(&path).is_ok() {
                return;
            }
            // Best-effort — a connection still has a chunk open.
            std::thread::sleep(TEMP_DELETE_RETRY_DELAY);
        }
        // Still locked after all that: DownloadManager's startup sweep collects
        // it, which is why that sweep treats a cancelled item's folder as
        // unclaimed rather than in use.
    });
}

#[async_trait]
impl traits::DownloadService for DownloadService {
    fn id(&self) -> Uuid {
        self.id
    }

    fn item(&self) -> DownloadItem {
        self.snapshot()
    }

    fn subscribe(&self) -> EventStream<DownloadEvent> {
        self.hub.subscribe()
    }

    fn subscribe_fn(&self, callback: EventCallback<DownloadEvent>) {
        self.hub.subscribe_fn(callback);
    }

    async fn start(&self) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        if matches!(self.with_item(|i| i.status), DownloadStatus::Downloading | DownloadStatus::Connecting) {
            return Ok(());
        }

        // The previous run's token is finished with.
        let cancel = CancellationToken::new();
        *self.cts.lock() = Some(cancel.clone());
        self.set_status(DownloadStatus::Connecting, None);

        let temp_root = self.settings.current().temp_folder.clone();
        self.with_item(|i| {
            if i.temp_folder_path.trim().is_empty() {
                i.temp_folder_path = join_path(&temp_root, &i.id.simple().to_string());
            }
        });

        match self.run_lifecycle(&cancel).await {
            Ok(()) => {}
            // pause()/stop() already set the appropriate status.
            Err(e) if e.is_cancelled() && cancel.is_cancelled() => {}
            Err(e) => self.set_status(DownloadStatus::Failed, Some(e.to_string())),
        }
        Ok(())
    }

    fn pause(&self) {
        if !matches!(self.with_item(|i| i.status), DownloadStatus::Downloading | DownloadStatus::Connecting) {
            return;
        }
        self.set_status(DownloadStatus::Paused, None);
        if let Some(cancel) = self.cts.lock().as_ref() {
            cancel.cancel();
        }
    }

    async fn resume(&self) -> Result<()> {
        if matches!(self.with_item(|i| i.status), DownloadStatus::Downloading | DownloadStatus::Connecting) {
            return Ok(());
        }
        self.start().await
    }

    fn stop(&self) {
        if let Some(cancel) = self.cts.lock().as_ref() {
            cancel.cancel();
        }

        // A finished download has nothing to cancel and no chunks left to
        // discard. Rewriting it as Cancelled is not harmless: DownloadManager
        // calls stop() on its way through remove(), so removing a completed
        // download used to persist it as cancelled and announce that status to
        // every open window a moment before the row disappeared.
        if self.with_item(|i| i.status) == DownloadStatus::Completed {
            return;
        }
        self.set_status(DownloadStatus::Cancelled, None);

        // A merge interrupted on another volume was writing into the user's own
        // folder; discarding the download discards that half-file too.
        let (temp, full) = self.with_item(|i| (i.temp_folder_path.clone(), i.full_path()));
        self.merger.discard_interrupted_merge(&temp, &full);
        self.schedule_temp_folder_deletion();
    }

    async fn retry(&self) -> Result<()> {
        self.with_item(|i| i.error_message = None);
        *self.last_file_info.lock() = None; // re-resolve in case the server-side file changed
        self.start().await
    }

    fn change_address(&self, change: AddressChange) -> bool {
        // Not while a run holds the lifecycle. Its connections are reading the
        // old address, and swapping it underneath them would split one file
        // between two servers mid-transfer.
        let Ok(_lifecycle) = self.lifecycle.try_lock() else { return false };
        if !download_actions::can_refresh_address(&self.snapshot()) {
            return false;
        }

        self.with_item(|i| {
            i.url = change.url.clone();
            if let Some(credentials) = &change.credentials {
                i.credentials = Some(credentials.clone());
            }
            if let Some(headers) = &change.headers {
                i.headers = headers.clone();
            }
        });
        if change.discard_progress {
            self.discard_progress();
        }
        self.adopt_file_info(&change.file_info);

        // A different server has to earn that distrust on its own.
        self.with_item(|i| i.if_range_unsupported = false);

        // The next start probes the new address and holds it to the version
        // adopted just now, like any other resume.
        *self.last_file_info.lock() = None;

        // Same status, fresh event: it clears the error the old address failed
        // with, persists, and tells every open window the address moved.
        let status = self.with_item(|i| i.status);
        self.set_status(status, None);
        true
    }

    fn control_connection(&self, connection_id: i32, command: ConnectionCommand) -> bool {
        connection_actions::accepts_commands(&self.snapshot()) && self.pool.control_connection(connection_id, command)
    }

    fn connections(&self) -> Vec<ConnectionInfo> {
        self.pool.snapshot()
    }

    fn update_item(&self, f: &mut dyn FnMut(&mut DownloadItem)) {
        self.with_item(|i| f(i));
    }

    fn set_speed_limits(&self, own: i64, queue: Option<i64>) {
        if let Some(limits) = &self.speed_limits {
            limits.own.set_bytes_per_second(own);
            if let Some(queue) = queue {
                limits.queue.set_bytes_per_second(queue);
            }
        }
    }
}

#[cfg(test)]
mod tests;
