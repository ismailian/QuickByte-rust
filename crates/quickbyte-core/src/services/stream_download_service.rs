//! Port of `src/QuickByte.Core/Services/StreamDownloadService.cs`.
//!
//! One streaming video's lifecycle — the `DownloadService` of a download whose
//! `DownloadItem::stream` is set. Same statuses, same events, same
//! pause/resume/stop semantics; only where the bytes come from differs.
//!
//! A stream is not one file split into ranges but hundreds of small files listed
//! by a playlist. So instead of a connection pool over byte ranges this reads the
//! playlist, fetches up to `DownloadItem::connections_count` pieces at once, and
//! joins them with the same resumable [`FileMerger`] — one output per track,
//! since the sound is often a playlist of its own and there is no muxer here to
//! put it back inside the video.
//!
//! **Resume.** A finished piece is renamed from `.part` to its final name, so a
//! piece's presence is the whole record of it; a `.part` is carried on from its
//! length, like a chunk. Except when it is encrypted: AES-CBC cannot be continued
//! from plaintext, so an encrypted piece starts again. Pieces are small and that
//! costs seconds. [`StreamState`] holds the rest.
//!
//! **Decryption.** AES-128 (HLS's whole-piece encryption) is decrypted as the
//! piece is written, never stored encrypted. The key is fetched once per address
//! with the download's cookies — a player's session is often what the key server
//! checks — and held only in memory. DRM schemes are refused when the playlist is
//! read (`stream_plan`).

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::{Arc, Weak};

use aes::cipher::{BlockDecryptMut, KeyIvInit, generic_array::GenericArray};
use async_trait::async_trait;
use parking_lot::Mutex;
use reqwest::StatusCode;
use reqwest::header::RANGE;
use tokio::io::AsyncReadExt;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use crate::enums::{ConnectionCommand, ConnectionStatus, DownloadStatus};
use crate::error::{CoreError, Result};
use crate::events::{ConnectionsSnapshot, DownloadEvent, DownloadProgress, DownloadStatusChanged, EventCallback, EventHub, EventStream};
use crate::helpers::bandwidth_limiter::UnlimitedBandwidthLimiter;
use crate::helpers::file_checksum::{self, ChecksumResult};
use crate::helpers::file_name_helper::stem_of;
use crate::helpers::mark_of_the_web;
use crate::helpers::retry_policy::RetryPolicy;
use crate::helpers::speed_calculator::SpeedCalculator;
use crate::models::{AddressChange, ConnectionInfo, DownloadItem, DownloadSettings, RequestOptions, StreamFormat, StreamSource, join_path};
use crate::net::http_client::{BodyReader, HttpClient, HttpRequest};
use crate::net::remote_file_info_provider::content_range_from;
use crate::net::{authenticated_http, request_decorator};
use crate::serde_compat::{DateTimeOffset, TimeSpan};
use crate::services::download_service::{PersistFn, SpeedLimits, referrer_of, schedule_folder_deletion};
use crate::streaming::dash_manifest::DashManifest;
use crate::streaming::hls_playlist::HlsPlaylist;
use crate::streaming::stream_fetch;
use crate::streaming::stream_plan::{self, StreamPiece, StreamTrack};
use crate::streaming::stream_state::StreamState;
use crate::traits::{self, BandwidthLimiter, FileMerger, ProgressFn, SettingsService};

const AES_KEY_BYTES: usize = 16;
const AES_BLOCK: usize = 16;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// The parts of the C# constructor that have defaults.
#[derive(Clone)]
pub struct StreamServiceOptions {
    pub bandwidth_limiter: Option<Arc<dyn BandwidthLimiter>>,
    pub mark_from_internet: bool,
    pub speed_limits: Option<SpeedLimits>,
}

impl Default for StreamServiceOptions {
    fn default() -> Self {
        Self { bandwidth_limiter: None, mark_from_internet: true, speed_limits: None }
    }
}

/// One of the download's parallel slots, as the details window's connection list
/// shows it: the piece it is on, and how far through.
struct PieceWorker {
    id: i32,
    bytes: AtomicI64,
    length: AtomicI64,
    status: Mutex<ConnectionStatus>,
    retry_count: AtomicI32,
    last_error: Mutex<Option<String>>,
    /// The worker's one read buffer, lent to whichever piece it is fetching.
    buffer: Mutex<Vec<u8>>,
}

impl PieceWorker {
    fn new(id: i32) -> Self {
        Self {
            id,
            bytes: AtomicI64::new(0),
            length: AtomicI64::new(-1),
            status: Mutex::new(ConnectionStatus::Idle),
            retry_count: AtomicI32::new(0),
            last_error: Mutex::new(None),
            buffer: Mutex::new(Vec::new()),
        }
    }
    fn bytes(&self) -> i64 {
        self.bytes.load(Ordering::SeqCst)
    }
    fn sending(&self) {
        *self.status.lock() = ConnectionStatus::SendingRequest;
    }
    fn receiving(&self, have: i64, length: i64) {
        self.bytes.store(have, Ordering::SeqCst);
        self.length.store(length, Ordering::SeqCst);
        *self.status.lock() = ConnectionStatus::ReceivingData;
    }
    fn add(&self, count: usize) {
        self.bytes.fetch_add(count as i64, Ordering::SeqCst);
    }
    fn retrying(&self, attempt: i32, message: String) {
        self.bytes.store(0, Ordering::SeqCst);
        self.retry_count.store(attempt, Ordering::SeqCst);
        *self.last_error.lock() = Some(message);
        *self.status.lock() = ConnectionStatus::SendingRequest;
    }
    /// The piece's bytes now count as completed, so they stop counting here.
    fn done(&self) {
        self.bytes.store(0, Ordering::SeqCst);
        self.length.store(-1, Ordering::SeqCst);
        self.retry_count.store(0, Ordering::SeqCst);
        *self.status.lock() = ConnectionStatus::Idle;
    }
    fn failed(&self, message: String) {
        *self.last_error.lock() = Some(message);
        *self.status.lock() = ConnectionStatus::Failed;
    }
    fn snapshot(&self) -> ConnectionInfo {
        ConnectionInfo {
            connection_id: self.id,
            range_start: 0,
            range_end: self.length.load(Ordering::SeqCst) - 1,
            bytes_downloaded: self.bytes(),
            status: *self.status.lock(),
            retry_count: self.retry_count.load(Ordering::SeqCst),
            last_error: self.last_error.lock().clone(),
        }
    }
}

#[derive(Clone)]
struct Job {
    track: usize,
    index: usize,
    piece: StreamPiece,
}

pub struct StreamDownloadService {
    me: Weak<StreamDownloadService>,
    id: Uuid,
    item: Mutex<DownloadItem>,
    http: Arc<HttpClient>,
    merger: Arc<dyn FileMerger>,
    settings: Arc<dyn SettingsService>,
    persist: PersistFn,
    bandwidth_limiter: Arc<dyn BandwidthLimiter>,
    mark_from_internet: bool,
    speed_limits: Option<SpeedLimits>,

    lifecycle: tokio::sync::Mutex<()>,
    keys: Mutex<HashMap<String, Arc<tokio::sync::OnceCell<[u8; AES_KEY_BYTES]>>>>,
    report_lock: Mutex<()>,
    cts: Mutex<Option<CancellationToken>>,

    // One run's accounting, reset at the start of each download phase.
    workers: Mutex<Arc<Vec<Arc<PieceWorker>>>>,
    completed_bytes: AtomicI64,
    completed_media_bytes: AtomicI64,
    completed_media_pieces: AtomicI32,
    total_media_pieces: AtomicI32,
    transferred: AtomicI64,
    speed: Mutex<Arc<SpeedCalculator>>,

    hub: EventHub<DownloadEvent>,
}

impl StreamDownloadService {
    pub fn new(
        item: DownloadItem,
        http: Arc<HttpClient>,
        merger: Arc<dyn FileMerger>,
        settings: Arc<dyn SettingsService>,
        persist: PersistFn,
        options: StreamServiceOptions,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            id: item.id,
            item: Mutex::new(item),
            http,
            merger,
            settings,
            persist,
            bandwidth_limiter: options.bandwidth_limiter.unwrap_or_else(UnlimitedBandwidthLimiter::shared),
            mark_from_internet: options.mark_from_internet,
            speed_limits: options.speed_limits,
            lifecycle: tokio::sync::Mutex::new(()),
            keys: Mutex::new(HashMap::new()),
            report_lock: Mutex::new(()),
            cts: Mutex::new(None),
            workers: Mutex::new(Arc::new(Vec::new())),
            completed_bytes: AtomicI64::new(0),
            completed_media_bytes: AtomicI64::new(0),
            completed_media_pieces: AtomicI32::new(0),
            total_media_pieces: AtomicI32::new(0),
            transferred: AtomicI64::new(0),
            speed: Mutex::new(Arc::new(SpeedCalculator::default())),
            hub: EventHub::default(),
        })
    }

    fn with_item<R>(&self, f: impl FnOnce(&mut DownloadItem) -> R) -> R {
        f(&mut self.item.lock())
    }

    fn temp_folder(&self) -> String {
        self.with_item(|i| i.temp_folder_path.clone())
    }

    async fn run_lifecycle(&self, cancel: &CancellationToken) -> Result<()> {
        let Some(source) = self.with_item(|i| i.stream.clone()) else {
            return Err(CoreError::invalid("This download is not a stream."));
        };
        let temp_root = self.settings.current().temp_folder.clone();
        self.with_item(|i| {
            if i.temp_folder_path.trim().is_empty() {
                i.temp_folder_path = join_path(&temp_root, &i.id.simple().to_string());
            }
        });
        let temp = PathBuf::from(self.temp_folder());

        // Keys are fetched afresh each run: they are held only in memory, and a
        // key server that refused the last run may well accept this one.
        self.keys.lock().clear();

        let mut state = StreamState::read(&temp);
        if !state.as_ref().is_some_and(|s| s.downloaded) {
            let tracks = self.read_playlists(&source, cancel).await?;

            if !state.as_ref().is_some_and(|s| s.describes(&tracks)) {
                self.discard_pieces(tracks.len());
                let fresh = StreamState::for_tracks(&tracks);
                StreamState::write(&temp, &fresh)?;
                state = Some(fresh);
            }

            if cancel.is_cancelled() {
                return Err(CoreError::Cancelled);
            }
            self.set_status(DownloadStatus::Downloading, None);
            self.download_pieces(&tracks, cancel).await?;
            if cancel.is_cancelled() {
                return Err(CoreError::Cancelled);
            }

            let mut done = state.take().unwrap_or_default();
            done.downloaded = true;
            StreamState::write(&temp, &done)?;
            state = Some(done);
        }

        self.merge(state.unwrap_or_default(), cancel).await
    }

    // ------------------------------------------------------------- playlists --

    /// Reads the chosen quality's playlists again. Every run does: piece
    /// addresses are commonly signed and expire within hours, and the playlist is
    /// what hands out current ones.
    async fn read_playlists(&self, source: &StreamSource, cancel: &CancellationToken) -> Result<Vec<StreamTrack>> {
        let url = self.with_item(|i| i.url.clone());
        let mut tracks = Vec::with_capacity(2);
        match source.format {
            StreamFormat::Hls => {
                let main = source.main_playlist_url.clone().unwrap_or(url);
                tracks.push(stream_plan::for_hls(&self.read_hls(&main, cancel).await?)?);
                if let Some(audio) = &source.audio_playlist_url {
                    tracks.push(stream_plan::for_hls(&self.read_hls(audio, cancel).await?)?);
                }
            }
            StreamFormat::Dash => {
                let (text, final_url) = self.fetch_text(&url, cancel).await?;
                let manifest = DashManifest::parse(&text, &final_url)?;
                tracks.push(stream_plan::for_dash(&manifest, source.main_representation.as_deref().unwrap_or(""))?);
                if let Some(audio) = &source.audio_representation {
                    tracks.push(stream_plan::for_dash(&manifest, audio)?);
                }
            }
        }
        Ok(tracks)
    }

    async fn read_hls(&self, url: &str, cancel: &CancellationToken) -> Result<HlsPlaylist> {
        let (text, final_url) = self.fetch_text(url, cancel).await?;
        HlsPlaylist::parse(&text, &final_url)
    }

    /// Retried like a connection: a playlist read is a request like any other.
    async fn fetch_text(&self, url: &str, cancel: &CancellationToken) -> Result<(String, Url)> {
        let uri = Url::parse(url).map_err(|_| CoreError::invalid("This download's address is not a web address."))?;
        let settings = self.settings.current();
        let options = self.with_item(|i| i.to_request_options());
        RetryPolicy::from_settings(&settings)
            .execute(|_| stream_fetch::get_text(&self.http, &uri, Some(&options), cancel), |_, _| {}, cancel)
            .await
    }

    // ---------------------------------------------------------------- pieces --

    async fn download_pieces(&self, tracks: &[StreamTrack], cancel: &CancellationToken) -> Result<()> {
        let settings = self.settings.current();
        let options = self.with_item(|i| i.to_request_options());
        let worker_count = settings.clamp_connections(self.with_item(|i| i.connections_count)) as usize;

        self.completed_bytes.store(0, Ordering::SeqCst);
        self.completed_media_bytes.store(0, Ordering::SeqCst);
        self.completed_media_pieces.store(0, Ordering::SeqCst);
        self.transferred.store(0, Ordering::SeqCst);
        *self.speed.lock() = Arc::new(SpeedCalculator::default());
        let media: usize = tracks.iter().map(|t| t.pieces.iter().filter(|p| !p.is_initialization).count()).sum();
        self.total_media_pieces.store(media as i32, Ordering::SeqCst);

        let mut jobs = Vec::new();
        for (t, track) in tracks.iter().enumerate() {
            fs::create_dir_all(self.track_folder(t))?;
            for (i, piece) in track.pieces.iter().enumerate() {
                match fs::metadata(self.piece_path(t, i)) {
                    Ok(done) if done.is_file() => self.record_completed(piece, done.len() as i64),
                    _ => jobs.push(Job { track: t, index: i, piece: piece.clone() }),
                }
            }
        }

        let workers: Vec<Arc<PieceWorker>> = (0..worker_count as i32).map(|id| Arc::new(PieceWorker::new(id))).collect();
        let mut idle: Vec<Arc<PieceWorker>> = workers.iter().rev().cloned().collect();
        *self.workers.lock() = Arc::new(workers);

        // The first piece to fail for good stops new pieces from starting, but the
        // ones already under way are let finish: each is progress a retry keeps.
        let mut failure: Option<CoreError> = None;

        let interval = settings.progress_interval();
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let me = self.me.upgrade().ok_or(CoreError::Cancelled)?;
        let mut running: JoinSet<(Arc<PieceWorker>, Result<()>)> = JoinSet::new();
        let mut queue = jobs.into_iter();
        loop {
            while failure.is_none() && !cancel.is_cancelled() && !idle.is_empty() {
                let Some(job) = queue.next() else { break };
                let worker = idle.pop().expect("checked above");
                let (service, options, settings, cancel) = (Arc::clone(&me), options.clone(), Arc::clone(&settings), cancel.clone());
                running.spawn(async move {
                    let result = service.run_job(&job, &worker, &options, &settings, &cancel).await;
                    (worker, result)
                });
            }
            if running.is_empty() {
                break;
            }
            tokio::select! {
                joined = running.join_next() => {
                    let Some(joined) = joined else { continue };
                    match joined {
                        Ok((worker, result)) => {
                            if let Err(error) = result
                                && !error.is_cancelled()
                            {
                                worker.failed(error.to_string());
                                failure.get_or_insert(error);
                            }
                            idle.push(worker);
                        }
                        Err(e) => {
                            failure.get_or_insert(CoreError::other(format!("A piece stopped unexpectedly: {e}")));
                        }
                    }
                }
                _ = ticker.tick() => self.report(),
            }
        }
        self.report();

        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        if let Some(failure) = failure {
            return Err(failure);
        }

        // Exact now that every piece is on disk.
        let total = self.completed_bytes.load(Ordering::SeqCst);
        self.with_item(|i| {
            i.total_bytes = total;
            i.downloaded_bytes = total;
        });
        Ok(())
    }

    async fn run_job(&self, job: &Job, worker: &PieceWorker, options: &RequestOptions, settings: &DownloadSettings, cancel: &CancellationToken) -> Result<()> {
        let length = RetryPolicy::from_settings(settings)
            .execute(
                |_| self.fetch_piece(job, worker, options, settings, cancel),
                |attempt, error| worker.retrying(attempt, error.to_string()),
                cancel,
            )
            .await?;
        let _report = self.report_lock.lock();
        self.record_completed(&job.piece, length);
        worker.done();
        Ok(())
    }

    fn record_completed(&self, piece: &StreamPiece, length: i64) {
        self.completed_bytes.fetch_add(length, Ordering::SeqCst);
        if piece.is_initialization {
            return;
        }
        self.completed_media_bytes.fetch_add(length, Ordering::SeqCst);
        self.completed_media_pieces.fetch_add(1, Ordering::SeqCst);
    }

    /// Fetches one piece into its `.part` file and renames it into place.
    /// Returns the piece's length on disk.
    async fn fetch_piece(&self, job: &Job, worker: &PieceWorker, options: &RequestOptions, settings: &DownloadSettings, cancel: &CancellationToken) -> Result<i64> {
        worker.sending();
        let piece = &job.piece;
        let path = self.piece_path(job.track, job.index);
        let part = PathBuf::from(format!("{}.part", path.to_string_lossy()));

        let key = match &piece.key {
            Some(key) => Some(self.get_key(&key.uri, options, cancel).await?),
            None => None,
        };

        // What is already in the .part, when it can be carried on from. Encrypted
        // bytes cannot be: the file holds plaintext, and CBC needs the ciphertext
        // block before the one it resumes at.
        let mut have = if key.is_none() { fs::metadata(&part).map(|m| m.len() as i64).unwrap_or(0) } else { 0 };
        if piece.range.is_some_and(|r| have >= r.length) {
            have = 0;
        }
        let first_byte = piece.range.map_or(0, |r| r.offset) + have;
        let ranged = piece.range.is_some() || have > 0;

        let mut request = HttpRequest::get(piece.uri.clone());
        request_decorator::apply(&mut request.headers, Some(options));
        if let Some(range) = piece.range {
            request.set_header(RANGE, &format!("bytes={first_byte}-{}", range.end()));
        } else if have > 0 {
            request.set_header(RANGE, &format!("bytes={first_byte}-"));
        }

        let response = authenticated_http::send(&self.http, &request, Some(options), cancel).await?;
        let response = stream_fetch::ensure_success(response, Some(options))?;

        // Checked before a byte is written, for the reason the connection checks:
        // a server that ignores a range says so only in its status line.
        let mut skip = 0;
        let partial = response.status() == StatusCode::PARTIAL_CONTENT;
        if ranged {
            if partial
                && let Some(from) = content_range_from(&response)
                && from != first_byte
            {
                return Err(CoreError::Io(format!("The server answered a request for byte {first_byte} with a range starting at {from}.")));
            }
            // The whole resource from byte zero: read past what is not wanted.
            if !partial {
                skip = first_byte;
            }
        }

        let mut remaining: Option<i64> = match piece.range {
            Some(wanted) => Some(wanted.length - have),
            None => response.content_length().map(|l| l - skip),
        };
        if remaining.is_some_and(|r| r < 0) {
            return Err(CoreError::Io("The server sent less than the piece it was asked for.".into()));
        }
        worker.receiving(have, remaining.map_or(-1, |left| have + left));

        let buffer_size = settings.clamp_buffer_size();
        let mut buffer = std::mem::take(&mut *worker.buffer.lock());
        if buffer.len() != buffer_size {
            buffer = vec![0; buffer_size];
        }

        let result = async {
            let mut file = OpenOptions::new().write(true).create(true).truncate(false).open(&part)?;
            file.set_len(have as u64)?;
            file.seek(SeekFrom::Start(have as u64))?;

            let mut body = response.into_reader();
            if skip > 0 {
                skip_bytes(&mut body, skip, &mut buffer, cancel).await?;
            }

            let mut decryptor = match (&key, &piece.key) {
                (Some(key), Some(spec)) => Some(CbcSink::new(key, &spec.iv)),
                _ => None,
            };

            loop {
                // Ask before reading, and return what was not used — the same
                // contract every connection keeps with the speed limit.
                let allowance = self.bandwidth_limiter.request(buffer.len(), cancel).await?;
                let want = match remaining {
                    Some(left) => (allowance as i64).min(left).max(0) as usize,
                    None => allowance,
                };
                let read = if want == 0 {
                    0
                } else {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                        read = body.read(&mut buffer[..want]) => read?,
                    }
                };
                if read == 0 {
                    self.bandwidth_limiter.give_back(allowance);
                    break;
                }
                self.bandwidth_limiter.give_back(allowance - read);

                match decryptor.as_mut() {
                    Some(sink) => sink.write(&buffer[..read], &mut file)?,
                    None => file.write_all(&buffer[..read])?,
                }
                if let Some(left) = remaining.as_mut() {
                    *left -= read as i64;
                }
                worker.add(read);
                self.transferred.fetch_add(read as i64, Ordering::SeqCst);
            }

            if let Some(left) = remaining
                && left > 0
            {
                return Err(CoreError::Io(format!("The connection ended {left} bytes early.")));
            }

            // Fails on a wrong key or a damaged piece (the padding does not check
            // out). Retried: the next attempt starts the piece over.
            if let Some(sink) = decryptor {
                sink.finish(&mut file)?;
            }
            file.flush()?;
            Ok(())
        }
        .await;
        *worker.buffer.lock() = buffer;
        result?;

        fs::rename(&part, &path)?;
        Ok(fs::metadata(&path)?.len() as i64)
    }

    /// One fetch per key address however many pieces share it; a failed one is
    /// forgotten so the next piece asks again.
    async fn get_key(&self, uri: &Url, options: &RequestOptions, cancel: &CancellationToken) -> Result<[u8; AES_KEY_BYTES]> {
        let cell = Arc::clone(self.keys.lock().entry(uri.to_string()).or_default());
        match cell.get_or_try_init(|| self.fetch_key(uri, options, cancel)).await {
            Ok(key) => Ok(*key),
            Err(error) => {
                let mut keys = self.keys.lock();
                if keys.get(uri.as_str()).is_some_and(|c| Arc::ptr_eq(c, &cell)) {
                    keys.remove(uri.as_str());
                }
                Err(error)
            }
        }
    }

    async fn fetch_key(&self, uri: &Url, options: &RequestOptions, cancel: &CancellationToken) -> Result<[u8; AES_KEY_BYTES]> {
        let mut request = HttpRequest::get(uri.clone());
        request_decorator::apply(&mut request.headers, Some(options));
        let response = authenticated_http::send(&self.http, &request, Some(options), cancel).await?;
        let response = stream_fetch::ensure_success(response, Some(options))?;
        let key = response.read_at_most(AES_KEY_BYTES).await?;

        // A sign-in page or an error document where the key should be: decrypting
        // with it would "succeed" into noise, or fail every piece one at a time.
        match key {
            Some(bytes) if bytes.len() == AES_KEY_BYTES => {
                let mut key = [0u8; AES_KEY_BYTES];
                key.copy_from_slice(&bytes);
                Ok(key)
            }
            _ => Err(CoreError::StreamNotSupported("The server did not hand out this video's decryption key.".into())),
        }
    }

    // ----------------------------------------------------------------- merge --

    async fn merge(&self, mut state: StreamState, cancel: &CancellationToken) -> Result<()> {
        self.with_item(|i| {
            if i.total_bytes > 0 {
                i.downloaded_bytes = i.total_bytes;
            }
            i.merge_progress_percentage = 0.0;
        });
        self.set_status(DownloadStatus::Merging, None);

        let temp = PathBuf::from(self.temp_folder());
        let outputs = self.output_paths();
        let track_count = state.piece_counts.len();

        for t in state.merged_tracks.max(0) as usize..track_count {
            if cancel.is_cancelled() {
                return Err(CoreError::Cancelled);
            }
            let folder = self.track_folder(t).to_string_lossy().into_owned();
            let destination = outputs[t.min(outputs.len() - 1)].clone();
            let expected: Vec<String> =
                (0..state.piece_counts[t].max(0) as usize).map(|i| self.piece_path(t, i).to_string_lossy().into_owned()).collect();

            // An interrupted merge carries on from its own plan, which names the pieces.
            let mut pieces: Vec<String> = Vec::new();
            if !self.merger.has_interrupted_merge(&folder) {
                pieces = expected.clone();

                // Joined and moved into place by a run that was interrupted before
                // it could record so: nothing of this track is left but its output.
                if !pieces.iter().any(|p| Path::new(p).is_file()) && Path::new(&destination).is_file() {
                    state.merged_tracks = t as i32 + 1;
                    StreamState::write(&temp, &state)?;
                    continue;
                }

                if !pieces.iter().all(|p| Path::new(p).is_file()) {
                    StreamState::write(&temp, &StreamState { downloaded: false, merged_tracks: 0, ..state.clone() })?;
                    return Err(CoreError::Io(
                        "Part of this download is no longer on disk, so it has to be downloaded again. Click Retry.".into(),
                    ));
                }
            }

            let track_bytes = if pieces.is_empty() {
                self.with_item(|i| i.total_bytes) / track_count as i64
            } else {
                pieces.iter().map(|p| fs::metadata(p).map(|m| m.len() as i64).unwrap_or(0)).sum()
            }
            .max(1);
            let me = self.me.clone();
            let progress: ProgressFn<i64> = Arc::new(move |merged: i64| {
                let Some(service) = me.upgrade() else { return };
                let percentage =
                    ((t as f64 + (merged as f64 / track_bytes as f64).clamp(0.0, 1.0)) * 100.0 / track_count as f64).clamp(0.0, 100.0);
                service.with_item(|i| i.merge_progress_percentage = percentage);
                service.raise_progress(0.0, Some(TimeSpan(0)), Some(percentage));
            });

            let buffer = self.settings.current().clamp_buffer_size();
            if let Err(error) = self.merger.merge(&folder, &pieces, &destination, buffer, Some(progress), cancel).await {
                if matches!(error, CoreError::Io(_))
                    && !cancel.is_cancelled()
                    && !self.merger.has_interrupted_merge(&folder)
                    && !expected.iter().all(|p| Path::new(p).is_file())
                {
                    // The merger gave up on its plan because a piece went missing.
                    // The next run fetches what is missing before it merges again.
                    StreamState::write(&temp, &StreamState { downloaded: false, merged_tracks: 0, ..state.clone() })?;
                }
                return Err(error);
            }

            self.merger.cleanup_chunks(&pieces, &folder);
            state.merged_tracks = t as i32 + 1;
            StreamState::write(&temp, &state)?;
        }

        StreamState::delete(&temp);
        // Best-effort — the startup sweep collects it.
        let _ = fs::remove_dir_all(&temp);

        let outputs: Vec<String> = outputs.into_iter().take(track_count).collect();
        if self.mark_from_internet {
            let (url, referrer) = self.with_item(|i| (i.url.clone(), referrer_of(i)));
            for output in &outputs {
                mark_of_the_web::apply(output, Some(&url), referrer.as_deref());
            }
        }

        let total: i64 = outputs.iter().map(|p| fs::metadata(p).map(|m| m.len() as i64).unwrap_or(0)).sum();
        if total > 0 {
            self.with_item(|i| i.total_bytes = total);
        }

        let expected = self.with_item(|i| file_checksum::from_stored(i.expected_checksum.as_deref()));
        if let Some(expected) = expected {
            self.with_item(|i| i.merge_progress_percentage = 0.0);
            self.set_status(DownloadStatus::Verifying, None);
            let result = file_checksum::verify(&outputs[0], &expected, None, cancel).await?;
            self.with_item(|i| i.checksum_result = result);
            if result != ChecksumResult::Match {
                self.set_status(
                    DownloadStatus::Failed,
                    Some(format!(
                        "The finished file does not match its {} checksum. It is damaged or not the file that was published.",
                        file_checksum::name(expected.algorithm)
                    )),
                );
                return Ok(());
            }
        }

        self.with_item(|i| {
            i.merge_progress_percentage = 100.0;
            i.downloaded_bytes = i.total_bytes;
            i.completed_at = Some(DateTimeOffset::now());
        });
        self.set_status(DownloadStatus::Completed, None);
        Ok(())
    }

    /// Where each track is saved: the download's own file, then the separate sound beside it.
    fn output_paths(&self) -> Vec<String> {
        self.with_item(|item| {
            let mut paths = vec![item.full_path()];
            if let Some(audio) = item.audio_full_path() {
                paths.push(audio);
            } else if let Some(stream) = item.stream.as_ref().filter(|s| s.has_separate_audio()) {
                let extension = stream.audio_extension.clone().unwrap_or_else(|| ".m4a".into());
                paths.push(join_path(&item.save_folder, &format!("{}.audio{extension}", stem_of(&item.file_name))));
            }
            paths
        })
    }

    fn track_folder(&self, track: usize) -> PathBuf {
        Path::new(&self.temp_folder()).join(if track == 0 { "main" } else { "audio" })
    }

    fn piece_path(&self, track: usize, index: usize) -> PathBuf {
        self.track_folder(track).join(format!("{index:06}.seg"))
    }

    fn discard_pieces(&self, track_count: usize) {
        let outputs = self.output_paths();
        for t in 0..track_count.max(2) {
            let folder = self.track_folder(t).to_string_lossy().into_owned();
            self.merger.discard_interrupted_merge(&folder, outputs.get(t).map_or("", String::as_str));
            // Best-effort — pieces the new playlist names are overwritten anyway.
            let _ = fs::remove_dir_all(&folder);
        }
        StreamState::delete(Path::new(&self.temp_folder()));
        self.with_item(|i| {
            i.downloaded_bytes = 0;
            i.merge_progress_percentage = 0.0;
        });
    }

    // -------------------------------------------------------------- progress --

    fn report(&self) {
        let _report = self.report_lock.lock();
        let workers = Arc::clone(&self.workers.lock());
        let completed = self.completed_bytes.load(Ordering::SeqCst);
        let downloaded = completed + workers.iter().map(|w| w.bytes()).sum::<i64>();

        // The size of a stream is only known once it is down: until then, the
        // pieces still to come are taken to be the size of those already here.
        let mut total = 0;
        let done_pieces = self.completed_media_pieces.load(Ordering::SeqCst);
        if done_pieces > 0 {
            let average = self.completed_media_bytes.load(Ordering::SeqCst) / done_pieces as i64;
            total = completed + average * (self.total_media_pieces.load(Ordering::SeqCst) - done_pieces).max(0) as i64;
        }
        total = total.max(downloaded);

        let speed_calculator = Arc::clone(&self.speed.lock());
        speed_calculator.add_sample(self.transferred.load(Ordering::SeqCst));
        let speed = speed_calculator.speed_bytes_per_second();

        let eta = self.with_item(|item| {
            item.downloaded_bytes = downloaded;
            item.total_bytes = if done_pieces > 0 { total } else { 0 };
            item.current_speed_bytes_per_second = speed;
            item.estimated_time_remaining = SpeedCalculator::estimate_time_remaining(downloaded, item.total_bytes, speed);
            item.estimated_time_remaining
        });

        self.raise_progress(speed, eta, None);
        self.hub.publish(DownloadEvent::Connections(ConnectionsSnapshot {
            download_id: self.id,
            connections: Arc::new(workers.iter().map(|w| w.snapshot()).collect()),
        }));
    }

    fn raise_progress(&self, speed: f64, remaining: Option<TimeSpan>, merge_percentage: Option<f64>) {
        let (downloaded, total) = self.with_item(|i| (i.downloaded_bytes, i.total_bytes));
        self.hub.publish(DownloadEvent::Progress(DownloadProgress {
            download_id: self.id,
            downloaded_bytes: downloaded,
            total_bytes: total,
            speed_bytes_per_second: speed,
            estimated_time_remaining: remaining,
            merge_percentage,
        }));
    }

    fn set_status(&self, new_status: DownloadStatus, error_message: Option<String>) {
        let (old, snapshot) = self.with_item(|item| {
            let old = item.status;
            item.status = new_status;
            item.error_message = error_message.clone();
            (old, item.clone())
        });
        (self.persist)(&snapshot);
        self.hub.publish(DownloadEvent::StatusChanged(DownloadStatusChanged {
            download_id: self.id,
            old_status: old,
            new_status,
            error_message,
        }));
    }
}

/// Reads past `count` bytes a server sent that the piece does not want.
async fn skip_bytes(body: &mut BodyReader, mut count: i64, buffer: &mut [u8], cancel: &CancellationToken) -> Result<()> {
    while count > 0 {
        let want = buffer.len().min(count as usize);
        let read = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(CoreError::Cancelled),
            read = body.read(&mut buffer[..want]) => read?,
        };
        if read == 0 {
            return Err(CoreError::Io("The server sent less than the piece it was asked for.".into()));
        }
        count -= read as i64;
    }
    Ok(())
}

/// AES-128-CBC decryption as the bytes arrive: whole blocks are decrypted and
/// written straight away, except the last one seen, which is held back because it
/// may be the one carrying the PKCS#7 padding.
struct CbcSink {
    decryptor: Aes128CbcDec,
    pending: Vec<u8>,
}

impl CbcSink {
    fn new(key: &[u8; AES_KEY_BYTES], iv: &[u8; AES_BLOCK]) -> Self {
        Self { decryptor: Aes128CbcDec::new(key.into(), iv.into()), pending: Vec::with_capacity(64 * 1024 + AES_BLOCK) }
    }

    fn write(&mut self, cipher: &[u8], out: &mut impl Write) -> Result<()> {
        self.pending.extend_from_slice(cipher);
        // Keep at least one whole block back, plus any partial one.
        let ready = if self.pending.len() > AES_BLOCK { ((self.pending.len() - 1) / AES_BLOCK) * AES_BLOCK } else { 0 };
        if ready == 0 {
            return Ok(());
        }
        for block in self.pending[..ready].chunks_exact_mut(AES_BLOCK) {
            self.decryptor.decrypt_block_mut(GenericArray::from_mut_slice(block));
        }
        out.write_all(&self.pending[..ready])?;
        self.pending.drain(..ready);
        Ok(())
    }

    fn finish(mut self, out: &mut impl Write) -> Result<()> {
        let bad = || CoreError::Io("Padding is invalid and cannot be removed.".into());
        if self.pending.len() != AES_BLOCK {
            return Err(bad());
        }
        let block = GenericArray::from_mut_slice(&mut self.pending[..]);
        self.decryptor.decrypt_block_mut(block);
        let pad = self.pending[AES_BLOCK - 1] as usize;
        if pad == 0 || pad > AES_BLOCK || !self.pending[AES_BLOCK - pad..].iter().all(|&b| b as usize == pad) {
            return Err(bad());
        }
        out.write_all(&self.pending[..AES_BLOCK - pad])?;
        Ok(())
    }
}

#[async_trait]
impl traits::DownloadService for StreamDownloadService {
    fn id(&self) -> Uuid {
        self.id
    }

    fn item(&self) -> DownloadItem {
        self.item.lock().clone()
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
        let cancel = CancellationToken::new();
        *self.cts.lock() = Some(cancel.clone());
        self.set_status(DownloadStatus::Connecting, None);

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
        traits::DownloadService::start(self).await
    }

    fn stop(&self) {
        if let Some(cancel) = self.cts.lock().as_ref() {
            cancel.cancel();
        }
        if self.with_item(|i| i.status) == DownloadStatus::Completed {
            return;
        }
        self.set_status(DownloadStatus::Cancelled, None);

        for (t, output) in self.output_paths().iter().enumerate() {
            self.merger.discard_interrupted_merge(&self.track_folder(t).to_string_lossy(), output);
        }
        // See DownloadService's twin: the pieces are still open when stop returns.
        let path = self.temp_folder();
        if !path.is_empty() {
            let me = self.me.clone();
            schedule_folder_deletion(path, move || me.upgrade().is_some_and(|s| s.with_item(|i| i.status) == DownloadStatus::Cancelled));
        }
    }

    async fn retry(&self) -> Result<()> {
        self.with_item(|i| i.error_message = None);
        traits::DownloadService::start(self).await
    }

    /// Not offered for a stream (`download_actions::can_refresh_address`). A
    /// fresh playlist is read on every run anyway, and a different playlist is a
    /// different cut of the video, which no size comparison could vouch for.
    fn change_address(&self, _change: AddressChange) -> bool {
        false
    }

    /// A stream's "connections" are pieces fetched a few at a time, each over in
    /// a second or two; there is no long-lived range for a command to steer.
    fn control_connection(&self, _connection_id: i32, _command: ConnectionCommand) -> bool {
        false
    }

    fn connections(&self) -> Vec<ConnectionInfo> {
        self.workers.lock().iter().map(|w| w.snapshot()).collect()
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
