//! The loopback bridge the browser extension talks to: the token, the two
//! routes, CORS for extensions only, the size caps, and following Options.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use quickbyte_core::events::{EventHub, EventStream};
use quickbyte_core::models::DownloadSettings;
use quickbyte_core::services::browser_integration_server::BrowserIntegrationServer;
use quickbyte_core::traits::{BrowserEvent, BrowserIntegrationService, SettingsService};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Settings in memory, publishing every save as the real service does.
struct FakeSettings {
    current: Mutex<Arc<DownloadSettings>>,
    hub: EventHub<Arc<DownloadSettings>>,
    saves: Mutex<usize>,
}

impl FakeSettings {
    fn new(settings: DownloadSettings) -> Arc<Self> {
        Arc::new(Self { current: Mutex::new(Arc::new(settings)), hub: EventHub::default(), saves: Mutex::new(0) })
    }
}

impl SettingsService for FakeSettings {
    fn current(&self) -> Arc<DownloadSettings> {
        self.current.lock().clone()
    }
    fn subscribe(&self) -> EventStream<Arc<DownloadSettings>> {
        self.hub.subscribe()
    }
    fn load(&self) {}
    fn save(&self, settings: DownloadSettings) {
        let settings = Arc::new(settings);
        *self.current.lock() = Arc::clone(&settings);
        *self.saves.lock() += 1;
        self.hub.publish(settings);
    }
}

fn settings(port: i32, token: &str) -> DownloadSettings {
    DownloadSettings {
        browser_integration_enabled: true,
        browser_integration_port: port,
        browser_integration_token: token.into(),
        ..Default::default()
    }
}

async fn exchange(port: u16, request: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response)).await;
    String::from_utf8_lossy(&response).into_owned()
}

fn get(path: &str, token: Option<&str>, origin: Option<&str>) -> String {
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    if let Some(token) = token {
        request.push_str(&format!("X-QuickByte-Token: {token}\r\n"));
    }
    if let Some(origin) = origin {
        request.push_str(&format!("Origin: {origin}\r\n"));
    }
    request.push_str("\r\n");
    request
}

fn post(path: &str, token: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nx-quickbyte-token: {token}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

async fn started(token: &str) -> (Arc<FakeSettings>, BrowserIntegrationServer) {
    let fake = FakeSettings::new(settings(0, token));
    let server = BrowserIntegrationServer::in_current_runtime(fake.clone());
    server.start();
    assert!(server.is_running(), "{:?}", server.last_error());
    (fake, server)
}

fn drain(events: &EventStream<BrowserEvent>) -> Vec<BrowserEvent> {
    events.try_iter().collect()
}

#[tokio::test]
async fn a_paired_ping_names_the_app() {
    let (_, server) = started("secret-token").await;
    let response = exchange(server.port(), &get("/ping?x=1", Some("secret-token"), None)).await;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("Content-Type: application/json; charset=utf-8\r\n"));
    assert!(response.contains("Cache-Control: no-store\r\n"));
    assert!(response.contains("Connection: close\r\n"));
    assert!(response.contains(r#"{"ok":true,"app":"QuickByte","version":""#));
}

#[tokio::test]
async fn a_missing_or_wrong_token_is_refused() {
    let (_, server) = started("secret-token").await;
    for token in [None, Some("wrong"), Some("secret-tokenX"), Some("")] {
        let response = exchange(server.port(), &get("/ping", token, None)).await;
        assert!(response.starts_with("HTTP/1.1 401 Unauthorized\r\n"), "{token:?}: {response}");
        assert!(response.ends_with(r#"{"ok":false,"error":"pairing token missing or wrong"}"#));
    }
}

#[tokio::test]
async fn a_preflight_is_answered_for_extensions_only() {
    let (_, server) = started("t").await;
    let preflight = |origin: &str| format!("OPTIONS /download HTTP/1.1\r\nOrigin: {origin}\r\nAccess-Control-Request-Method: POST\r\n\r\n");

    let extension = exchange(server.port(), &preflight("chrome-extension://abcdefghijklmnop")).await;
    assert!(extension.starts_with("HTTP/1.1 204 No Content\r\n"));
    assert!(extension.contains("Access-Control-Allow-Origin: chrome-extension://abcdefghijklmnop\r\n"));
    assert!(extension.contains("Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n"));
    assert!(extension.contains("Access-Control-Allow-Headers: Content-Type, X-QuickByte-Token\r\n"));
    assert!(extension.contains("Access-Control-Max-Age: 600\r\n"));

    let firefox = exchange(server.port(), &preflight("moz-extension://1234")).await;
    assert!(firefox.contains("Access-Control-Allow-Origin: moz-extension://1234\r\n"));

    // An ordinary page's JSON POST never survives its preflight.
    let page = exchange(server.port(), &preflight("https://evil.example.com")).await;
    assert!(page.starts_with("HTTP/1.1 204"));
    assert!(!page.contains("Access-Control"));
}

#[tokio::test]
async fn a_captured_download_is_handed_over_with_what_the_browser_knew() {
    let (_, server) = started("t").await;
    let events = server.subscribe();
    let body = r#"{"url":"https://example.com/files/a.zip","fileName":"a.zip","fileSize":1234,"mimeType":"application/zip","referrer":"https://example.com/","userAgent":"Mozilla/5.0 (a browser)","cookie":"session=1"}"#;

    let response = exchange(server.port(), &post("/download", "t", body)).await;

    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.ends_with(r#"{"ok":true}"#));
    let captured: Vec<_> = drain(&events)
        .into_iter()
        .filter_map(|e| match e {
            BrowserEvent::DownloadCaptured(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(captured.len(), 1);
    let c = &captured[0];
    assert_eq!(c.url, "https://example.com/files/a.zip");
    assert_eq!(c.file_name.as_deref(), Some("a.zip"));
    assert_eq!(c.total_bytes, 1234);
    assert_eq!(c.cookie.as_deref(), Some("session=1"));
    assert_eq!(c.to_headers().get("User-Agent").map(String::as_str), Some("Mozilla/5.0 (a browser)"));
}

#[tokio::test]
async fn only_web_and_ftp_links_are_taken() {
    let (_, server) = started("t").await;
    let events = server.subscribe();

    let bad = exchange(server.port(), &post("/download", "t", r#"{"url":"file:///C:/Windows/win.ini"}"#)).await;
    assert!(bad.starts_with("HTTP/1.1 400 Bad Request"));
    assert!(bad.ends_with(r#"{"ok":false,"error":"unsupported url scheme"}"#));

    let relative = exchange(server.port(), &post("/download", "t", r#"{"url":"/relative/path"}"#)).await;
    assert!(relative.ends_with(r#"{"ok":false,"error":"a valid absolute url is required"}"#));

    let garbage = exchange(server.port(), &post("/download", "t", "not json")).await;
    assert!(garbage.starts_with("HTTP/1.1 400"));

    let ftp = exchange(server.port(), &post("/download", "t", r#"{"url":"ftp://ftp.example.com/a.iso"}"#)).await;
    assert!(ftp.starts_with("HTTP/1.1 200"));

    let captured = drain(&events).into_iter().filter(|e| matches!(e, BrowserEvent::DownloadCaptured(_))).count();
    assert_eq!(captured, 1);
}

#[tokio::test]
async fn an_unknown_route_or_method_is_not_found() {
    let (_, server) = started("t").await;
    assert!(exchange(server.port(), &get("/download", Some("t"), None)).await.starts_with("HTTP/1.1 404 Not Found"));
    assert!(exchange(server.port(), &get("/admin", Some("t"), None)).await.ends_with(r#"{"ok":false,"error":"no such route"}"#));
}

#[tokio::test]
async fn headers_over_the_cap_get_no_answer() {
    let (_, server) = started("t").await;
    let huge = format!("GET /ping HTTP/1.1\r\nX-Padding: {}\r\n\r\n", "a".repeat(10 * 1024));
    assert_eq!(exchange(server.port(), &huge).await, "");
}

#[tokio::test]
async fn the_token_is_minted_on_first_use_and_kept() {
    let fake = FakeSettings::new(settings(0, ""));
    let server = BrowserIntegrationServer::in_current_runtime(fake.clone());

    let token = server.token();

    assert_eq!(token.len(), 32);
    assert_eq!(fake.current().browser_integration_token, token);
    assert_eq!(server.token(), token);
    assert_eq!(*fake.saves.lock(), 1);

    let fresh = server.regenerate_token();
    assert_ne!(fresh, token);
    assert_eq!(fake.current().browser_integration_token, fresh);
}

#[tokio::test]
async fn a_disabled_bridge_does_not_listen() {
    let fake = FakeSettings::new(DownloadSettings { browser_integration_enabled: false, browser_integration_port: 0, ..Default::default() });
    let server = BrowserIntegrationServer::in_current_runtime(fake);
    server.start();
    assert!(!server.is_running());
}

#[tokio::test]
async fn a_port_already_in_use_is_reported_not_raised() {
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = squatter.local_addr().unwrap().port();
    let fake = FakeSettings::new(settings(port as i32, "t"));
    let server = BrowserIntegrationServer::in_current_runtime(fake);

    server.start();

    assert!(!server.is_running());
    assert!(server.last_error().is_some());
    assert_eq!(server.port(), port);
}

#[tokio::test]
async fn options_turn_the_bridge_off_and_on_again_live() {
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    let fake = FakeSettings::new(settings(port as i32, "t"));
    let server = BrowserIntegrationServer::in_current_runtime(fake.clone());
    server.start();
    assert!(server.is_running());
    assert_eq!(server.port(), port);

    let mut off = (*fake.current()).clone();
    off.browser_integration_enabled = false;
    fake.save(off);
    wait_until(|| !server.is_running()).await;

    let mut on = (*fake.current()).clone();
    on.browser_integration_enabled = true;
    fake.save(on);
    wait_until(|| server.is_running()).await;
    assert!(exchange(server.port(), &get("/ping", Some("t"), None)).await.starts_with("HTTP/1.1 200"));

    server.stop();
    assert!(!server.is_running());
}

async fn wait_until(condition: impl Fn() -> bool) {
    for _ in 0..100 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the bridge never followed the settings");
}
