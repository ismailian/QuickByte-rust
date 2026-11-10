//! Port of `src/QuickByte.Core/Services/Ftp/FtpDownloadConnection.cs`.
//!
//! The FTP counterpart of [`crate::net::download_connection::DownloadConnection`]:
//! fetches exactly one byte range into one chunk file, reports its own state,
//! and knows nothing about its siblings.
//!
//! The differences from the HTTP version are both consequences of FTP having no
//! Range header:
//!
//! * Only the *start* of a segment can be requested (`REST`), so the end is
//!   enforced here by counting bytes and closing the data connection — the
//!   server would otherwise keep sending to the end of the file.
//! * Every attempt opens its own control connection. There is no connection
//!   pool to share: an FTP control channel is stateful (working directory,
//!   transfer type, restart offset), so two segments cannot take turns on one.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::enums::ConnectionStatus;
use crate::error::{CoreError, Result};
use crate::ftp::control_channel::FtpControlChannel;
use crate::ftp::ftp_url;
use crate::helpers::retry_policy::RetryPolicy;
use crate::models::{DownloadCredentials, DownloadSettings};
use crate::net::app_proxy::AppProxy;
use crate::net::download_connection::{AtomicStatus, allowance, give_back, open_chunk};
use crate::net::http_client::parse_url;
use crate::net::segment_range::SegmentRange;
use crate::traits::{BandwidthLimiter, ConnectionSpec, ResizableConnection};

pub struct FtpDownloadConnection {
    connection_id: i32,
    url: String,
    /// The range, which a split may shorten while this runs — see [`SegmentRange`].
    range: SegmentRange,
    chunk_file_path: String,
    settings: Arc<DownloadSettings>,
    bandwidth_limiter: Option<Arc<dyn BandwidthLimiter>>,
    credentials: Option<DownloadCredentials>,
    proxy: Arc<AppProxy>,
    bytes_downloaded: AtomicI64,
    // Atomic for the same reason the status is: written by the transfer and
    // read by the pool's report timer.
    retry_count: AtomicI32,
    status: AtomicStatus,
    last_error: Mutex<Option<String>>,
    buffer: tokio::sync::Mutex<Vec<u8>>,
}

impl FtpDownloadConnection {
    pub fn new(spec: ConnectionSpec) -> Self {
        Self::with_proxy(spec, Arc::clone(AppProxy::shared()))
    }

    pub fn with_proxy(spec: ConnectionSpec, proxy: Arc<AppProxy>) -> Self {
        let range = SegmentRange::new(spec.range_start, spec.range_end);
        let total = range.len();
        Self {
            connection_id: spec.connection_id,
            url: spec.url,
            range,
            chunk_file_path: spec.chunk_file_path,
            settings: spec.settings,
            bandwidth_limiter: spec.bandwidth_limiter,
            credentials: spec.options.credentials,
            proxy,
            bytes_downloaded: AtomicI64::new(spec.already_downloaded.clamp(0, total.max(0))),
            retry_count: AtomicI32::new(0),
            status: AtomicStatus::new(ConnectionStatus::Idle),
            last_error: Mutex::new(None),
            buffer: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    fn downloaded(&self) -> i64 {
        self.bytes_downloaded.load(Ordering::SeqCst)
    }

    /// True when this connection was handed no real end — see
    /// `RangeSplitter::UNBOUNDED_END`. The data connection closing is then the
    /// end of the file rather than a transfer cut short.
    fn is_unbounded(&self) -> bool {
        self.range.is_unbounded()
    }

    async fn download_once(&self, url: &Url, cancel: &CancellationToken) -> Result<()> {
        match self.transfer(url, cancel).await {
            Err(CoreError::FtpRestartNotSupported) => {
                // The chunk on disk can no longer be continued, and re-requesting
                // the same offset would be refused identically every time. Throwing
                // the partial away and fetching from zero is the only path that
                // finishes, so it happens here rather than costing a retry attempt.
                self.discard_partial_chunk();
                self.transfer(url, cancel).await?;
            }
            other => other?,
        }
        self.status.set(ConnectionStatus::Finished);
        Ok(())
    }

    async fn transfer(&self, url: &Url, cancel: &CancellationToken) -> Result<()> {
        self.status.set(ConnectionStatus::SendingRequest);

        let range_start = self.range.start();
        let start = range_start + self.downloaded();
        if self.range.end() < start {
            return Ok(());
        }

        // Kept alive until the read is done: dropping it is what ends the transfer.
        let mut channel = FtpControlChannel::connect(url, self.credentials.as_ref(), cancel, Some(&self.proxy)).await?;
        let mut data = channel.open_read(&ftp_url::path_of(url), start, cancel).await?;

        self.status.set(ConnectionStatus::ReceivingData);

        let mut buffer = self.buffer.lock().await;
        if buffer.is_empty() {
            buffer.resize(self.settings.clamp_buffer_size(), 0);
        }
        let buffer = &mut buffer[..];

        // Open-or-create + seek rather than append: the same file is reopened on
        // every resume and must not be truncated, and the offset is derived from
        // what is already in it.
        let mut file = open_chunk(&self.chunk_file_path, start - range_start, false, buffer.len())?;
        let limiter = self.bandwidth_limiter.as_ref();

        let copy = async {
            loop {
                // Ask before reading, exactly as the HTTP connection does, so the
                // limiter caps what comes off the socket instead of sleeping off an
                // overshoot. The read is additionally clamped to what is left of the
                // segment, and reserved against a split — FTP would otherwise run on
                // to the end of the file and this chunk would overwrite its
                // neighbour's bytes.
                let granted = allowance(limiter, buffer.len(), cancel).await?;
                let wanted = self.range.reserve(range_start + self.downloaded(), granted.min(buffer.len()));
                let read = if wanted == 0 {
                    0
                } else {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                        read = data.read(&mut buffer[..wanted]) => read?,
                    }
                };
                if read == 0 {
                    give_back(limiter, granted);
                    return Ok(());
                }
                give_back(limiter, granted - read);
                file.write_all(&buffer[..read])?;
                self.bytes_downloaded.fetch_add(read as i64, Ordering::SeqCst);
            }
        };
        let result = copy.await;
        self.range.release(range_start + self.downloaded());
        result?;
        file.flush()?;
        drop(file);
        drop(data);
        drop(channel);

        // A stream that ended early left the segment short. Reported as a failure
        // so the retry policy gets a turn — silently accepting it would hand the
        // merger a chunk full of zeroes at the tail.
        let remaining = self.range.end() - (range_start + self.downloaded()) + 1;
        if remaining > 0 && !self.is_unbounded() {
            return Err(CoreError::Io(format!("The FTP data connection ended {remaining} bytes early.")));
        }
        Ok(())
    }

    fn discard_partial_chunk(&self) {
        self.bytes_downloaded.store(0, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.chunk_file_path);
    }
}

#[async_trait]
impl crate::traits::DownloadConnection for FtpDownloadConnection {
    fn connection_id(&self) -> i32 {
        self.connection_id
    }
    fn range_start(&self) -> i64 {
        self.range.start()
    }
    fn range_end(&self) -> i64 {
        self.range.end()
    }
    fn bytes_downloaded(&self) -> i64 {
        self.downloaded()
    }
    fn retry_count(&self) -> i32 {
        self.retry_count.load(Ordering::SeqCst)
    }
    fn status(&self) -> ConnectionStatus {
        self.status.get()
    }
    fn last_error(&self) -> Option<String> {
        self.last_error.lock().clone()
    }
    fn chunk_file_path(&self) -> &str {
        &self.chunk_file_path
    }

    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        if self.downloaded() >= self.range.len() {
            self.status.set(ConnectionStatus::Finished);
            return Ok(());
        }
        let url = parse_url(&self.url)?;

        let result = RetryPolicy::from_settings(&self.settings)
            .execute(
                |_| self.download_once(&url, &cancel),
                |attempt, error| {
                    self.retry_count.store(attempt, Ordering::SeqCst);
                    *self.last_error.lock() = Some(error.to_string());
                    self.status.set(ConnectionStatus::SendingRequest);
                },
                &cancel,
            )
            .await;

        if let Err(error) = &result
            && !error.is_cancelled()
        {
            // Said here, not only to the pool: the details window lists this
            // connection, and a status still reading "retrying" would be a lie.
            *self.last_error.lock() = Some(error.to_string());
            self.status.set(ConnectionStatus::Failed);
        }
        *self.buffer.lock().await = Vec::new();
        result
    }

    fn as_resizable(&self) -> Option<&dyn ResizableConnection> {
        Some(self)
    }
}

impl ResizableConnection for FtpDownloadConnection {
    fn committed_end(&self) -> i64 {
        self.range.committed_end(self.range.start() + self.downloaded())
    }
    fn try_shrink_end(&self, new_end: i64) -> bool {
        self.range.try_shrink_end(new_end, self.range.start() + self.downloaded())
    }
}
