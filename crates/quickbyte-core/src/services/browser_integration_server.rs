//! Port of `src/QuickByte.Core/Services/BrowserIntegrationServer.cs`.
//!
//! A very small HTTP server on 127.0.0.1 that the browser extension posts
//! captured downloads to. Two routes: `GET /ping` (so the extension can show
//! whether QuickByte is running and correctly paired) and `POST /download` (a
//! link to take over — a file, or a video playlist from the popup's *Videos on
//! this page*; both arrive the same way).
//!
//! Written on a raw TCP listener rather than http.sys: http.sys URL
//! reservations are an administrator-level concept, and a download manager that
//! needs an elevated prompt to talk to a browser extension is not shippable. A
//! socket bound to loopback needs no permission from anyone.
//!
//! Three things guard it, because anything running on the machine — including
//! every web page in the browser — can reach a loopback port:
//!
//! * **The bind address.** 127.0.0.1, so nothing off-machine can even connect.
//! * **The token.** Every request must carry `X-QuickByte-Token`, compared in
//!   fixed time. A page can send a request it cannot read the answer to, so the
//!   secret has to be in the request.
//! * **The origin.** CORS headers are only issued to `chrome-extension://` and
//!   `moz-extension://` origins, so an ordinary page cannot complete the
//!   preflight that a JSON POST requires.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use parking_lot::Mutex;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use crate::events::{EventHub, EventStream};
use crate::models::{CapturedDownload, DownloadSettings};
use crate::traits::{BrowserEvent, BrowserIntegrationService, SettingsService};

/// Caps on what one request may send, so a stray client can't exhaust memory.
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;

/// A pairing request that stalls holding a socket open is not worth waiting on.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

struct Running {
    cancel: CancellationToken,
    port: u16,
}

struct Inner {
    settings: Arc<dyn SettingsService>,
    runtime: tokio::runtime::Handle,
    events: EventHub<BrowserEvent>,
    running: Mutex<Option<Running>>,
    last_error: Mutex<Option<String>>,
}

pub struct BrowserIntegrationServer {
    inner: Arc<Inner>,
}

impl BrowserIntegrationServer {
    /// `runtime` is where the listener's tasks run; the server can be started and
    /// stopped from any thread (the UI's included).
    ///
    /// It watches the settings, and for the same reason the global speed limit
    /// does: a bridge you have to restart the app to enable is a bridge the user
    /// will assume is broken. Enabling it, disabling it or moving its port takes
    /// effect on Save.
    pub fn new(settings: Arc<dyn SettingsService>, runtime: tokio::runtime::Handle) -> Self {
        let inner = Arc::new(Inner {
            settings,
            runtime,
            events: EventHub::default(),
            running: Mutex::new(None),
            last_error: Mutex::new(None),
        });
        watch_settings(Arc::downgrade(&inner), inner.settings.subscribe());
        Self { inner }
    }

    /// For the composition root and the tests: the server on the current runtime.
    pub fn in_current_runtime(settings: Arc<dyn SettingsService>) -> Self {
        Self::new(settings, tokio::runtime::Handle::current())
    }

    /// Who to wake when a capture or a status change is published. Without it a
    /// capture waits in the subscriber's queue until something else happens to
    /// wake the UI — an idle window never shows the Add Download dialog.
    pub fn set_dispatcher(&self, dispatcher: Arc<dyn crate::events::Dispatcher>) {
        self.inner.events.set_dispatcher(dispatcher);
    }
}

impl Drop for BrowserIntegrationServer {
    fn drop(&mut self) {
        self.inner.stop();
    }
}

fn watch_settings(inner: Weak<Inner>, changes: EventStream<Arc<DownloadSettings>>) {
    let spawned = std::thread::Builder::new().name("qb-browser-settings".into()).spawn(move || {
        loop {
            match changes.recv_timeout(Duration::from_secs(1)) {
                Ok(settings) => match inner.upgrade() {
                    Some(inner) => inner.on_settings_changed(&settings),
                    None => return,
                },
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if inner.strong_count() == 0 {
                        return;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    drop(spawned);
}

/// 24 random bytes, base64url without padding.
fn new_token() -> String {
    let bytes: [u8; 24] = rand::random();
    URL_SAFE_NO_PAD.encode(bytes)
}

impl Inner {
    fn token(&self) -> String {
        let current = self.settings.current();
        if !current.browser_integration_token.is_empty() {
            return current.browser_integration_token.clone();
        }
        // Generated lazily, and saved as a copy of the current settings with only
        // this one changed, so no field this type doesn't know about is reset.
        let token = new_token();
        self.save_token(&token);
        token
    }

    fn save_token(&self, token: &str) {
        let mut settings = self.settings.create_copy();
        settings.browser_integration_token = token.to_string();
        self.settings.save(settings);
    }

    fn is_running(&self) -> bool {
        self.running.lock().is_some()
    }

    fn start(self: &Arc<Self>) {
        if !self.settings.current().browser_integration_enabled {
            return;
        }

        // Minted here, on the caller's thread, rather than lazily inside a
        // request handler: reading the token can persist a new one, and writing
        // settings.json from an accept-loop task would race the UI thread doing
        // the same.
        let _ = self.token();

        {
            let mut running = self.running.lock();
            if running.is_some() {
                return;
            }
            let port = self.settings.current().browser_integration_port.clamp(0, u16::MAX as i32) as u16;
            match self.bind(port) {
                Ok((listener, active_port)) => {
                    let cancel = CancellationToken::new();
                    *running = Some(Running { cancel: cancel.clone(), port: active_port });
                    *self.last_error.lock() = None;
                    let inner = Arc::clone(self);
                    self.runtime.spawn(async move { inner.accept_loop(listener, cancel).await });
                }
                Err(error) => {
                    // Almost always "port already in use" — another copy of a
                    // download manager, or something else squatting the port.
                    // Recorded rather than raised: the app is perfectly usable
                    // without the bridge, and Options shows the reason.
                    *self.last_error.lock() = Some(error.to_string());
                }
            }
        }
        self.events.publish(BrowserEvent::StatusChanged);
    }

    fn bind(&self, port: u16) -> std::io::Result<(tokio::net::TcpListener, u16)> {
        // A listener just stopped (Options moving the bridge off and back on)
        // closes when its accept task next runs; give it a moment rather than
        // reporting our own port as taken.
        let mut attempt = 0;
        let listener = loop {
            match std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)) {
                Ok(listener) => break listener,
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && port != 0 && attempt < 8 => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(e),
            }
        };
        listener.set_nonblocking(true)?;
        let active = listener.local_addr()?.port();
        let _guard = self.runtime.enter();
        Ok((tokio::net::TcpListener::from_std(listener)?, active))
    }

    fn stop(&self) {
        {
            let mut running = self.running.lock();
            let Some(current) = running.take() else { return };
            current.cancel.cancel();
        }
        self.events.publish(BrowserEvent::StatusChanged);
    }

    fn on_settings_changed(self: &Arc<Self>, settings: &DownloadSettings) {
        let should_run = settings.browser_integration_enabled;
        let active = self.running.lock().as_ref().map(|r| r.port);
        let port_moved = active.is_some_and(|p| i32::from(p) != settings.browser_integration_port);

        if active.is_some() && (!should_run || port_moved) {
            self.stop();
        }
        if should_run && !self.is_running() {
            self.start();
        }
    }

    async fn accept_loop(self: Arc<Self>, listener: tokio::net::TcpListener, cancel: CancellationToken) {
        loop {
            let accepted = tokio::select! {
                _ = cancel.cancelled() => return,
                accepted = listener.accept() => accepted,
            };
            // One bad accept must not take the bridge down.
            let Ok((client, _)) = accepted else { continue };
            let inner = Arc::clone(&self);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = cancel.cancelled() => {}
                    _ = tokio::time::timeout(REQUEST_TIMEOUT, inner.handle_client(client)) => {}
                }
            });
        }
    }

    async fn handle_client(&self, mut client: TcpStream) {
        // A half-spoken request, a client that hung up, a timeout. None of it is
        // worth surfacing — the extension retries by simply capturing the next
        // download.
        let Some(request) = read_request(&mut client).await else { return };
        let response = self.build_response(&request);
        let _ = client.write_all(response.as_bytes()).await;
        let _ = client.flush().await;
        let _ = client.shutdown().await;
    }

    fn build_response(&self, request: &ParsedRequest) -> String {
        let origin = request.header("origin").unwrap_or("");

        // Answered before the token check: a preflight is not allowed to carry
        // custom headers, so it could never present one.
        if request.method == "OPTIONS" {
            return respond(204, "", origin);
        }

        if !self.is_token_valid(request.header("x-quickbyte-token")) {
            return respond(401, r#"{"ok":false,"error":"pairing token missing or wrong"}"#, origin);
        }

        match (request.path.as_str(), request.method.as_str()) {
            ("/ping", "GET") => respond(200, &format!(r#"{{"ok":true,"app":"QuickByte","version":"{}"}}"#, assembly_version()), origin),
            ("/download", "POST") => self.capture(&request.body, origin),
            _ => respond(404, r#"{"ok":false,"error":"no such route"}"#, origin),
        }
    }

    fn is_token_valid(&self, presented: Option<&str>) -> bool {
        let Some(presented) = presented.filter(|p| !p.is_empty()) else { return false };
        // Fixed-time: the token is a bearer secret on a port every process on the
        // machine can reach, so it must not be discoverable one byte at a time.
        presented.as_bytes().ct_eq(self.token().as_bytes()).into()
    }

    fn capture(&self, body: &str, origin: &str) -> String {
        let Some(payload) = parse_capture(body) else {
            return respond(400, r#"{"ok":false,"error":"a valid absolute url is required"}"#, origin);
        };
        let Ok(url) = url::Url::parse(&payload.url) else {
            return respond(400, r#"{"ok":false,"error":"a valid absolute url is required"}"#, origin);
        };

        // Whitelisted rather than blacklisted: the URL ends up being fetched, and
        // file:// or a custom scheme has no business arriving over this bridge.
        if !matches!(url.scheme(), "http" | "https" | "ftp" | "ftps") {
            return respond(400, r#"{"ok":false,"error":"unsupported url scheme"}"#, origin);
        }

        self.events.publish(BrowserEvent::DownloadCaptured(payload));
        respond(200, r#"{"ok":true}"#, origin)
    }
}

impl BrowserIntegrationService for BrowserIntegrationServer {
    fn subscribe(&self) -> EventStream<BrowserEvent> {
        self.inner.events.subscribe()
    }

    fn is_running(&self) -> bool {
        self.inner.is_running()
    }

    /// The loopback port currently listened on, or the configured one when stopped.
    fn port(&self) -> u16 {
        match self.inner.running.lock().as_ref() {
            Some(running) => running.port,
            None => self.inner.settings.current().browser_integration_port.clamp(0, u16::MAX as i32) as u16,
        }
    }

    /// The shared secret the extension must send as `X-QuickByte-Token`.
    /// Generated on first use and persisted with the rest of the settings.
    fn token(&self) -> String {
        self.inner.token()
    }

    fn last_error(&self) -> Option<String> {
        self.inner.last_error.lock().clone()
    }

    /// Starts listening if the feature is enabled; a no-op when it already is.
    fn start(&self) {
        self.inner.start();
    }

    fn stop(&self) {
        self.inner.stop();
    }

    /// Issues a new token and persists it, revoking whatever was paired before.
    /// Every browser has to be re-paired afterwards — which is the point.
    fn regenerate_token(&self) -> String {
        let token = new_token();
        self.inner.save_token(&token);
        self.inner.events.publish(BrowserEvent::StatusChanged);
        token
    }
}

struct ParsedRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

impl ParsedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<ParsedRequest> {
    let mut buffer = vec![0u8; MAX_HEADER_BYTES];
    let mut filled = 0;

    // Read until the blank line that ends the header block. The body usually
    // arrives in the same packet, so whatever came with it is kept and counted
    // rather than re-read.
    let header_end = loop {
        if filled == buffer.len() {
            return None; // headers over the cap
        }
        let read = stream.read(&mut buffer[filled..]).await.ok()?;
        if read == 0 {
            return None;
        }
        filled += read;
        if let Some(end) = buffer[..filled].windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n").filter(|l| !l.is_empty());
    let request_line: Vec<&str> = lines.next()?.split(' ').collect();
    if request_line.len() < 2 {
        return None;
    }

    let mut headers = HashMap::new();
    for line in lines {
        let Some(colon) = line.find(':') else { continue };
        if colon == 0 {
            continue;
        }
        headers.insert(line[..colon].trim().to_ascii_lowercase(), line[colon + 1..].trim().to_string());
    }

    let body_start = header_end + 4;
    let content_length =
        headers.get("content-length").and_then(|v| v.parse::<i64>().ok()).map(|l| l.clamp(0, MAX_BODY_BYTES as i64) as usize).unwrap_or(0);

    let mut body = Vec::with_capacity(content_length);
    let carried = content_length.min(filled.saturating_sub(body_start));
    body.extend_from_slice(&buffer[body_start..body_start + carried]);
    while body.len() < content_length {
        let want = (content_length - body.len()).min(buffer.len());
        let read = stream.read(&mut buffer[..want]).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..read]);
    }

    Some(ParsedRequest {
        method: request_line[0].to_ascii_uppercase(),
        path: request_line[1].split('?').next().unwrap_or("").to_string(),
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// The wire shape of a `POST /download` body — `{url, fileName, fileSize,
/// mimeType, referrer, userAgent, cookie}`, as `background.js` sends it — read
/// with property names in any case, as the C# server read it.
fn parse_capture(body: &str) -> Option<CapturedDownload> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let object = value.as_object()?;
    let field = |name: &str| object.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v);
    let text = |name: &str| -> Result<Option<String>, ()> {
        match field(name) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(()),
        }
    };

    let url = text("url").ok()??;
    let file_size = match field("fileSize") {
        None | Some(serde_json::Value::Null) => 0,
        Some(serde_json::Value::Number(n)) => n.as_i64()?,
        Some(_) => return None,
    };
    Some(CapturedDownload {
        url,
        file_name: text("fileName").ok()?,
        total_bytes: file_size.max(0),
        mime_type: text("mimeType").ok()?,
        referrer: text("referrer").ok()?,
        user_agent: text("userAgent").ok()?,
        cookie: text("cookie").ok()?,
    })
}

fn respond(status: u16, json: &str, origin: &str) -> String {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        _ => "Not Found",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\n",
        json.len()
    );

    // CORS only for browser extensions. Without a matching allow-origin the
    // preflight fails, and a JSON POST from an ordinary page never gets sent at
    // all — which is the whole defence against a web page driving this.
    if is_extension_origin(origin) {
        response.push_str(&format!("Access-Control-Allow-Origin: {origin}\r\n"));
        response.push_str("Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n");
        response.push_str("Access-Control-Allow-Headers: Content-Type, X-QuickByte-Token\r\n");
        response.push_str("Access-Control-Max-Age: 600\r\n");
    }

    // Every response closes its connection: there is no keep-alive bookkeeping
    // here, and a capture is one request every few minutes at most.
    response.push_str("Connection: close\r\n\r\n");
    response.push_str(json);
    response
}

fn is_extension_origin(origin: &str) -> bool {
    let lower = origin.to_ascii_lowercase();
    lower.starts_with("chrome-extension://") || lower.starts_with("moz-extension://")
}

/// The four-part assembly version the C# server reported (`1.27.0.0`).
fn assembly_version() -> String {
    let numeric = crate::VERSION.split(['-', '+']).next().unwrap_or("0");
    let mut parts: Vec<&str> = numeric.split('.').collect();
    while parts.len() < 4 {
        parts.push("0");
    }
    parts[..4].join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capture_is_read_as_the_extension_sends_it() {
        let capture = parse_capture(
            r#"{"url":"https://example.com/a.zip","fileName":"a.zip","fileSize":1234,"mimeType":"application/zip","referrer":"https://example.com/","userAgent":"Mozilla/5.0","cookie":"s=1"}"#,
        )
        .unwrap();
        assert_eq!(
            capture,
            CapturedDownload {
                url: "https://example.com/a.zip".into(),
                file_name: Some("a.zip".into()),
                total_bytes: 1234,
                mime_type: Some("application/zip".into()),
                referrer: Some("https://example.com/".into()),
                user_agent: Some("Mozilla/5.0".into()),
                cookie: Some("s=1".into()),
            }
        );
    }

    #[test]
    fn names_are_read_in_any_case_and_a_negative_size_is_none() {
        let capture = parse_capture(r#"{"URL":"https://example.com/a","FILESIZE":-5}"#).unwrap();
        assert_eq!(capture.url, "https://example.com/a");
        assert_eq!(capture.total_bytes, 0);
    }

    #[test]
    fn a_body_that_is_not_a_capture_is_refused() {
        assert_eq!(parse_capture("not json"), None);
        assert_eq!(parse_capture(r#"{"fileName":"x"}"#), None);
        assert_eq!(parse_capture(r#"{"url":42}"#), None);
        assert_eq!(parse_capture(r#"{"url":"https://x/","fileSize":"big"}"#), None);
    }

    #[test]
    fn cors_is_offered_to_extensions_only() {
        assert!(respond(200, "{}", "chrome-extension://abc").contains("Access-Control-Allow-Origin: chrome-extension://abc\r\n"));
        assert!(respond(200, "{}", "moz-extension://abc").contains("Access-Control-Allow-Origin"));
        assert!(!respond(200, "{}", "https://evil.example.com").contains("Access-Control"));
        assert!(!respond(200, "{}", "").contains("Access-Control"));
    }

    #[test]
    fn tokens_are_url_safe_and_long_enough() {
        let token = new_token();
        assert_eq!(token.len(), 32);
        assert!(token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_ne!(token, new_token());
    }

    #[test]
    fn the_version_has_four_parts() {
        assert_eq!(assembly_version().split('.').count(), 4);
    }
}
