//! Port of `src/QuickByte.Core/Services/DownloadConnection.cs` and
//! `HttpConnectionFactory.cs`.
//!
//! Downloads exactly one byte range `[range_start, range_end]` into its own temp
//! chunk file using an HTTP Range request. Fully independent and unaware of
//! sibling connections — it only exposes its own thread-safe state, which the
//! pool manager polls on a timer to build aggregated progress (no per-byte event
//! chatter reaches the UI).

use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};

use async_trait::async_trait;
use parking_lot::Mutex;
use reqwest::StatusCode;
use reqwest::header::{IF_RANGE, RANGE};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::enums::ConnectionStatus;
use crate::error::{CoreError, Result};
use crate::helpers::file_version_check;
use crate::helpers::retry_policy::RetryPolicy;
use crate::models::{DownloadSettings, RequestOptions, authority};
use crate::net::http_client::{HttpClient, HttpRequest, HttpResponse, parse_url};
use crate::net::remote_file_info_provider::content_range_from;
use crate::net::request_decorator::{self, RangeCondition};
use crate::net::segment_range::SegmentRange;
use crate::net::authenticated_http;
use crate::traits::{BandwidthLimiter, ConnectionFactory, ConnectionSpec, ResizableConnection};

/// Factory: HTTP(S) [`DownloadConnection`] workers, all sharing the engine's one
/// pooled client (a client per connection is how sockets run out).
pub struct HttpConnectionFactory {
    http: Arc<HttpClient>,
}

impl Default for HttpConnectionFactory {
    fn default() -> Self {
        Self { http: HttpClient::shared() }
    }
}

impl HttpConnectionFactory {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_client(http: Arc<HttpClient>) -> Self {
        Self { http }
    }
}

impl ConnectionFactory for HttpConnectionFactory {
    fn create(&self, spec: ConnectionSpec) -> Arc<dyn crate::traits::DownloadConnection> {
        Arc::new(DownloadConnection::new(Arc::clone(&self.http), spec))
    }
}

/// `ConnectionStatus` as an atomic: written by the transfer, read by the pool's timer.
pub(crate) struct AtomicStatus(AtomicI32);

impl AtomicStatus {
    pub(crate) fn new(status: ConnectionStatus) -> Self {
        Self(AtomicI32::new(status as i32))
    }
    pub(crate) fn set(&self, status: ConnectionStatus) {
        self.0.store(status as i32, Ordering::SeqCst);
    }
    pub(crate) fn get(&self) -> ConnectionStatus {
        match self.0.load(Ordering::SeqCst) {
            1 => ConnectionStatus::SendingRequest,
            2 => ConnectionStatus::ReceivingData,
            3 => ConnectionStatus::Finished,
            4 => ConnectionStatus::Failed,
            5 => ConnectionStatus::Paused,
            _ => ConnectionStatus::Idle,
        }
    }
}

/// Asks the limiter (if any) for an allowance before a read.
pub(crate) async fn allowance(limiter: Option<&Arc<dyn BandwidthLimiter>>, desired: usize, cancel: &CancellationToken) -> Result<usize> {
    match limiter {
        // With no limit configured this returns the full buffer without touching a lock.
        None => Ok(desired),
        Some(limiter) => limiter.request(desired, cancel).await,
    }
}

pub(crate) fn give_back(limiter: Option<&Arc<dyn BandwidthLimiter>>, unused: usize) {
    if let Some(limiter) = limiter
        && unused > 0
    {
        limiter.give_back(unused);
    }
}

/// Opens a chunk file for writing at `offset` without truncating it: the chunk
/// is reopened on every resume, and the offset to carry on from is derived from
/// what is already in it. `truncate` empties it first.
pub(crate) fn open_chunk(path: &str, offset: i64, truncate: bool, buffer_size: usize) -> Result<BufWriter<fs::File>> {
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().write(true).create(true).truncate(false).open(path)?;
    if truncate {
        file.set_len(0)?;
    }
    file.seek(SeekFrom::Start(offset.max(0) as u64))?;
    Ok(BufWriter::with_capacity(buffer_size, file))
}

pub struct DownloadConnection {
    http: Arc<HttpClient>,
    connection_id: i32,
    url: String,
    /// The range, which a split may shorten while this runs — see [`SegmentRange`].
    range: SegmentRange,
    chunk_file_path: String,
    settings: Arc<DownloadSettings>,
    bandwidth_limiter: Option<Arc<dyn BandwidthLimiter>>,
    options: RequestOptions,
    bytes_downloaded: AtomicI64,
    // Atomic for the same reason the status is: written by the transfer and
    // read by the pool's report timer.
    retry_count: AtomicI32,
    status: AtomicStatus,
    last_error: Mutex<Option<String>>,
    buffer: tokio::sync::Mutex<Vec<u8>>,
}

impl DownloadConnection {
    pub fn new(http: Arc<HttpClient>, spec: ConnectionSpec) -> Self {
        let range = SegmentRange::new(spec.range_start, spec.range_end);
        let total = range.len();
        Self {
            http,
            connection_id: spec.connection_id,
            url: spec.url,
            range,
            chunk_file_path: spec.chunk_file_path,
            settings: spec.settings,
            bandwidth_limiter: spec.bandwidth_limiter,
            options: spec.options,
            bytes_downloaded: AtomicI64::new(spec.already_downloaded.clamp(0, total.max(0))),
            retry_count: AtomicI32::new(0),
            status: AtomicStatus::new(ConnectionStatus::Idle),
            last_error: Mutex::new(None),
            buffer: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    fn total_bytes(&self) -> i64 {
        self.range.len()
    }

    fn downloaded(&self) -> i64 {
        self.bytes_downloaded.load(Ordering::SeqCst)
    }

    /// True when this connection was handed no real end: the single-connection
    /// case for a file whose size the server never disclosed. It asks for an
    /// open-ended range and reads until the stream closes, so the
    /// short-transfer check at the end of a fetch has nothing to measure against.
    fn is_unbounded(&self) -> bool {
        self.range.is_unbounded()
    }

    async fn download_once(&self, cancel: &CancellationToken) -> Result<()> {
        self.status.set(ConnectionStatus::SendingRequest);

        // One buffer for the life of the connection: the hot loop allocates nothing.
        let mut buffer = self.buffer.lock().await;
        if buffer.is_empty() {
            buffer.resize(self.settings.clamp_buffer_size(), 0);
        }
        let buffer = &mut buffer[..];

        let range_start = self.range.start();
        let mut start = range_start + self.downloaded();

        let if_range = request_decorator::if_range_for(self.options.resume_validator.as_ref());
        let response = self.send_conditional_range(start, if_range, cancel).await?;

        // 200 instead of 206 means the server ignored the Range header and is
        // answering with the whole file from byte zero — the HTTP twin of an FTP
        // server refusing REST, and every bit as quiet about it. Writing that body
        // at this segment's offset is precisely what corrupts a resumed or
        // segmented download, so it is recovered from here rather than trusted.
        let range_honoured = response.status() == StatusCode::PARTIAL_CONTENT;

        // A 206 that starts somewhere else is the same failure wearing the right
        // status code. Only checked when the server said where it started.
        if range_honoured
            && let Some(from) = content_range_from(&response)
            && from != start
        {
            return Err(CoreError::Io(format!("The server answered a request for byte {start} with a range starting at {from}.")));
        }

        if !range_honoured && range_start > 0 {
            // A later segment cannot be salvaged from a whole-file response: the
            // pool split this download on the strength of a probe that said ranges
            // were supported, and the bytes this connection owns are not where
            // this response begins.
            return Err(CoreError::Io(format!(
                "The server ignored the request for bytes {start}-{} and answered with the whole file.",
                self.range.end()
            )));
        }

        // The first segment can be salvaged — the response starts exactly where its
        // chunk does. Throw the partial away and take the file from zero, the same
        // recovery the FTP connection performs when REST is refused.
        let restart_from_zero = !range_honoured && start > range_start;
        if restart_from_zero {
            self.bytes_downloaded.store(0, Ordering::SeqCst);
            start = range_start;
        }

        self.status.set(ConnectionStatus::ReceivingData);

        // Only when the partial was just disowned is the chunk emptied: an older,
        // longer chunk would otherwise leave a tail of stale bytes past the end of
        // the new content.
        let mut file = open_chunk(&self.chunk_file_path, start - range_start, restart_from_zero, buffer.len())?;
        let mut body = response.into_reader();
        let limiter = self.bandwidth_limiter.as_ref();

        let transfer = async {
            loop {
                // Ask before reading, not after: the allowance caps how much this
                // read may pull off the socket, so the connection never overshoots
                // the limit and then sleeps it off.
                let granted = allowance(limiter, buffer.len(), cancel).await?;

                // Clamped to what is left of the segment, and reserved against a
                // split: the pool may shorten this range while it reads, but never
                // inside a read already under way. A whole-file response accepted
                // above — or a server that simply sends more than it was asked for
                // — would otherwise run this chunk on over its neighbour's bytes.
                let wanted = self.range.reserve(range_start + self.downloaded(), granted.min(buffer.len()));
                let read = if wanted == 0 {
                    0
                } else {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                        read = body.read(&mut buffer[..wanted]) => read?,
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
        let result = transfer.await;
        self.range.release(range_start + self.downloaded());
        result?;
        file.flush()?;
        drop(file);

        // A stream that ended before the segment did leaves a short chunk, and
        // nothing downstream would notice: the pool sees a connection that ran to
        // the end of its loop, and the merge writes the hole into the final file.
        // Reported so the retry policy gets a turn — and because the bytes that did
        // arrive are still on disk, the retry carries on from them.
        let remaining = self.range.end() - (range_start + self.downloaded()) + 1;
        if remaining > 0 && !self.is_unbounded() {
            return Err(CoreError::Io(format!("The connection ended {remaining} bytes early.")));
        }

        self.status.set(ConnectionStatus::Finished);
        Ok(())
    }

    /// Asks for the rest of this segment, conditional on the file still being the
    /// version the download's chunks came from.
    ///
    /// `If-Range` is the one request header that turns "the file changed while
    /// this was paused" from silent corruption into an answer: a server holding a
    /// different version sends the whole new file with a 200 instead of the tail
    /// of it with a 206. That 200 is only meaningful from the host that issued the
    /// validator, though. Redirects are followed and carry the header along, so a
    /// mirror redirector can hand the request to a mirror that computes its own
    /// ETag for the identical file — and holding that mirror to another server's
    /// validator would restart downloads that were fine. Such a response is asked
    /// again without the condition, which is exactly how the request looked before
    /// the condition existed.
    async fn send_conditional_range(&self, start: i64, if_range: Option<RangeCondition>, cancel: &CancellationToken) -> Result<HttpResponse> {
        let conditional = if_range.is_some();
        let response = self.send_range(start, if_range, cancel).await?;
        if !conditional || response.status() == StatusCode::PARTIAL_CONTENT {
            return Ok(response);
        }

        let answered_by = authority(response.url());
        let issued_by = self.options.resume_validator.as_ref().and_then(|v| v.host.as_deref());
        drop(response);
        if file_version_check::same_host(answered_by.as_deref(), issued_by) {
            return Err(CoreError::RemoteFileChanged("The file on the server has changed since this download started.".into()));
        }
        self.send_range(start, None, cancel).await
    }

    async fn send_range(&self, start: i64, if_range: Option<RangeCondition>, cancel: &CancellationToken) -> Result<HttpResponse> {
        let mut request = HttpRequest::get(parse_url(&self.url)?);
        // An open-ended "bytes=start-" when there is no known end. The sentinel
        // written out as a literal 9223372036854775806 is a range a fair number of
        // servers answer with 416 instead of the rest of the file.
        let range = if self.is_unbounded() { format!("bytes={start}-") } else { format!("bytes={start}-{}", self.range.end()) };
        request.set_header(RANGE, &range);
        request_decorator::apply(&mut request.headers, Some(&self.options));
        if let Some(condition) = if_range {
            request.set_header(IF_RANGE, &condition.header_value());
        }

        // Through a signed-in connection of its own when the server wants Digest,
        // NTLM or Negotiate rather than the Basic the decorator sends.
        let response = authenticated_http::send(&self.http, &request, Some(&self.options), cancel).await?;

        // Separated from the generic failure below so the retry policy stops
        // rather than spending its attempts re-presenting the same rejected password.
        if response.status() == StatusCode::PROXY_AUTHENTICATION_REQUIRED {
            return Err(CoreError::ProxyAuthentication(CoreError::PROXY_AUTH_NEEDED.into()));
        }
        if response.status() == StatusCode::UNAUTHORIZED {
            let supplied = self.options.has_credentials();
            return Err(CoreError::auth(
                if supplied {
                    "The server rejected the supplied user name or password."
                } else {
                    "The server requires a user name and password."
                },
                supplied,
            ));
        }
        response.error_for_status()
    }
}

#[async_trait]
impl crate::traits::DownloadConnection for DownloadConnection {
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
        if self.downloaded() >= self.total_bytes() {
            self.status.set(ConnectionStatus::Finished);
            return Ok(());
        }

        let result = RetryPolicy::from_settings(&self.settings)
            .execute(
                |_| self.download_once(&cancel),
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
        // The buffer is the connection's largest allocation; a finished or failed
        // connection does not need to keep it.
        *self.buffer.lock().await = Vec::new();
        result
    }

    fn as_resizable(&self) -> Option<&dyn ResizableConnection> {
        Some(self)
    }
}

impl ResizableConnection for DownloadConnection {
    fn committed_end(&self) -> i64 {
        self.range.committed_end(self.range.start() + self.downloaded())
    }
    fn try_shrink_end(&self, new_end: i64) -> bool {
        self.range.try_shrink_end(new_end, self.range.start() + self.downloaded())
    }
}
