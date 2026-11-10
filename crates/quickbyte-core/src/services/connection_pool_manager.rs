//! Port of `src/QuickByte.Core/Services/ConnectionPoolManager.cs`.
//!
//! Oversees the full set of connections for one download: splits the byte
//! range (via `range_splitter`), assigns each connection its range and temp
//! chunk path (via [`ConnectionFactory`]), runs them concurrently, and raises
//! throttled, aggregated progress/connection snapshot events on a timer so the
//! UI stays smooth regardless of how many connections are running. Once every
//! connection finishes, control returns to the caller (`DownloadService`) which
//! triggers the merge.
//!
//! **Dynamic segmentation.** A fixed split is only as fast as its slowest
//! segment: when seven connections finish and the eighth crawls, the download
//! crawls with it on one connection. So when a connection finishes, its slot is
//! given to the back half of whichever running range has the most left — that
//! range is shortened (safely, see [`SegmentRange`](crate::net::segment_range::SegmentRange))
//! and a new connection fetches the rest into a chunk of its own. The number
//! running never exceeds the download's connection count, which is all a server
//! that limits connections was ever promised. Ranges under
//! [`MINIMUM_SPLIT_BYTES`] are left alone: a new connection costs a round trip
//! that a small tail does not repay.
//!
//! **The layout is written down.** Once ranges can be split, a chunk's number no
//! longer says which bytes it holds, so [`SegmentMap`] records it in the temp
//! folder — before the new connection writes a byte. Resume reads the map; a
//! download without one is the fixed split and gets one.
//!
//! **Performance.** Connections only bump their own atomics; this pool samples
//! them on its timer (the settings' interval, clamped to 50–2000 ms) and
//! publishes one aggregated event per tick. Nothing is raised per byte.

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::enums::{ConnectionCommand, ConnectionStatus};
use crate::error::{CoreError, Result};
use crate::events::{DownloadProgress, EventCallback, EventHub, EventStream};
use crate::helpers::connection_actions;
use crate::helpers::range_splitter::{self, Range, UNBOUNDED_END};
use crate::helpers::retry_policy;
use crate::helpers::speed_calculator::SpeedCalculator;
use crate::models::{ConnectionInfo, DownloadItem, DownloadSettings, RemoteFileInfo, RequestOptions};
use crate::services::segment_map::{MAX_SEGMENTS, Segment, SegmentMap};
use crate::traits::{self, BandwidthLimiter, ConnectionFactory, ConnectionSpec, DownloadConnection, PoolEvent};

/// Chunk files are `part0.tmp`, `part1.tmp`… — named here because resume also has to recognise them.
const CHUNK_PREFIX: &str = "part";

/// A range must have at least this much left to be worth splitting: each half is then 512 KiB or more.
pub const MINIMUM_SPLIT_BYTES: i64 = 1024 * 1024;

/// How many times one range is fetched before its failure counts: on the
/// connection itself, on a replacement, and on a replacement that waited for a
/// slot another connection gave up. See the scheduler in [`ConnectionPoolManager::run`].
pub const RECOVERY_ATTEMPTS: i32 = 3;

type Connection = Arc<dyn DownloadConnection>;

struct Shared {
    /// Copy-on-write, so the report timer reads it with one reference count.
    connections: Arc<Vec<Connection>>,
    /// What the snapshot reports as Paused: a stopped connection's own status
    /// still says what it was doing.
    paused: HashSet<i32>,
}

pub struct ConnectionPoolManager {
    connection_factory: Arc<dyn ConnectionFactory>,
    /// Handed to every connection this pool builds, so one budget covers the
    /// whole download instead of each segment getting its own. Unlike the rest
    /// of [`DownloadSettings`], which is snapshotted when the pool starts, the
    /// limiter is a live object — changing its rate takes effect on the transfer
    /// already running.
    bandwidth_limiter: Option<Arc<dyn BandwidthLimiter>>,
    speed_calculator: SpeedCalculator,
    sync: Mutex<Shared>,
    download_id: Mutex<Uuid>,
    total_bytes: AtomicI64,

    // Per-connection commands, queued from any thread and carried out by the
    // scheduler loop, which `wake` rouses.
    commands: Mutex<VecDeque<(i32, ConnectionCommand)>>,
    wake: Notify,
    running: AtomicBool,

    /// The wait before a range is fetched again after a failure, doubled each
    /// time. Long enough for a rate limiter's window to move on.
    recovery_base_delay: Duration,
    hub: EventHub<PoolEvent>,
}

/// What a split needs to build a connection like the others. One per run.
struct RunContext {
    url: String,
    temp_folder: String,
    settings: Arc<DownloadSettings>,
    options: RequestOptions,
    map: Option<SegmentMap>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotState {
    Pending,
    Running,
    Paused,
    Failed,
    Finished,
}

/// One range's place in the schedule. Touched only by the scheduler loop.
struct Slot {
    connection: Connection,
    state: SlotState,
    attempt: i32,
    /// The connection's own stop, a child of the run's.
    cancel: Option<CancellationToken>,
    /// A pause or restart asked for while it was running, carried out once it has stopped.
    interrupt: Option<ConnectionCommand>,
}

impl Slot {
    fn new(connection: Connection) -> Self {
        Self { connection, state: SlotState::Pending, attempt: 1, cancel: None, interrupt: None }
    }
}

enum Outcome {
    Done,
    Failed(CoreError),
}

impl ConnectionPoolManager {
    pub fn new(connection_factory: Arc<dyn ConnectionFactory>, bandwidth_limiter: Option<Arc<dyn BandwidthLimiter>>) -> Self {
        Self {
            connection_factory,
            bandwidth_limiter,
            speed_calculator: SpeedCalculator::default(),
            sync: Mutex::new(Shared { connections: Arc::new(Vec::new()), paused: HashSet::new() }),
            download_id: Mutex::new(Uuid::nil()),
            total_bytes: AtomicI64::new(0),
            commands: Mutex::new(VecDeque::new()),
            wake: Notify::new(),
            running: AtomicBool::new(false),
            recovery_base_delay: Duration::from_secs(2),
            hub: EventHub::default(),
        }
    }

    /// For the tests: how long a failed range waits before it is fetched again.
    pub fn with_recovery_base_delay(mut self, delay: Duration) -> Self {
        self.recovery_base_delay = delay;
        self
    }

    fn retry_delay(&self, attempt: i32) -> Duration {
        if attempt <= 1 { Duration::ZERO } else { self.recovery_base_delay.mul_f64(2f64.powi(attempt - 2)) }
    }

    fn connections(&self) -> Arc<Vec<Connection>> {
        Arc::clone(&self.sync.lock().connections)
    }

    fn create(&self, ctx: &RunContext, chunk: i32, range: Range, already: i64, chunk_path: String) -> Connection {
        self.connection_factory.create(ConnectionSpec {
            connection_id: chunk,
            url: ctx.url.clone(),
            range_start: range.start,
            range_end: range.end,
            already_downloaded: already,
            chunk_file_path: chunk_path,
            settings: Arc::clone(&ctx.settings),
            bandwidth_limiter: self.bandwidth_limiter.clone(),
            options: ctx.options.clone(),
        })
    }

    /// Runs the connections at most `concurrency` at a time — a resumed download
    /// may have more unfinished segments than that after earlier splits — and,
    /// when `splitting`, gives each freed slot to the back half of the running
    /// range with the most left. It is also where the user's per-connection
    /// commands (`control_connection`) are carried out, so the slots are only
    /// ever moved by one task.
    ///
    /// Splitting is an optimisation, and must never be the reason a download
    /// fails. Every split opens a new connection and abandons the old one's
    /// socket mid-response, and a server that rate-limits connections answers
    /// that churn with `429 Too Many Requests` — measured, not supposed. So the
    /// first failure of any kind stops splitting for the rest of the run.
    ///
    /// **Recovery.** A connection that has used up its own retries is not the
    /// end of its range, because what dropped it is as often the server's
    /// patience with parallel connections as the network. Its chunk is thrown
    /// away and a replacement fetches the same range from scratch. If the
    /// replacement fails too, the server very likely will not serve this many
    /// connections at once, so the download stops asking it to: the run's
    /// concurrency drops to what is still running, and the range is fetched in
    /// the next slot a finishing connection frees. Only a range that fails all
    /// [`RECOVERY_ATTEMPTS`] times is reported. A failure that a fresh
    /// connection could not fix — a wrong password, a full disk — is reported at
    /// once (`retry_policy::is_transient`).
    ///
    /// **A paused connection keeps its slot.** Pausing one is the user asking
    /// for fewer connections, not for a different one to take its place, and
    /// Resume then starts it straight away instead of behind whatever the slot
    /// was given to.
    #[allow(clippy::too_many_arguments)]
    async fn run_scheduled(
        &self,
        ctx: &mut RunContext,
        connections: Vec<Connection>,
        mut concurrency: usize,
        mut splitting: bool,
        run: &CancellationToken,
        file_changed: &Arc<Mutex<Option<CoreError>>>,
        interval: Duration,
    ) {
        let mut slots: Vec<Slot> = connections.into_iter().map(Slot::new).collect();
        let mut pending: VecDeque<usize> = (0..slots.len()).collect();
        let mut running: FuturesUnordered<BoxFuture<'static, (usize, Outcome)>> = FuturesUnordered::new();

        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // The scheduler's moves, as closures over its own state would fight the
        // borrow checker; they are methods on a small context instead.
        let mut sched = Scheduler { pool: self, run, file_changed };

        sched.fill(ctx, &mut slots, &mut pending, &mut running, concurrency, &mut splitting);
        while !running.is_empty() || (held(&slots) > 0 && !run.is_cancelled()) {
            tokio::select! {
                Some((index, outcome)) = running.next(), if !running.is_empty() => {
                    let slot = &mut slots[index];
                    let stopped_alone = slot.cancel.as_ref().is_some_and(CancellationToken::is_cancelled) && !run.is_cancelled();
                    slot.cancel = None;
                    let interrupt = slot.interrupt.take();
                    let finished = slot.connection.status() == ConnectionStatus::Finished;

                    if interrupt == Some(ConnectionCommand::Restart) {
                        sched.requeue(ctx, &mut slots, &mut pending, index, true, 1);
                    } else if interrupt == Some(ConnectionCommand::Pause) && !finished {
                        sched.hold(&mut slots, index);
                    } else if stopped_alone && !finished {
                        // Its pause was called off while it was being stopped.
                        sched.requeue(ctx, &mut slots, &mut pending, index, false, 1);
                    } else if let Outcome::Failed(error) = outcome {
                        // The server has had enough of new connections, or the
                        // network has; either way, no more of them this run.
                        splitting = false;
                        let attempt = slots[index].attempt;
                        if !run.is_cancelled() && attempt < RECOVERY_ATTEMPTS && retry_policy::is_transient(&error) {
                            // The same range failing a second time, on a connection
                            // of its own: the server will not serve this many at
                            // once, so this one waits for one of the others to end.
                            if attempt >= 2 {
                                concurrency = (running.len() + held(&slots)).max(1);
                            }
                            sched.requeue(ctx, &mut slots, &mut pending, index, true, attempt + 1);
                        } else {
                            slots[index].state = SlotState::Failed;
                            let id = slots[index].connection.connection_id();
                            self.hub.publish(PoolEvent::ConnectionFailed(format!("Connection #{id}: {error}")));
                        }
                    } else {
                        slots[index].state = SlotState::Finished;
                    }
                    sched.fill(ctx, &mut slots, &mut pending, &mut running, concurrency, &mut splitting);
                }
                _ = self.wake.notified() => {
                    loop {
                        let next = self.commands.lock().pop_front();
                        let Some((id, command)) = next else { break };
                        sched.apply(ctx, &mut slots, &mut pending, id, command);
                    }
                    sched.fill(ctx, &mut slots, &mut pending, &mut running, concurrency, &mut splitting);
                }
                // An all-paused download has nothing running to wake the loop, so a
                // pause of the whole download has to.
                _ = run.cancelled(), if !run.is_cancelled() => {}
                _ = ticker.tick() => self.report_progress(),
            }
        }

        self.commands.lock().clear();
    }

    /// Halves the running range with the most left and returns a connection for
    /// its back half, or `None` when nothing is worth splitting. The map is
    /// written before the new connection exists, so a crash at any point leaves
    /// a layout that describes every byte on disk.
    fn try_split<'a>(&self, ctx: &mut RunContext, running: impl Iterator<Item = &'a Connection>) -> Option<Connection> {
        let map = ctx.map.as_ref()?;
        if map.segments.len() >= MAX_SEGMENTS {
            return None;
        }

        let mut candidates: Vec<(&Connection, i64, i64)> = running
            .filter(|c| !matches!(c.status(), ConnectionStatus::Finished | ConnectionStatus::Failed))
            .filter_map(|c| c.as_resizable().map(|r| (c, r.committed_end() + 1, c.range_end())))
            .filter(|&(_, _, end)| end != UNBOUNDED_END)
            .collect();
        candidates.sort_by_key(|&(_, from, end)| std::cmp::Reverse(end - from + 1));

        for (connection, from, end) in candidates {
            let remaining = end - from + 1;
            if remaining < MINIMUM_SPLIT_BYTES {
                return None; // sorted: nothing after is bigger
            }
            let new_end = from + remaining / 2 - 1;

            // Refused when the connection read past the split point in the moment
            // since it was measured; the next largest may still do.
            let Some(resizable) = connection.as_resizable() else { continue };
            if !resizable.try_shrink_end(new_end) {
                continue;
            }

            let map = ctx.map.as_ref()?;
            let segment = Segment::new(map.next_chunk(), new_end + 1, end);
            let updated = map.with_split(connection.connection_id(), new_end, segment);
            SegmentMap::write(&ctx.temp_folder, &updated);
            ctx.map = Some(updated);

            let chunk_path = chunk_path(&ctx.temp_folder, segment.chunk);
            let _ = std::fs::remove_file(&chunk_path); // best-effort: the chunk number is new

            let created = self.create(ctx, segment.chunk, Range::new(segment.start, segment.end), 0, chunk_path);
            let mut shared = self.sync.lock();
            let mut list = Vec::with_capacity(shared.connections.len() + 1);
            list.extend(shared.connections.iter().cloned());
            list.push(Arc::clone(&created));
            shared.connections = Arc::new(list);
            return Some(created);
        }
        None
    }

    /// A fresh connection for a range whose connection has stopped, in place of
    /// it in the connection list — the list is what "did every connection
    /// finish" is asked of. It carries on from what the old one left on disk,
    /// or, when `discard`, from nothing: the chunk is deleted first.
    fn recreate(&self, ctx: &RunContext, old: &Connection, discard: bool) -> Connection {
        if discard {
            let _ = std::fs::remove_file(old.chunk_file_path()); // best-effort: a chunk that stays is rewritten from 0
        }
        let already = if discard { 0 } else { file_length(old.chunk_file_path()).unwrap_or(0) };
        let replacement = self.create(
            ctx,
            old.connection_id(),
            Range::new(old.range_start(), old.range_end()),
            already,
            old.chunk_file_path().to_string(),
        );

        let mut shared = self.sync.lock();
        let list: Vec<Connection> =
            shared.connections.iter().map(|c| if Arc::ptr_eq(c, old) { Arc::clone(&replacement) } else { Arc::clone(c) }).collect();
        shared.connections = Arc::new(list);
        replacement
    }

    fn build_connections(&self, ctx: &mut RunContext, file_info: &RemoteFileInfo, connections_count: i32) -> Result<Vec<Connection>> {
        let layout: Vec<(i32, Range)> = if file_info.supports_range_requests && file_info.has_known_size() {
            let map = load_or_create_map(&ctx.temp_folder, file_info.content_length, connections_count)?;
            let mut segments = map.segments.clone();
            segments.sort_by_key(|s| s.start);
            ctx.map = Some(map);
            segments.into_iter().map(|s| (s.chunk, Range::new(s.start, s.end))).collect()
        } else {
            // One connection: the whole file, or an open-ended range when the
            // server never said how big it is. There is no layout to keep, and a
            // map left by a run that could split would describe chunks that are
            // about to be discarded.
            ctx.map = None;
            SegmentMap::delete(&ctx.temp_folder);

            let single =
                Range::new(0, if file_info.has_known_size() { file_info.content_length - 1 } else { UNBOUNDED_END });
            discard_chunks_from_a_different_split(&ctx.temp_folder, &[(0, single)]);
            vec![(0, single)]
        };

        Ok(layout
            .into_iter()
            .map(|(chunk, range)| {
                let path = chunk_path(&ctx.temp_folder, chunk);
                let already = file_length(&path).unwrap_or(0);
                self.create(ctx, chunk, range, already, path)
            })
            .collect())
    }

    fn report_progress(&self) {
        let connections = self.connections();
        if connections.is_empty() {
            return;
        }
        let downloaded: i64 = connections.iter().map(|c| c.bytes_downloaded()).sum();
        let total = self.total_bytes.load(Ordering::Relaxed);
        self.speed_calculator.add_sample(downloaded);
        let speed = self.speed_calculator.speed_bytes_per_second();
        let eta = SpeedCalculator::estimate_time_remaining(downloaded, total, speed);

        self.hub.publish(PoolEvent::Progress(DownloadProgress {
            download_id: *self.download_id.lock(),
            downloaded_bytes: downloaded,
            total_bytes: total,
            speed_bytes_per_second: speed,
            estimated_time_remaining: eta,
            merge_percentage: None,
        }));
        self.hub.publish(PoolEvent::Connections(self.build_snapshot()));
    }

    fn build_snapshot(&self) -> Vec<ConnectionInfo> {
        let (connections, paused) = {
            let shared = self.sync.lock();
            (Arc::clone(&shared.connections), shared.paused.clone())
        };
        connections
            .iter()
            .map(|c| {
                let mut info = c.info();
                if paused.contains(&info.connection_id) {
                    info.status = ConnectionStatus::Paused;
                }
                info
            })
            .collect()
    }
}

fn held(slots: &[Slot]) -> usize {
    slots.iter().filter(|s| s.state == SlotState::Paused).count()
}

/// The scheduler's moves on its slots.
struct Scheduler<'a> {
    pool: &'a ConnectionPoolManager,
    run: &'a CancellationToken,
    file_changed: &'a Arc<Mutex<Option<CoreError>>>,
}

impl Scheduler<'_> {
    fn start(&self, slots: &mut [Slot], index: usize, running: &mut FuturesUnordered<BoxFuture<'static, (usize, Outcome)>>) {
        let slot = &mut slots[index];
        slot.state = SlotState::Running;
        let token = self.run.child_token();
        slot.cancel = Some(token.clone());
        let delay = self.pool.retry_delay(slot.attempt);
        let connection = Arc::clone(&slot.connection);
        let run = self.run.clone();
        let file_changed = Arc::clone(self.file_changed);
        // A task of its own per connection, so the chunk writes of eight of them
        // are not serialised on one worker. Bounded: never more than the run's
        // concurrency at once.
        let task = tokio::spawn(run_connection_safe(connection, token, run, delay, file_changed));
        running.push(Box::pin(async move {
            let outcome = task.await.unwrap_or_else(|e| Outcome::Failed(CoreError::other(format!("The connection stopped unexpectedly: {e}"))));
            (index, outcome)
        }));
    }

    fn fill(
        &mut self,
        ctx: &mut RunContext,
        slots: &mut Vec<Slot>,
        pending: &mut VecDeque<usize>,
        running: &mut FuturesUnordered<BoxFuture<'static, (usize, Outcome)>>,
        concurrency: usize,
        splitting: &mut bool,
    ) {
        while running.len() + held(slots) < concurrency && !self.run.is_cancelled() {
            let Some(next) = pending.pop_front() else { break };
            self.start(slots, next, running);
        }

        while *splitting && running.len() + held(slots) < concurrency && !self.run.is_cancelled() {
            let candidates: Vec<Connection> = slots
                .iter()
                .filter(|s| s.state == SlotState::Running && s.interrupt.is_none())
                .map(|s| Arc::clone(&s.connection))
                .collect();
            let Some(created) = self.pool.try_split(ctx, candidates.iter()) else { break };
            slots.push(Slot::new(created));
            let index = slots.len() - 1;
            self.start(slots, index, running);
        }
    }

    /// Front of the line: a range being recovered or resumed has already had its
    /// turn, and the segments still waiting have not lost anything yet.
    fn requeue(&self, ctx: &RunContext, slots: &mut [Slot], pending: &mut VecDeque<usize>, index: usize, discard: bool, attempt: i32) {
        let slot = &mut slots[index];
        self.pool.sync.lock().paused.remove(&slot.connection.connection_id());
        slot.connection = self.pool.recreate(ctx, &slot.connection, discard);
        slot.attempt = attempt;
        slot.state = SlotState::Pending;
        pending.push_front(index);
    }

    fn hold(&self, slots: &mut [Slot], index: usize) {
        slots[index].state = SlotState::Paused;
        self.pool.sync.lock().paused.insert(slots[index].connection.connection_id());
    }

    fn apply(&self, ctx: &RunContext, slots: &mut [Slot], pending: &mut VecDeque<usize>, connection_id: i32, command: ConnectionCommand) {
        let Some(index) = slots.iter().position(|s| s.connection.connection_id() == connection_id) else { return };
        let state = slots[index].state;
        match (command, state) {
            (ConnectionCommand::Pause | ConnectionCommand::Restart, SlotState::Running) => {
                // Carried out once its task has unwound and the chunk is closed.
                slots[index].interrupt = Some(command);
                if let Some(cancel) = &slots[index].cancel {
                    cancel.cancel();
                }
            }
            (ConnectionCommand::Pause, SlotState::Pending) => {
                pending.retain(|&i| i != index);
                self.hold(slots, index);
            }
            (ConnectionCommand::Resume, SlotState::Paused | SlotState::Failed) => {
                self.requeue(ctx, slots, pending, index, false, 1);
            }
            (ConnectionCommand::Resume, SlotState::Running) => {
                // A pause that has not landed yet is called off.
                if slots[index].interrupt == Some(ConnectionCommand::Pause) {
                    slots[index].interrupt = None;
                }
            }
            (ConnectionCommand::Restart, _) => {
                if state == SlotState::Pending {
                    pending.retain(|&i| i != index);
                }
                self.requeue(ctx, slots, pending, index, true, 1);
            }
            _ => {}
        }
    }
}

/// Runs one connection to its end and says how it ended, never failing: the
/// scheduler decides what a failure means, which depends on whether the range
/// was one of the originals or split off from one.
async fn run_connection_safe(
    connection: Connection,
    token: CancellationToken,
    run: CancellationToken,
    delay: Duration,
    file_changed: Arc<Mutex<Option<CoreError>>>,
) -> Outcome {
    if !delay.is_zero() {
        tokio::select! {
            _ = token.cancelled() => return Outcome::Done,
            _ = tokio::time::sleep(delay) => {}
        }
    }
    match connection.run(token).await {
        Ok(()) => Outcome::Done,
        // Expected on pause/stop, of the download or of this connection alone —
        // leave the partial chunk in place for resume.
        Err(CoreError::Cancelled) => Outcome::Done,
        Err(error @ CoreError::RemoteFileChanged(_)) => {
            file_changed.lock().get_or_insert(error);
            // The siblings are appending bytes of a version that no longer exists
            // to chunks that are about to be judged. Stopping them now saves the
            // bandwidth; the chunks themselves are left for DownloadService.
            run.cancel();
            Outcome::Done
        }
        Err(error) => Outcome::Failed(error),
    }
}

#[async_trait]
impl traits::ConnectionPoolManager for ConnectionPoolManager {
    fn subscribe(&self) -> EventStream<PoolEvent> {
        self.hub.subscribe()
    }

    fn subscribe_fn(&self, callback: EventCallback<PoolEvent>) {
        self.hub.subscribe_fn(callback);
    }

    fn snapshot(&self) -> Vec<ConnectionInfo> {
        self.build_snapshot()
    }

    async fn run(&self, item: &DownloadItem, file_info: &RemoteFileInfo, settings: Arc<DownloadSettings>, cancel: CancellationToken) -> Result<bool> {
        *self.download_id.lock() = item.id;
        self.total_bytes.store(file_info.content_length, Ordering::Relaxed);

        // Resolved once and shared: every connection of a download must present
        // the same login and the same captured headers, or the segments come back
        // from different things.
        let mut ctx = RunContext {
            url: item.url.clone(),
            temp_folder: item.temp_folder_path.clone(),
            settings: Arc::clone(&settings),
            options: item.to_request_options(),
            map: None,
        };

        let segmentable = file_info.supports_range_requests && file_info.has_known_size();
        let concurrency = if segmentable { settings.clamp_connections(item.connections_count) } else { 1 };

        std::fs::create_dir_all(&item.temp_folder_path)?;

        let built = self.build_connections(&mut ctx, file_info, concurrency)?;
        self.sync.lock().connections = Arc::new(built.clone());

        // Floor the sampling interval: the windows interpolate between samples,
        // and anything slower than ~50 ms starts to feel like stepping.
        let interval = settings.progress_interval();

        // A child rather than the caller's token directly: one connection
        // learning that the file changed on the server has to stop all of them,
        // and that is not a pause the caller asked for.
        let run = cancel.child_token();
        let file_changed = Arc::new(Mutex::new(None));
        let splitting = segmentable && settings.dynamic_segmentation && ctx.map.is_some();

        // Nothing a previous run left behind — a command queued as it ended — may reach this one.
        self.commands.lock().clear();
        self.running.store(true, Ordering::SeqCst);
        self.run_scheduled(&mut ctx, built, concurrency as usize, splitting, &run, &file_changed, interval).await;

        // A pause of a connection lasts as long as the run: the next run starts
        // every unfinished range, as a resumed download always has.
        self.running.store(false, Ordering::SeqCst);
        self.sync.lock().paused.clear();
        self.report_progress(); // final, exact snapshot

        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }

        // Every chunk belongs to the version that changed, not just the one whose
        // connection noticed, so this is the download's decision rather than a
        // failed connection's. DownloadService makes it.
        if let Some(changed) = file_changed.lock().take() {
            return Err(changed);
        }

        Ok(self.connections().iter().all(|c| c.status() == ConnectionStatus::Finished))
    }

    fn discard_chunks(&self, temp_folder: &str) {
        if temp_folder.trim().is_empty() {
            return;
        }
        // The layout goes with the chunks it described.
        SegmentMap::delete(temp_folder);
        for (path, _) in chunk_files(temp_folder) {
            // Only this pool's own names (chunk_files filters them). The folder is
            // the download's, but a file in it that does not parse as a chunk was
            // not put there by us.
            let _ = std::fs::remove_file(path);
        }
    }

    fn forget_layout(&self, temp_folder: &str) {
        if !temp_folder.trim().is_empty() {
            SegmentMap::delete(temp_folder);
        }
    }

    fn ordered_chunk_paths(&self) -> Vec<String> {
        let connections = self.connections();
        let mut ordered: Vec<&Connection> = connections.iter().collect();
        ordered.sort_by_key(|c| c.range_start());
        ordered.into_iter().map(|c| c.chunk_file_path().to_string()).collect()
    }

    fn control_connection(&self, connection_id: i32, command: ConnectionCommand) -> bool {
        if !self.running.load(Ordering::SeqCst) {
            return false;
        }
        let snapshot = self.build_snapshot();
        let Some(connection) = snapshot.iter().find(|c| c.connection_id == connection_id) else { return false };
        if !connection_actions::allows(command, connection) {
            return false;
        }
        self.commands.lock().push_back((connection_id, command));
        self.wake.notify_one();
        true
    }
}

/// The layout to resume from: the saved map when it describes this file and the
/// chunks on disk agree with it, else a fresh fixed split — which, for a
/// download from before maps existed, is exactly the layout its chunks have.
fn load_or_create_map(temp_folder: &str, total_bytes: i64, connections_count: i32) -> Result<SegmentMap> {
    if let Some(saved) = SegmentMap::read(temp_folder).filter(|m| m.describes(total_bytes)) {
        let saved_ranges: Vec<(i32, Range)> = saved.segments.iter().map(|s| (s.chunk, Range::new(s.start, s.end))).collect();
        if discard_chunks_from_a_different_split(temp_folder, &saved_ranges) {
            return Ok(saved);
        }
    }

    let ranges = range_splitter::split(total_bytes, connections_count)?;
    let layout: Vec<(i32, Range)> = ranges.iter().enumerate().map(|(i, r)| (i as i32, *r)).collect();
    discard_chunks_from_a_different_split(temp_folder, &layout);

    let map = SegmentMap::from_ranges(total_bytes, &ranges);
    SegmentMap::write(temp_folder, &map);
    Ok(map)
}

/// Throws away chunk files left by a run that split the file differently.
/// Returns whether the chunks on disk fit `layout` — nothing was discarded.
///
/// Resume reads a chunk's length off disk and trusts it to be the head of that
/// connection's range — which holds only while the layout stays the same, and it
/// does not have to. Retry re-resolves the file, and a server that stops
/// advertising byte ranges (or starts, or changes the length) moves the download
/// between one connection and N. The old part0.tmp then holds a prefix of the
/// whole file where the new split wants just the first eighth of it, every other
/// chunk is orphaned or misread, and the merge writes the difference into the
/// finished file without a word.
///
/// Two things give a foreign chunk set away, and neither mistakes a run that was
/// paused before every connection had opened its file for an incompatible one: a
/// chunk the layout does not name, and a chunk longer than the range it would
/// now be resuming.
fn discard_chunks_from_a_different_split(temp_folder: &str, layout: &[(i32, Range)]) -> bool {
    let existing = chunk_files(temp_folder);
    if existing.is_empty() {
        return true;
    }

    let compatible = existing.iter().all(|(path, index)| match layout.iter().find(|(chunk, _)| chunk == index) {
        None => false,
        Some((_, range)) => file_length(path).is_none_or(|length| length <= range.len()),
    });
    if compatible {
        return true;
    }
    for (path, _) in existing {
        let _ = std::fs::remove_file(path);
    }
    false
}

/// The `partN.tmp` files in a folder, with their N. Empty for a folder that is not there.
fn chunk_files(temp_folder: &str) -> Vec<(String, i32)> {
    let Ok(entries) = std::fs::read_dir(temp_folder) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let path = e.path();
            let index = chunk_index_of(&path)?;
            Some((path.to_string_lossy().into_owned(), index))
        })
        .collect()
}

/// The `N` of a `partN.tmp`, or `None` for anything else.
fn chunk_index_of(path: &Path) -> Option<i32> {
    let extension = path.extension()?.to_str()?;
    if !extension.eq_ignore_ascii_case("tmp") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let digits = stem.strip_prefix(CHUNK_PREFIX)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn chunk_path(temp_folder: &str, chunk: i32) -> String {
    Path::new(temp_folder).join(format!("{CHUNK_PREFIX}{chunk}.tmp")).to_string_lossy().into_owned()
}

fn file_length(path: &str) -> Option<i64> {
    std::fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| m.len() as i64)
}

#[cfg(test)]
mod tests;
