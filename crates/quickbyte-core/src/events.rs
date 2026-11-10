//! The C# `Events/` folder, plus the observer plumbing that replaces .NET events.
//!
//! Every publisher owns an [`EventHub`]. Subscribers get an [`EventStream`] — an
//! unbounded channel, so publishing never blocks a download task — and a
//! [`Dispatcher`] is told after each publish, so a UI can wake up and drain its
//! streams on its own thread. That pair is the Rust shape of C#'s
//! `event` + `IDispatcher.Post`: events are produced anywhere and consumed, in
//! order, on the thread that owns the windows.

use std::sync::Arc;

use parking_lot::Mutex;
use uuid::Uuid;

use crate::enums::{DownloadStatus, QueueState};
use crate::models::{ConnectionInfo, DownloadItem, DownloadQueue};
use crate::serde_compat::TimeSpan;

/// What is left of `IDispatcher`: "something happened, come and look". The UI's
/// implementation calls `egui::Context::request_repaint`; headless hosts use
/// [`NoopDispatcher`].
pub trait Dispatcher: Send + Sync {
    fn notify(&self);
}

#[derive(Default)]
pub struct NoopDispatcher;

impl Dispatcher for NoopDispatcher {
    fn notify(&self) {}
}

/// The receiving end of a subscription.
pub type EventStream<E> = crossbeam_channel::Receiver<E>;

/// A subscriber that is called inline, on the publishing thread, in publish
/// order — the shape of a C# event handler. For the engine's own layers (the
/// service listening to its pool, the manager re-publishing its services), which
/// must see every event as it happens and must not cost a thread or a timer each.
/// A callback must be quick and must not block.
pub type EventCallback<E> = Arc<dyn Fn(&E) + Send + Sync>;

/// Observer: a set of subscribers, each with its own unbounded queue.
pub struct EventHub<E: Clone + Send + 'static> {
    subscribers: Mutex<Vec<crossbeam_channel::Sender<E>>>,
    /// Copy-on-write, so a publish takes a reference count rather than a copy of the list.
    callbacks: Mutex<Arc<Vec<EventCallback<E>>>>,
    dispatcher: Mutex<Option<Arc<dyn Dispatcher>>>,
}

impl<E: Clone + Send + 'static> Default for EventHub<E> {
    fn default() -> Self {
        Self::new(None)
    }
}

impl<E: Clone + Send + 'static> EventHub<E> {
    pub fn new(dispatcher: Option<Arc<dyn Dispatcher>>) -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            callbacks: Mutex::new(Arc::new(Vec::new())),
            dispatcher: Mutex::new(dispatcher),
        }
    }

    /// Adds an inline subscriber (see [`EventCallback`]). It lives as long as the hub.
    pub fn subscribe_fn(&self, callback: EventCallback<E>) {
        let mut callbacks = self.callbacks.lock();
        let mut next = Vec::with_capacity(callbacks.len() + 1);
        next.extend(callbacks.iter().cloned());
        next.push(callback);
        *callbacks = Arc::new(next);
    }

    pub fn set_dispatcher(&self, dispatcher: Arc<dyn Dispatcher>) {
        *self.dispatcher.lock() = Some(dispatcher);
    }

    pub fn subscribe(&self) -> EventStream<E> {
        let (tx, rx) = crossbeam_channel::unbounded();
        self.subscribers.lock().push(tx);
        rx
    }

    /// Delivers `event` to every live subscriber; drops the ones that went away.
    pub fn publish(&self, event: E) {
        {
            let mut subs = self.subscribers.lock();
            subs.retain(|tx| tx.send(event.clone()).is_ok());
        }
        // Called outside every lock: a callback may well subscribe, or publish on another hub.
        let callbacks = Arc::clone(&self.callbacks.lock());
        for callback in callbacks.iter() {
            callback(&event);
        }
        if let Some(d) = self.dispatcher.lock().as_ref() {
            d.notify();
        }
    }

    pub fn has_subscribers(&self) -> bool {
        !self.subscribers.lock().is_empty() || !self.callbacks.lock().is_empty()
    }
}

/// Aggregated progress for a whole download, raised on a throttled timer.
#[derive(Clone, Debug, PartialEq)]
pub struct DownloadProgress {
    pub download_id: Uuid,
    pub downloaded_bytes: i64,
    pub total_bytes: i64,
    pub speed_bytes_per_second: f64,
    pub estimated_time_remaining: Option<TimeSpan>,
    /// Set only while chunks are concatenated (0–100), on its own channel so the
    /// overall bar can stay at 100% instead of walking backwards.
    pub merge_percentage: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DownloadStatusChanged {
    pub download_id: Uuid,
    pub old_status: DownloadStatus,
    pub new_status: DownloadStatus,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConnectionsSnapshot {
    pub download_id: Uuid,
    pub connections: Arc<Vec<ConnectionInfo>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadListChangeType {
    Added,
    Removed,
}

/// Everything the download side publishes. One enum on one hub keeps the
/// order between kinds (a status change after the last progress tick).
#[derive(Clone, Debug)]
pub enum DownloadEvent {
    ListChanged { change: DownloadListChangeType, item: Box<DownloadItem> },
    Progress(DownloadProgress),
    StatusChanged(DownloadStatusChanged),
    Connections(ConnectionsSnapshot),
    /// Anything else about an item changed (name after a stream resolved, a new
    /// address, completion options, checksum result) — re-read it.
    ItemUpdated(Box<DownloadItem>),
}

impl DownloadEvent {
    pub fn download_id(&self) -> Uuid {
        match self {
            Self::ListChanged { item, .. } | Self::ItemUpdated(item) => item.id,
            Self::Progress(p) => p.download_id,
            Self::StatusChanged(s) => s.download_id,
            Self::Connections(c) => c.download_id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueChangeType {
    Added,
    Removed,
    Updated,
    MembershipChanged,
}

#[derive(Clone, Debug)]
pub enum QueueEvent {
    /// The set of queues, one queue's configuration, or its membership changed.
    QueuesChanged { change: QueueChangeType, queue: Option<Box<DownloadQueue>> },
    /// A queue started or stopped. `drained`: it ran out of downloads to start,
    /// rather than being stopped by the user or its own stop time.
    StateChanged { queue_id: Uuid, state: QueueState, drained: bool },
}
