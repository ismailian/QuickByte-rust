//! The one shared view model, keyed by download id.
//!
//! In C# the windows read the same mutable `DownloadItem` objects the engine
//! wrote. In Rust the engine owns its items and publishes events; the UI keeps
//! its own copies here and updates them from those events, on the UI thread,
//! in order. The main window and every details window read *this*, never a
//! private copy — which is what keeps the C# guarantee that every open window
//! shows identical numbers (README, "Multi-window synchronization").
//!
//! Applying an event returns a [`VmEffect`] when it is something the app must
//! react to beyond repainting (a download added, finished, a queue drained).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use quickbyte_core::enums::{DownloadStatus, QueueState};
use quickbyte_core::events::{DownloadEvent, DownloadListChangeType, QueueEvent};
use quickbyte_core::models::{ConnectionInfo, DownloadItem, DownloadQueue, DownloadSettings};
use uuid::Uuid;

use crate::services::AppServices;
use crate::widgets::animator::Animator;

/// One download as the windows see it.
#[derive(Clone, Debug)]
pub struct DownloadRow {
    pub item: DownloadItem,
    pub connections: Arc<Vec<ConnectionInfo>>,
    /// Set only while chunks are merged (or the file verified), 0–100.
    pub merge_percentage: Option<f64>,
}

/// Something the app reacts to beyond repainting.
#[derive(Clone, Debug, PartialEq)]
pub enum VmEffect {
    Added(Uuid),
    Removed(Uuid),
    StatusChanged {
        id: Uuid,
        old: DownloadStatus,
        new: DownloadStatus,
        error: Option<String>,
    },
    QueuesChanged,
    QueueStateChanged {
        queue_id: Uuid,
        state: QueueState,
        drained: bool,
    },
}

pub struct ViewModel {
    rows: HashMap<Uuid, DownloadRow>,
    /// List order: persisted downloads in the manager's order, new ones on top.
    order: Vec<Uuid>,
    pub queues: Vec<DownloadQueue>,
    queue_of: HashMap<Uuid, Uuid>,
    queue_states: HashMap<Uuid, QueueState>,
    pub settings: Arc<DownloadSettings>,
    /// Displayed (eased) progress per download — shared, so the list and a
    /// details window glide in step.
    pub progress: Animator<Uuid>,
    /// Bumped whenever a row is added/removed or changes status or membership,
    /// so a window can cache what it derives from the list.
    pub generation: u64,
    /// Downloads touched since the last [`ViewModel::take_dirty`].
    dirty: HashSet<Uuid>,
    last_advance: Option<std::time::Instant>,
}

impl ViewModel {
    /// Seeds the model from the services' current snapshots.
    pub fn load(services: &AppServices) -> Self {
        let mut vm = Self {
            rows: HashMap::new(),
            order: Vec::new(),
            queues: Vec::new(),
            queue_of: HashMap::new(),
            queue_states: HashMap::new(),
            settings: services.settings.current(),
            progress: Animator::percentages(),
            generation: 0,
            dirty: HashSet::new(),
            last_advance: None,
        };
        for item in services.downloads.downloads() {
            let id = item.id;
            vm.progress.set_target(id, item.progress_percentage(), true);
            let connections = Arc::new(services.downloads.connections(id));
            vm.rows.insert(
                id,
                DownloadRow {
                    item,
                    connections,
                    merge_percentage: None,
                },
            );
            vm.order.push(id);
        }
        vm.reload_queues(services);
        vm
    }

    pub fn reload_queues(&mut self, services: &AppServices) {
        self.queues = services.queues.queues();
        self.queue_of.clear();
        for q in &self.queues {
            for id in &q.item_ids {
                self.queue_of.insert(*id, q.id);
            }
            self.queue_states
                .insert(q.id, services.queues.state_of(q.id));
        }
        self.generation += 1;
    }

    // ------------------------------------------------------------- reading --

    pub fn get(&self, id: Uuid) -> Option<&DownloadRow> {
        self.rows.get(&id)
    }

    pub fn item(&self, id: Uuid) -> Option<&DownloadItem> {
        self.rows.get(&id).map(|r| &r.item)
    }

    /// Every download, in list order.
    pub fn rows(&self) -> impl Iterator<Item = &DownloadRow> {
        self.order.iter().filter_map(|id| self.rows.get(id))
    }

    pub fn items(&self) -> impl Iterator<Item = &DownloadItem> {
        self.rows().map(|r| &r.item)
    }

    pub fn order(&self) -> &[Uuid] {
        &self.order
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn queue_id_of(&self, download: Uuid) -> Option<Uuid> {
        self.queue_of.get(&download).copied()
    }

    pub fn queue_state(&self, queue: Uuid) -> QueueState {
        self.queue_states.get(&queue).copied().unwrap_or_default()
    }

    /// (downloads, active, complete, total speed of the transferring ones).
    pub fn totals(&self) -> Totals {
        let mut t = Totals::default();
        for item in self.items() {
            t.total += 1;
            if item.status.is_active() {
                t.active += 1;
            }
            if item.status == DownloadStatus::Completed {
                t.completed += 1;
            }
            if item.status == DownloadStatus::Downloading {
                t.speed += item.current_speed_bytes_per_second;
            }
        }
        t
    }

    /// The downloads touched since the last call (to repaint the windows showing them).
    pub fn take_dirty(&mut self) -> HashSet<Uuid> {
        std::mem::take(&mut self.dirty)
    }

    // ------------------------------------------------------------ applying --

    pub fn apply_download_event(
        &mut self,
        event: DownloadEvent,
        services: &AppServices,
    ) -> Option<VmEffect> {
        self.dirty.insert(event.download_id());
        match event {
            DownloadEvent::ListChanged {
                change: DownloadListChangeType::Added,
                item,
            } => {
                let id = item.id;
                if self.rows.contains_key(&id) {
                    return None;
                }
                self.progress
                    .set_target(id, item.progress_percentage(), true);
                self.rows.insert(
                    id,
                    DownloadRow {
                        item: *item,
                        connections: Arc::new(Vec::new()),
                        merge_percentage: None,
                    },
                );
                self.order.insert(0, id);
                self.generation += 1;
                Some(VmEffect::Added(id))
            }
            DownloadEvent::ListChanged {
                change: DownloadListChangeType::Removed,
                item,
            } => {
                let id = item.id;
                self.rows.remove(&id);
                self.order.retain(|x| *x != id);
                self.progress.remove(&id);
                self.generation += 1;
                Some(VmEffect::Removed(id))
            }
            DownloadEvent::Progress(p) => {
                let row = self.rows.get_mut(&p.download_id)?;
                let item = &mut row.item;
                item.downloaded_bytes = p.downloaded_bytes;
                if p.total_bytes > 0 {
                    item.total_bytes = p.total_bytes;
                }
                item.current_speed_bytes_per_second = p.speed_bytes_per_second;
                item.estimated_time_remaining = p.estimated_time_remaining;
                row.merge_percentage = p.merge_percentage;
                if let Some(m) = p.merge_percentage {
                    item.merge_progress_percentage = m;
                }
                let pct = item.progress_percentage();
                self.progress.set_target(p.download_id, pct, false);
                None
            }
            DownloadEvent::StatusChanged(s) => {
                // The engine's snapshot carries what the event does not (the
                // completion time, a refreshed address, the checksum result).
                let fresh = services.downloads.get(s.download_id);
                let row = self.rows.get_mut(&s.download_id)?;
                if let Some(fresh) = fresh {
                    let (speed, eta) = (
                        row.item.current_speed_bytes_per_second,
                        row.item.estimated_time_remaining,
                    );
                    row.item = fresh;
                    if s.new_status.is_active() {
                        row.item.current_speed_bytes_per_second = speed;
                        row.item.estimated_time_remaining = eta;
                    }
                }
                row.item.status = s.new_status;
                if s.error_message.is_some() {
                    row.item.error_message = s.error_message.clone();
                }
                if !matches!(
                    s.new_status,
                    DownloadStatus::Merging | DownloadStatus::Verifying
                ) {
                    row.merge_percentage = None;
                }
                if !s.new_status.is_active() {
                    row.item.current_speed_bytes_per_second = 0.0;
                    row.item.estimated_time_remaining = None;
                }
                let pct = if s.new_status == DownloadStatus::Completed {
                    100.0
                } else {
                    row.item.progress_percentage()
                };
                self.progress.set_target(s.download_id, pct, false);
                self.generation += 1;
                Some(VmEffect::StatusChanged {
                    id: s.download_id,
                    old: s.old_status,
                    new: s.new_status,
                    error: s.error_message,
                })
            }
            DownloadEvent::Connections(c) => {
                let row = self.rows.get_mut(&c.download_id)?;
                row.connections = c.connections;
                None
            }
            DownloadEvent::ItemUpdated(item) => {
                let row = self.rows.get_mut(&item.id)?;
                let status_changed = row.item.status != item.status;
                let (speed, eta) = (
                    row.item.current_speed_bytes_per_second,
                    row.item.estimated_time_remaining,
                );
                row.item = *item;
                if row.item.status.is_active() {
                    row.item.current_speed_bytes_per_second = speed;
                    row.item.estimated_time_remaining = eta;
                }
                if status_changed {
                    self.generation += 1;
                }
                None
            }
        }
    }

    pub fn apply_queue_event(
        &mut self,
        event: QueueEvent,
        services: &AppServices,
    ) -> Option<VmEffect> {
        match event {
            QueueEvent::QueuesChanged { .. } => {
                self.reload_queues(services);
                Some(VmEffect::QueuesChanged)
            }
            QueueEvent::StateChanged {
                queue_id,
                state,
                drained,
            } => {
                self.queue_states.insert(queue_id, state);
                Some(VmEffect::QueueStateChanged {
                    queue_id,
                    state,
                    drained,
                })
            }
        }
    }

    pub fn apply_settings(&mut self, settings: Arc<DownloadSettings>) {
        self.settings = settings;
    }

    /// Moves the eased progress on by however long it has been since the last
    /// call (whichever window draws first does it). Returns whether anything is
    /// still moving.
    pub fn advance_animation(&mut self) -> bool {
        let now = std::time::Instant::now();
        let elapsed = self
            .last_advance
            .map_or(crate::widgets::animator::frame_interval(), |t| now - t);
        self.last_advance = Some(now);
        if !self.progress.is_animating() {
            return false;
        }
        self.progress
            .advance_by(elapsed.min(std::time::Duration::from_millis(100)));
        self.progress.is_animating()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Totals {
    pub total: usize,
    pub active: usize,
    pub completed: usize,
    pub speed: f64,
}
