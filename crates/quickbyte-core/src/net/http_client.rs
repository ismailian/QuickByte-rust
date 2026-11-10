//! The engine's one shared, pooled HTTP client — `HttpConnectionFactory.SharedClient`
//! in the C# build — plus the small request/response shapes the rest of `net/`
//! passes around.
//!
//! One client for every connection of every download, the probe, the playlist
//! reader and the update check: a client per connection is how a download
//! manager exhausts its sockets. It is built on native-tls (SChannel, the
//! Windows certificate store, as .NET used), asks [`AppProxy`] for every request,
//! sends no `Accept-Encoding` and decompresses nothing (a download's bytes are the
//! bytes on the wire, and a `Range` means an offset into them), and keeps no
//! cookie jar: the browser extension hands over the page's own `Cookie` header,
//! and a jar injecting its own would leave two cookie sources fighting over one
//! request.
//!
//! Timeouts follow the C# clients: none on the client itself (a 30 GB download
//! is not a timeout), and a per-request budget for the headers where the C#
//! code had one (the probe and the update check: 30 s).
//!
//! The client is rebuilt — cheaply, and only then — when Options changes the
//! proxy, so a connection pooled under the old proxy is never reused under the
//! new one. Requests already in flight keep the client they started on.

use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::TryStreamExt;
use parking_lot::Mutex;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::{CoreError, Result};
use crate::helpers::app_user_agent;
use crate::models::DownloadSettings;
use crate::net::app_proxy::AppProxy;

/// QuickByte's own User-Agent. The value actually sent is
/// [`app_user_agent::current`], written per request by the decorator — this is
/// the client-wide backstop, and it is a fixed string on purpose: re-stamping
/// the defaults when Options changes would race every request in flight.
pub const USER_AGENT: &str = app_user_agent::DEFAULT;

/// How many redirects are followed, as `HttpClientHandler.MaxAutomaticRedirections`.
pub const MAX_REDIRECTS: usize = 50;

/// The shared client and the proxy it routes by.
pub struct HttpClient {
    proxy: Arc<AppProxy>,
    client: Mutex<(u64, reqwest::Client)>,
}

impl HttpClient {
    /// The process-wide client, routed by [`AppProxy::shared`].
    pub fn shared() -> Arc<HttpClient> {
        static SHARED: OnceLock<Arc<HttpClient>> = OnceLock::new();
        SHARED.get_or_init(|| HttpClient::new(Arc::clone(AppProxy::shared()))).clone()
    }

    /// A client of its own, routed by `proxy`. For tests, and for nothing else:
    /// production has exactly one client, and it is shared on purpose.
    pub fn new(proxy: Arc<AppProxy>) -> Arc<HttpClient> {
        let generation = proxy.generation();
        let client = build(&proxy);
        Arc::new(Self { proxy, client: Mutex::new((generation, client)) })
    }

    pub fn proxy(&self) -> &Arc<AppProxy> {
        &self.proxy
    }

    /// The `reqwest` client for the proxy settings in force right now.
    pub fn reqwest(&self) -> reqwest::Client {
        let generation = self.proxy.generation();
        let mut slot = self.client.lock();
        if slot.0 != generation {
            *slot = (generation, build(&self.proxy));
        }
        slot.1.clone()
    }

    /// Sends `request` as it stands: no login handshakes (see
    /// [`crate::net::authenticated_http`] for those), redirects followed.
    pub async fn send(&self, request: &HttpRequest, cancel: &CancellationToken) -> Result<HttpResponse> {
        let builder = self.reqwest().request(request.method.clone(), request.url.clone()).headers(request.headers.clone());
        let pending = async {
            let response = builder.send().await.map_err(|e| map_reqwest_error(&e, &self.proxy))?;
            Ok(HttpResponse::from_reqwest(response))
        };
        with_deadline(pending, request.timeout, cancel).await
    }
}

fn build(proxy: &Arc<AppProxy>) -> reqwest::Client {
    reqwest::Client::builder()
        .proxy(proxy.reqwest_proxy())
        .user_agent(USER_AGENT)
        .redirect(redirect_policy())
        // HttpClient never sent a Referer of its own on a redirect, and a
        // captured one from the browser must not be replaced by ours.
        .referer(false)
        .pool_max_idle_per_host((DownloadSettings::MAX_CONNECTIONS + 4) as usize)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .build()
        .expect("the HTTP client's configuration is static and valid")
}

/// Redirects as `HttpClientHandler` followed them: up to fifty, and never from
/// HTTPS down to plain HTTP (that answer is returned as it is, and fails as the
/// non-success it is). reqwest drops `Authorization` and `Cookie` when a
/// redirect leaves the origin, which is the rule this engine holds a login to
/// anyway: it is only ever sent to the server it was typed for.
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        let downgrade = attempt.previous().last().is_some_and(|p| p.scheme() == "https") && attempt.url().scheme() == "http";
        if downgrade { attempt.stop() } else { attempt.follow() }
    })
}

/// Runs `future` with an optional deadline, stopping early on `cancel`.
pub(crate) async fn with_deadline<T>(
    future: impl std::future::Future<Output = Result<T>>,
    timeout: Option<Duration>,
    cancel: &CancellationToken,
) -> Result<T> {
    let timed = async {
        match timeout {
            Some(limit) => tokio::time::timeout(limit, future).await.map_err(|_| CoreError::Timeout)?,
            None => future.await,
        }
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(CoreError::Cancelled),
        result = timed => result,
    }
}

/// What every request is: a method, an address, headers, and how long its
/// headers may take to come back. Built once and sent as many times as a login
/// handshake needs, since a sent request cannot be sent again.
#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    /// Until the response headers arrive; `None` for no limit.
    pub timeout: Option<Duration>,
}

impl HttpRequest {
    pub fn new(method: Method, url: Url) -> Self {
        Self { method, url, headers: HeaderMap::new(), timeout: None }
    }
    pub fn get(url: Url) -> Self {
        Self::new(Method::GET, url)
    }
    pub fn head(url: Url) -> Self {
        Self::new(Method::HEAD, url)
    }

    /// Sets a header, replacing any value it had. A value that cannot be a
    /// header (a control character) is not sent.
    pub fn set_header(&mut self, name: HeaderName, value: &str) {
        if let Ok(value) = HeaderValue::from_str(value) {
            self.headers.insert(name, value);
        }
    }
}

/// Parses an absolute web address the way `new Uri(url)` accepted it.
pub fn parse_url(url: &str) -> Result<Url> {
    Url::parse(url.trim()).map_err(|_| CoreError::invalid(format!("'{url}' is not a valid address.")))
}

/// A body as bytes, read as they arrive.
pub type BodyReader = Pin<Box<dyn AsyncRead + Send>>;

enum Body {
    Reqwest(reqwest::Response),
    Stream(BodyReader),
}

/// A response whose headers have arrived and whose body has not been read —
/// `HttpCompletionOption.ResponseHeadersRead`, always. Dropping it is what stops
/// a server that is sending the whole file when it was asked for one byte.
pub struct HttpResponse {
    status: StatusCode,
    headers: HeaderMap,
    url: Url,
    body: Body,
}

impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpResponse").field("status", &self.status).field("url", &self.url.as_str()).finish_non_exhaustive()
    }
}

impl HttpResponse {
    fn from_reqwest(response: reqwest::Response) -> Self {
        Self { status: response.status(), headers: response.headers().clone(), url: response.url().clone(), body: Body::Reqwest(response) }
    }

    pub(crate) fn from_parts(status: StatusCode, headers: HeaderMap, url: Url, body: BodyReader) -> Self {
        Self { status, headers, url, body: Body::Stream(body) }
    }

    /// Keeps `guard` alive for as long as the body is (the connection a
    /// signed-in response streams over).
    pub(crate) fn holding(self, guard: Box<dyn std::any::Any + Send>) -> Self {
        let Self { status, headers, url, body } = self;
        let reader = match body {
            Body::Reqwest(response) => Self::from_reqwest(response).into_reader(),
            Body::Stream(reader) => reader,
        };
        Self { status, headers, url, body: Body::Stream(Box::pin(Guarded { reader, _guard: guard })) }
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }
    /// The address that answered, after redirects.
    pub fn url(&self) -> &Url {
        &self.url
    }
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// The first value of `name`, if it is text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Every value of `name`, as text.
    pub fn header_values<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers.get_all(name).iter().filter_map(|v| v.to_str().ok())
    }

    pub fn content_length(&self) -> Option<i64> {
        self.header("content-length").and_then(|v| v.trim().parse::<i64>().ok())
    }

    /// `EnsureSuccessStatusCode`, with its message.
    pub fn error_for_status(self) -> Result<Self> {
        if self.status.is_success() {
            return Ok(self);
        }
        Err(status_error(self.status))
    }

    /// The body as an `AsyncRead`.
    pub fn into_reader(self) -> BodyReader {
        match self.body {
            Body::Reqwest(response) => {
                let stream = response.bytes_stream().map_err(|e| std::io::Error::other(error_chain(&e)));
                Box::pin(tokio_util::io::StreamReader::new(stream))
            }
            Body::Stream(reader) => reader,
        }
    }

    /// The whole body, or `None` if it holds more than `limit` bytes.
    pub async fn read_at_most(self, limit: usize) -> Result<Option<Vec<u8>>> {
        let mut reader = self.into_reader();
        let mut buffer = Vec::new();
        let mut chunk = vec![0u8; 81_920];
        loop {
            let read = reader.read(&mut chunk).await?;
            if read == 0 {
                return Ok(Some(buffer));
            }
            if buffer.len() + read > limit {
                return Ok(None);
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

/// A body reader that keeps something else alive while it is read.
struct Guarded {
    reader: BodyReader,
    _guard: Box<dyn std::any::Any + Send>,
}

impl AsyncRead for Guarded {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.get_mut().reader.as_mut().poll_read(cx, buf)
    }
}

/// The message `HttpResponseMessage.EnsureSuccessStatusCode` throws with.
pub fn status_error(status: StatusCode) -> CoreError {
    CoreError::http(
        Some(status.as_u16()),
        format!(
            "Response status code does not indicate success: {} ({}).",
            status.as_u16(),
            status.canonical_reason().unwrap_or("Unknown")
        ),
    )
}

/// An error's message and every cause under it.
pub(crate) fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let part = cause.to_string();
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        source = cause.source();
    }
    text
}

/// Turns a transport failure into the engine's error. A proxy refusing its
/// login is said as the proxy's (never retried, never a site sign-in prompt);
/// anything else is an HTTP failure worth retrying.
pub(crate) fn map_reqwest_error(error: &reqwest::Error, proxy: &AppProxy) -> CoreError {
    if error.is_timeout() {
        return CoreError::Timeout;
    }
    let text = error_chain(error);
    let lower = text.to_ascii_lowercase();
    let has_login = matches!(proxy.login(), crate::net::app_proxy::ProxyLogin::Manual(_));
    if lower.contains("proxy authorization required") || lower.contains("server does not support user/pass") {
        return CoreError::ProxyAuthentication(
            if has_login { CoreError::PROXY_AUTH_REJECTED } else { CoreError::PROXY_AUTH_NEEDED }.into(),
        );
    }
    if lower.contains("credentials not accepted") {
        return CoreError::ProxyAuthentication(CoreError::PROXY_AUTH_REJECTED.into());
    }
    if lower.contains("server implements authentication incorrectly") {
        return CoreError::ProxyAuthentication(CoreError::PROXY_AUTH_NEEDED.into());
    }
    CoreError::http(None, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_success_reads_as_dotnet_said_it() {
        assert_eq!(
            status_error(StatusCode::INTERNAL_SERVER_ERROR).to_string(),
            "Response status code does not indicate success: 500 (Internal Server Error)."
        );
    }

    #[test]
    fn the_client_follows_a_proxy_change_with_a_fresh_pool() {
        let proxy = Arc::new(AppProxy::new(None));
        let client = HttpClient::new(Arc::clone(&proxy));
        let _ = client.reqwest();
        let before = client.client.lock().0;
        proxy.configure(None);
        let _ = client.reqwest();
        assert_eq!(client.client.lock().0, before + 1);
    }

    #[tokio::test]
    async fn a_deadline_is_a_timeout_and_a_cancel_is_a_cancel() {
        let never = std::future::pending::<Result<()>>();
        let result = with_deadline(never, Some(Duration::from_millis(10)), &CancellationToken::new()).await;
        assert!(matches!(result, Err(CoreError::Timeout)));

        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = with_deadline(std::future::pending::<Result<()>>(), None, &cancel).await;
        assert!(matches!(result, Err(CoreError::Cancelled)));
    }
}
