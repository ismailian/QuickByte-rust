//! Port of `AuthenticatedHttpTests.cs`: servers that want more than Basic.
//! Digest is checked end to end against a server that really verifies the
//! response hash; NTLM, which needs a Windows account to finish, is checked as
//! far as the client opening the handshake with it.

use std::sync::Arc;

use md5::{Digest, Md5};
use parking_lot::Mutex;
use quickbyte_core::CoreError;
use quickbyte_core::models::{DownloadCredentials, DownloadSettings, RequestOptions};
use quickbyte_core::net::authenticated_http;
use quickbyte_core::net::download_connection::DownloadConnection;
use quickbyte_core::net::http_client::{HttpRequest, parse_url};
use quickbyte_core::net::remote_file_info_provider::RemoteFileInfoProvider;
use quickbyte_core::traits::{ConnectionSpec, DownloadConnection as _, RemoteFileInfoProvider as _};
use tokio_util::sync::CancellationToken;

use crate::support::{Asked, Reply, TempFolder, TestServer, direct_client};

const REALM: &str = "files";
const NONCE: &str = "dcd98b7102dd2f0e8b11d0f600bfb0c093";

fn file() -> Vec<u8> {
    (0..5000).map(|i| (i % 251) as u8).collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Digest,
    Ntlm,
}

/// An HTTP/1.1 server with keep-alive that serves one file by ranges, behind
/// Digest (verified: RFC 7616, MD5, qop=auth) or behind an NTLM challenge it
/// cannot finish.
struct AuthServer {
    server: TestServer,
    authorizations: Arc<Mutex<Vec<Option<String>>>>,
}

fn md5_hex(text: &str) -> String {
    Md5::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn digest_fields(header: &str) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    let mut rest = header;
    while let Some(eq) = rest.find('=') {
        let name = rest[..eq].trim().trim_start_matches(',').trim().to_string();
        rest = &rest[eq + 1..];
        let value;
        if let Some(stripped) = rest.strip_prefix('"') {
            let close = stripped.find('"').unwrap_or(stripped.len());
            value = stripped[..close].to_string();
            rest = &stripped[(close + 1).min(stripped.len())..];
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            value = rest[..end].trim().to_string();
            rest = &rest[end..];
        }
        fields.push((name, value));
    }
    fields
}

fn authorized(mode: Mode, user: &str, password: &str, method: &str, authorization: Option<&str>) -> bool {
    let Some(header) = authorization.filter(|a| a.starts_with("Digest ")) else { return false };
    if mode == Mode::Ntlm {
        return false;
    }
    let fields = digest_fields(&header[7..]);
    let get = |name: &str| fields.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone()).unwrap_or_default();
    if get("username") != user || get("nonce") != NONCE {
        return false;
    }
    let ha1 = md5_hex(&format!("{user}:{REALM}:{password}"));
    let ha2 = md5_hex(&format!("{method}:{}", get("uri")));
    let expected = md5_hex(&format!("{ha1}:{NONCE}:{}:{}:{}:{ha2}", get("nc"), get("cnonce"), get("qop")));
    expected.eq_ignore_ascii_case(&get("response"))
}

impl AuthServer {
    async fn start(mode: Mode, user: &'static str, password: &'static str) -> Self {
        let authorizations = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&authorizations);
        let server = TestServer::start(move |asked: &Asked, _| {
            let authorization = asked.header("Authorization");
            seen.lock().push(authorization.clone());
            if !authorized(mode, user, password, &asked.method, authorization.as_deref()) {
                return match mode {
                    Mode::Digest => Reply::status(401)
                        .header("WWW-Authenticate", &format!("Digest realm=\"{REALM}\", qop=\"auth\", nonce=\"{NONCE}\", algorithm=MD5"))
                        .header("WWW-Authenticate", &format!("Basic realm=\"{REALM}\"")),
                    Mode::Ntlm => Reply::status(401).header("WWW-Authenticate", "NTLM"),
                };
            }
            let data = file();
            let (mut from, mut to) = (0usize, data.len() - 1);
            let mut reply = Reply::status(200);
            if let Some(range) = asked.header("Range").and_then(|r| r.strip_prefix("bytes=").map(String::from)) {
                let (a, b) = range.split_once('-').unwrap();
                from = a.parse().unwrap();
                if !b.is_empty() {
                    to = to.min(b.parse().unwrap());
                }
                reply = Reply::status(206).header("Content-Range", &format!("bytes {from}-{to}/{}", data.len()));
            }
            reply = reply.header("Accept-Ranges", "bytes");
            if asked.method == "HEAD" { reply.declared(data.len() as u64) } else { reply.with_body(&data[from..=to]) }
        })
        .await;
        Self { server, authorizations }
    }

    fn url(&self) -> String {
        self.server.url("/files/data.bin")
    }

    fn sent(&self) -> Vec<Option<String>> {
        self.authorizations.lock().clone()
    }
}

fn login(user: &str, password: &str) -> RequestOptions {
    RequestOptions { credentials: Some(DownloadCredentials::new(user, password)), ..Default::default() }
}

fn connection(url: &str, start: i64, end: i64, chunk: &str, max_retries: i32, options: RequestOptions) -> DownloadConnection {
    DownloadConnection::new(
        direct_client(),
        ConnectionSpec {
            connection_id: 0,
            url: url.to_string(),
            range_start: start,
            range_end: end,
            already_downloaded: 0,
            chunk_file_path: chunk.to_string(),
            settings: Arc::new(DownloadSettings { max_retries, retry_delay_milliseconds: 0, ..Default::default() }),
            bandwidth_limiter: None,
            options,
        },
    )
}

fn scheme(authorization: &Option<String>) -> Option<String> {
    authorization.as_ref().map(|a| a.split(' ').next().unwrap_or("").to_string())
}

#[tokio::test]
async fn a_digest_server_is_answered_and_the_segment_downloads_intact() {
    let server = AuthServer::start(Mode::Digest, "alice", "s3cret").await;
    let folder = TempFolder::new();

    connection(&server.url(), 1000, 2999, &folder.file("part0.tmp"), 0, login("alice", "s3cret")).run(CancellationToken::new()).await.unwrap();

    assert_eq!(std::fs::read(folder.file("part0.tmp")).unwrap(), file()[1000..3000]);
    // Basic first, as always; refused with a Digest challenge; then Digest.
    let sent = server.sent();
    assert_eq!(scheme(&sent[0]).as_deref(), Some("Basic"));
    assert_eq!(scheme(sent.last().unwrap()).as_deref(), Some("Digest"));
}

#[tokio::test]
async fn once_a_server_has_asked_for_digest_later_requests_go_straight_to_it() {
    let server = AuthServer::start(Mode::Digest, "bob", "pw").await;
    let folder = TempFolder::new();

    connection(&server.url(), 0, 99, &folder.file("a.tmp"), 0, login("bob", "pw")).run(CancellationToken::new()).await.unwrap();
    let basic = |sent: &[Option<String>]| sent.iter().filter(|a| a.as_deref().is_some_and(|a| a.starts_with("Basic"))).count();
    let before = basic(&server.sent());

    connection(&server.url(), 100, 199, &folder.file("b.tmp"), 0, login("bob", "pw")).run(CancellationToken::new()).await.unwrap();

    assert_eq!(basic(&server.sent()), before);
    assert_eq!(std::fs::read(folder.file("b.tmp")).unwrap(), file()[100..200]);
}

#[tokio::test]
async fn a_wrong_password_for_digest_is_still_a_rejected_password() {
    let server = AuthServer::start(Mode::Digest, "carol", "right").await;
    let folder = TempFolder::new();

    let error = connection(&server.url(), 0, 99, &folder.file("c.tmp"), 3, login("carol", "wrong")).run(CancellationToken::new()).await.unwrap_err();

    assert!(matches!(error, CoreError::AuthenticationRequired { credentials_were_supplied: true, .. }));
}

#[tokio::test]
async fn the_probe_signs_in_with_digest_too() {
    let server = AuthServer::start(Mode::Digest, "dave", "pw").await;
    let info = RemoteFileInfoProvider::with_client(direct_client()).get_file_info(&server.url(), &login("dave", "pw"), &CancellationToken::new()).await.unwrap();
    assert_eq!(info.content_length, 5000);
    assert!(info.supports_range_requests);
}

#[cfg(windows)]
#[tokio::test]
async fn an_ntlm_server_is_offered_the_ntlm_handshake_and_never_basic_again() {
    let server = AuthServer::start(Mode::Ntlm, "CORP\\erin", "pw").await;
    let folder = TempFolder::new();

    // The scripted server cannot complete NTLM — that needs a Windows account —
    // so the request ends refused. What matters is what the client sent.
    let error = connection(&server.url(), 0, 99, &folder.file("d.tmp"), 0, login("CORP\\erin", "pw")).run(CancellationToken::new()).await.unwrap_err();
    assert!(matches!(error, CoreError::AuthenticationRequired { .. }), "{error}");

    let sent: Vec<String> = server.sent().into_iter().flatten().collect();
    assert!(sent[0].starts_with("Basic"));
    // "TlRMTVNTUAAB" is base64 for NTLMSSP followed by message type 1, the negotiate message.
    assert!(sent[1..].iter().any(|a| a.starts_with("NTLM TlRMTVNTUAAB")), "{sent:?}");
    assert!(!sent[1..].iter().any(|a| a.starts_with("Basic")));
}

#[tokio::test]
async fn without_a_login_a_challenge_is_simply_the_answer() {
    let server = AuthServer::start(Mode::Ntlm, "x", "y").await;
    let request = HttpRequest::get(parse_url(&server.url()).unwrap());
    let response = authenticated_http::send(&direct_client(), &request, None, &CancellationToken::new()).await.unwrap();
    assert_eq!(response.status().as_u16(), 401);
    assert_eq!(server.sent().len(), 1);
}
