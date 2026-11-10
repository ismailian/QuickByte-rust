//! Every service contract — the C# `Interfaces/` folder.
//!
//! Async methods use `async_trait` so the contracts can be held as
//! `Arc<dyn Trait>` (the composition root picks implementations; tests pass
//! fakes), exactly as the C# code depended on interfaces rather than classes.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::enums::{CompletionActions, ConnectionCommand, ConnectionStatus, PowerAction, QueueState};
use crate::error::Result;
use crate::events::{DownloadEvent, EventCallback, EventStream, QueueEvent};
use crate::helpers::file_checksum::{ChecksumResult, ExpectedChecksum};
use crate::models::{
    AddressChange, CapturedDownload, ConnectionInfo, DownloadItem, DownloadQueue, DownloadRequest, DownloadSettings,
    RemoteFileInfo, RequestOptions, UpdateCheckResult, UpdateDownloadProgress, UpdateManifest,
};
use crate::streaming::StreamDescription;

/// A progress callback (`IProgress<T>`). Called from worker tasks.
pub type ProgressFn<T> = Arc<dyn Fn(T) + Send + Sync>;

/// The default body of the `subscribe_fn` methods below, for an implementation
/// that only offers channels (a test fake, the UI's demo services): a thread that
/// drains a subscription into the callback, and ends when the publisher goes away.
pub fn forward_to_callback<E: Clone + Send + 'static>(stream: EventStream<E>, callback: EventCallback<E>) {
    let _ = std::thread::Builder::new().name("quickbyte-events".into()).spawn(move || {
        for event in stream {
            callback(&event);
        }
    });
}

// ---------------------------------------------------------------------------
// Bandwidth
// ---------------------------------------------------------------------------

/// A token bucket a connection asks *before* it reads. It takes no more than it
/// was granted and refunds what it did not use.
#[async_trait]
pub trait BandwidthLimiter: Send + Sync {
    /// Waits until some allowance is available; returns how many bytes (1..=desired) may be read.
    async fn request(&self, desired_bytes: usize, cancel: &CancellationToken) -> Result<usize>;
    /// Hands back allowance that was granted but not used.
    fn give_back(&self, unused_bytes: usize);
}

// ---------------------------------------------------------------------------
// Probing
// ---------------------------------------------------------------------------

#[async_trait]
pub trait RemoteFileInfoProvider: Send + Sync {
    async fn get_file_info(&self, url: &str, options: &RequestOptions, cancel: &CancellationToken) -> Result<RemoteFileInfo>;
}

#[async_trait]
pub trait StreamResolver: Send + Sync {
    async fn describe(&self, url: &str, options: &RequestOptions, cancel: &CancellationToken) -> Result<StreamDescription>;
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

/// One byte range into one chunk file. Fully independent of its siblings.
#[async_trait]
pub trait DownloadConnection: Send + Sync {
    fn connection_id(&self) -> i32;
    fn range_start(&self) -> i64;
    fn range_end(&self) -> i64;
    fn bytes_downloaded(&self) -> i64;
    fn retry_count(&self) -> i32;
    fn status(&self) -> ConnectionStatus;
    fn last_error(&self) -> Option<String>;
    fn chunk_file_path(&self) -> &str;

    /// Downloads the range, retrying transient failures internally.
    async fn run(&self, cancel: CancellationToken) -> Result<()>;

    /// For dynamic segmentation: a connection whose end can be pulled in while
    /// it streams. `None` for one that cannot (e.g. a single-connection FTP stream).
    fn as_resizable(&self) -> Option<&dyn ResizableConnection> {
        None
    }

    /// A snapshot for the details window.
    fn info(&self) -> ConnectionInfo {
        ConnectionInfo {
            connection_id: self.connection_id(),
            range_start: self.range_start(),
            range_end: self.range_end(),
            bytes_downloaded: self.bytes_downloaded(),
            status: self.status(),
            retry_count: self.retry_count(),
            last_error: self.last_error(),
        }
    }
}

/// A connection whose range can be shortened while it runs. It reserves each
/// read before making it, so the end can only move beyond everything written
/// or reserved, and no byte ever lands in two chunks.
pub trait ResizableConnection: Send + Sync {
    /// The last byte this connection has written or reserved.
    fn committed_end(&self) -> i64;
    /// Moves the end in to `new_end`; false if that would cut into committed bytes.
    fn try_shrink_end(&self, new_end: i64) -> bool;
}

/// Factory: creates connections without the pool knowing the protocol.
pub struct ConnectionSpec {
    pub connection_id: i32,
    pub url: String,
    pub range_start: i64,
    pub range_end: i64,
    pub already_downloaded: i64,
    pub chunk_file_path: String,
    pub settings: Arc<DownloadSettings>,
    pub bandwidth_limiter: Option<Arc<dyn BandwidthLimiter>>,
    pub options: RequestOptions,
}

pub trait ConnectionFactory: Send + Sync {
    fn create(&self, spec: ConnectionSpec) -> Arc<dyn DownloadConnection>;
}

// ---------------------------------------------------------------------------
// Pool, merger, service, manager
// ---------------------------------------------------------------------------

/// Pool events published by a pool manager.
#[derive(Clone, Debug)]
pub enum PoolEvent {
    Progress(crate::events::DownloadProgress),
    Connections(Vec<ConnectionInfo>),
    ConnectionFailed(String),
}

#[async_trait]
pub trait ConnectionPoolManager: Send + Sync {
    fn subscribe(&self) -> EventStream<PoolEvent>;
    fn snapshot(&self) -> Vec<ConnectionInfo>;
    /// Runs every range to completion. `Ok(true)` when all bytes are down.
    async fn run(&self, item: &DownloadItem, file_info: &RemoteFileInfo, settings: Arc<DownloadSettings>, cancel: CancellationToken) -> Result<bool>;
    fn discard_chunks(&self, temp_folder: &str);
    fn forget_layout(&self, temp_folder: &str);
    fn ordered_chunk_paths(&self) -> Vec<String>;
    fn control_connection(&self, connection_id: i32, command: ConnectionCommand) -> bool;

    /// An inline subscriber, called on the publishing task (the C# event handler
    /// `DownloadService` attaches). Defaults to a thread draining [`Self::subscribe`].
    fn subscribe_fn(&self, callback: EventCallback<PoolEvent>) {
        forward_to_callback(self.subscribe(), callback);
    }
}

#[async_trait]
pub trait FileMerger: Send + Sync {
    async fn merge(
        &self,
        temp_folder: &str,
        ordered_chunk_paths: &[String],
        destination_file_path: &str,
        buffer_size: usize,
        bytes_merged: Option<ProgressFn<i64>>,
        cancel: &CancellationToken,
    ) -> Result<()>;
    fn has_interrupted_merge(&self, temp_folder: &str) -> bool;
    fn discard_interrupted_merge(&self, temp_folder: &str, destination_file_path: &str);
    fn cleanup_chunks(&self, chunk_paths: &[String], temp_folder: &str);
}

/// One download's lifecycle: Queued → Connecting → Downloading → Merging →
/// (Verifying →) Completed, or Paused / Failed / Cancelled.
#[async_trait]
pub trait DownloadService: Send + Sync {
    fn id(&self) -> Uuid;
    /// A snapshot of the item.
    fn item(&self) -> DownloadItem;
    fn subscribe(&self) -> EventStream<DownloadEvent>;
    async fn start(&self) -> Result<()>;
    fn pause(&self);
    async fn resume(&self) -> Result<()>;
    fn stop(&self);
    async fn retry(&self) -> Result<()>;
    fn change_address(&self, change: AddressChange) -> bool;
    fn control_connection(&self, connection_id: i32, command: ConnectionCommand) -> bool;
    fn connections(&self) -> Vec<ConnectionInfo>;
    /// Mutates the item under its lock (speed limit, completion options, …).
    fn update_item(&self, f: &mut dyn FnMut(&mut DownloadItem));
    /// Applies a new per-download / queue limit live.
    fn set_speed_limits(&self, own: i64, queue: Option<i64>);

    /// An inline subscriber (what `DownloadManager` re-publishes through).
    /// Defaults to a thread draining [`Self::subscribe`].
    fn subscribe_fn(&self, callback: EventCallback<DownloadEvent>) {
        forward_to_callback(self.subscribe(), callback);
    }
}

/// The facade every window talks to — the single source of truth.
#[async_trait]
pub trait DownloadManager: Send + Sync {
    /// Snapshots of every download, in list order.
    fn downloads(&self) -> Vec<DownloadItem>;
    fn get(&self, download_id: Uuid) -> Option<DownloadItem>;
    fn subscribe(&self) -> EventStream<DownloadEvent>;
    fn connections(&self, download_id: Uuid) -> Vec<ConnectionInfo>;

    async fn add_download(&self, request: DownloadRequest) -> Result<DownloadItem>;
    async fn start(&self, download_id: Uuid) -> Result<()>;
    fn pause(&self, download_id: Uuid);
    fn pause_all(&self) -> usize;
    async fn resume(&self, download_id: Uuid) -> Result<()>;
    fn stop(&self, download_id: Uuid);
    async fn retry(&self, download_id: Uuid) -> Result<()>;
    fn remove(&self, download_id: Uuid, delete_file: bool);
    fn change_address(&self, download_id: Uuid, change: AddressChange) -> bool;
    fn control_connection(&self, download_id: Uuid, connection_id: i32, command: ConnectionCommand) -> bool;
    async fn verify_checksum(
        &self,
        download_id: Uuid,
        expected: ExpectedChecksum,
        progress: Option<ProgressFn<i64>>,
        cancel: CancellationToken,
    ) -> Result<ChecksumResult>;

    fn global_speed_limit(&self) -> i64;
    fn set_speed_limit(&self, download_id: Uuid, bytes_per_second: i64);
    fn set_completion_actions(&self, download_id: Uuid, actions: CompletionActions, power: PowerAction);
    fn set_queue_speed_limit(&self, download_id: Uuid, bytes_per_second: i64);
    fn set_global_speed_limit(&self, bytes_per_second: i64);

    fn load_persisted_downloads(&self);
    async fn cleanup_orphaned_temp_folders(&self) -> usize;
    /// Flushes pending saves (called on exit).
    fn flush(&self);

    /// An inline subscriber (what `QueueManager` listens to removals through).
    /// Defaults to a thread draining [`Self::subscribe`].
    fn subscribe_fn(&self, callback: EventCallback<DownloadEvent>) {
        forward_to_callback(self.subscribe(), callback);
    }
}

// ---------------------------------------------------------------------------
// Persistence and settings
// ---------------------------------------------------------------------------

pub trait DownloadRepository: Send + Sync {
    fn load_all(&self) -> Vec<DownloadItem>;
    fn save_all(&self, items: &[DownloadItem]);
}

pub trait QueueRepository: Send + Sync {
    fn load_all(&self) -> Vec<DownloadQueue>;
    /// `None` when the file exists but could not be read (half-written by the
    /// other process) — "try again", not "no queues".
    fn try_load_all(&self) -> Option<Vec<DownloadQueue>>;
    fn save_all(&self, queues: &[DownloadQueue]);
}

pub trait SettingsService: Send + Sync {
    fn current(&self) -> Arc<DownloadSettings>;
    fn subscribe(&self) -> EventStream<Arc<DownloadSettings>>;
    fn load(&self);
    fn save(&self, settings: DownloadSettings);
    fn create_copy(&self) -> DownloadSettings {
        (*self.current()).clone()
    }

    /// An inline subscriber (the C# `SettingsChanged` handler `DownloadManager`
    /// keeps the global limit live with). Defaults to a thread draining [`Self::subscribe`].
    fn subscribe_fn(&self, callback: EventCallback<Arc<DownloadSettings>>) {
        forward_to_callback(self.subscribe(), callback);
    }

    // By name (`ISettingsService.Get` / `Save(key, value)` / `SaveMany`; the names
    // and value forms are `helpers::settings_keys`'). Changing one setting this way
    // cannot reset the others, which is the mistake writing a whole object invites.
    // Default bodies so a test's stand-in only has to supply `current` and `save`;
    // `services::SettingsService` overrides `save_many` to do it under its write lock.

    /// Every setting, by name.
    fn get_all(&self) -> crate::helpers::settings_keys::SettingValues {
        crate::helpers::settings_keys::read_all(&self.current())
    }
    /// One setting's value.
    fn get(&self, key: &str) -> std::result::Result<String, crate::helpers::settings_keys::SettingsKeyError> {
        crate::helpers::settings_keys::read(&self.current(), key)
    }
    /// Changes one setting and persists it, leaving every other one as it is.
    fn save_value(&self, key: &str, value: &str) -> std::result::Result<(), crate::helpers::settings_keys::SettingsKeyError> {
        self.save_many(&[(key, value)])
    }
    /// Changes several settings in one write and one announcement. All or
    /// nothing: if one value is refused, none is saved.
    fn save_many(&self, changes: &[(&str, &str)]) -> std::result::Result<(), crate::helpers::settings_keys::SettingsKeyError> {
        let updated = crate::helpers::settings_keys::with(&self.current(), changes)?;
        self.save(updated);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Queues
// ---------------------------------------------------------------------------

pub trait QueueManager: Send + Sync {
    fn queues(&self) -> Vec<DownloadQueue>;
    fn subscribe(&self) -> EventStream<QueueEvent>;
    fn load(&self);
    fn create(&self, name: &str) -> DownloadQueue;
    fn update(&self, edited: DownloadQueue);
    fn delete(&self, queue_id: Uuid);
    fn find(&self, queue_id: Uuid) -> Option<DownloadQueue>;
    fn queue_of(&self, download_id: Uuid) -> Option<DownloadQueue>;
    fn queue_id_of(&self, download_id: Uuid) -> Option<Uuid>;
    fn add_to_queue(&self, queue_id: Uuid, download_ids: &[Uuid]);
    fn remove_from_queues(&self, download_ids: &[Uuid]);
    fn move_item(&self, queue_id: Uuid, download_id: Uuid, offset: i32) -> bool;
    fn start(&self, queue_id: Uuid);
    fn start_from_scheduler(&self, queue_id: Uuid);
    fn stop(&self, queue_id: Uuid);
    fn state_of(&self, queue_id: Uuid) -> QueueState;
    fn next_run_at(&self, queue_id: Uuid) -> Option<chrono::NaiveDateTime>;
    fn has_scheduled_queues(&self) -> bool;
    /// Stops runners and timers.
    fn shutdown(&self);
}

// ---------------------------------------------------------------------------
// Updates and the browser bridge
// ---------------------------------------------------------------------------

#[async_trait]
pub trait UpdateService: Send + Sync {
    fn manifest_url(&self) -> &str;
    async fn check_for_update(&self, current_version: &str, cancel: &CancellationToken) -> Result<UpdateCheckResult>;
    async fn download_installer(
        &self,
        manifest: &UpdateManifest,
        progress: Option<ProgressFn<UpdateDownloadProgress>>,
        cancel: &CancellationToken,
    ) -> Result<String>;
    async fn cleanup_downloaded_installers(&self) -> usize;
}

#[derive(Clone, Debug)]
pub enum BrowserEvent {
    DownloadCaptured(CapturedDownload),
    StatusChanged,
}

pub trait BrowserIntegrationService: Send + Sync {
    fn subscribe(&self) -> EventStream<BrowserEvent>;
    fn is_running(&self) -> bool;
    fn port(&self) -> u16;
    fn token(&self) -> String;
    fn last_error(&self) -> Option<String>;
    fn start(&self);
    fn stop(&self);
    fn regenerate_token(&self) -> String;
}
