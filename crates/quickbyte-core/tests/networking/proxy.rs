//! Port of `Proxy/ProxyTests.cs`: whether a change in Options reaches a client
//! created long before it, and the three handshakes FTP tunnels through,
//! checked byte for byte against scripted proxies.

use std::sync::Arc;

use base64::Engine;
use quickbyte_core::CoreError;
use quickbyte_core::ftp::control_channel::FtpControlChannel;
use quickbyte_core::models::{ProxyKind, ProxyMode, ProxySettings};
use quickbyte_core::net::app_proxy::{AppProxy, ProxyCredential, ProxyEndpoint};
use quickbyte_core::net::http_client::{HttpClient, HttpRequest, parse_url};
use quickbyte_core::net::proxy_tunnel;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use crate::support::{FakeFtpServer, FakeSocks5Proxy, FtpBehaviour, ScriptedServer, tiny_http_server};

const GREETING: &[u8] = b"220 ready\r\n";

fn endpoint(kind: ProxyKind, port: u16, login: Option<(&str, &str)>) -> ProxyEndpoint {
    ProxyEndpoint {
        kind,
        host: "127.0.0.1".into(),
        port,
        credentials: login.map(|(u, p)| ProxyCredential { user_name: u.into(), password: p.into() }),
    }
}

async fn first_line(stream: &mut TcpStream) -> String {
    let mut buffer = vec![0u8; GREETING.len()];
    stream.read_exact(&mut buffer).await.unwrap();
    String::from_utf8(buffer).unwrap()
}

fn manual_socks5(port: u16, user: &str, password: &str) -> ProxySettings {
    ProxySettings {
        mode: ProxyMode::Manual,
        kind: ProxyKind::Socks5,
        host: "127.0.0.1".into(),
        port: port as i32,
        user_name: user.into(),
        password: password.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_client_created_once_follows_the_proxy_from_one_request_to_the_next() {
    // The engine's client is shared; a proxy changed in Options must reach it
    // without a restart, credentials included.
    let site = tiny_http_server("hello").await;
    let first = FakeSocks5Proxy::start(Some(("alice", "one"))).await;
    let second = FakeSocks5Proxy::start(Some(("bob", "two"))).await;

    let proxy = Arc::new(AppProxy::new(None));
    let client = HttpClient::new(Arc::clone(&proxy));
    let url = parse_url(&format!("http://files.test:{}/a.txt", site.port)).unwrap();
    let get = || async {
        let response = client.send(&HttpRequest::get(url.clone()), &CancellationToken::new()).await.unwrap();
        String::from_utf8(response.read_at_most(1024).await.unwrap().unwrap()).unwrap()
    };

    proxy.configure(Some(&manual_socks5(first.port, "alice", "one")));
    assert_eq!(get().await, "hello");

    proxy.configure(Some(&manual_socks5(second.port, "bob", "two")));
    assert_eq!(get().await, "hello");

    // The name went to the proxy to resolve; it resolves nowhere else.
    assert_eq!(*first.requests.lock(), vec![("files.test".to_string(), site.port)]);
    assert_eq!(*second.requests.lock(), vec![("files.test".to_string(), site.port)]);
    assert_eq!(*second.logins.lock(), vec!["bob:two".to_string()]);
}

#[tokio::test]
async fn a_socks5_proxy_that_refuses_its_login_is_the_proxys_problem() {
    let socks = FakeSocks5Proxy::start(Some(("alice", "right"))).await;
    let proxy = Arc::new(AppProxy::new(None));
    proxy.configure(Some(&manual_socks5(socks.port, "alice", "wrong")));
    let client = HttpClient::new(Arc::clone(&proxy));

    let error = client.send(&HttpRequest::get(parse_url("http://files.test:1/a").unwrap()), &CancellationToken::new()).await.unwrap_err();

    assert!(matches!(&error, CoreError::ProxyAuthentication(m) if m == CoreError::PROXY_AUTH_REJECTED), "{error}");
}

#[tokio::test]
async fn socks5_sends_the_host_name_for_the_proxy_to_resolve_and_reads_no_further_than_its_reply() {
    let server = ScriptedServer::start(|mut s| async move {
        assert_eq!(ScriptedServer::read(&mut s, 3).await, vec![5, 1, 0]);
        s.write_all(&[5, 0]).await.unwrap();
        let request = ScriptedServer::read(&mut s, 5 + "ftp.example.com".len() + 2).await;
        assert_eq!(&request[..5], &[5, 1, 0, 3, 15]);
        assert_eq!(&request[5..20], b"ftp.example.com");
        assert_eq!(&request[20..], &[0, 21]);
        // The reply and the FTP greeting in one write, as a fast link delivers them.
        let mut reply = vec![5, 0, 0, 3, 4, b'n', b'a', b'm', b'e', 0, 21];
        reply.extend_from_slice(GREETING);
        s.write_all(&reply).await.unwrap();
    })
    .await;

    let mut stream = proxy_tunnel::connect("ftp.example.com", 21, Some(&endpoint(ProxyKind::Socks5, server.port, None))).await.unwrap();
    assert_eq!(first_line(&mut stream).await, "220 ready\r\n");
    server.completion.await.unwrap();
}

#[tokio::test]
async fn socks5_signs_in_when_the_proxy_asks() {
    let server = ScriptedServer::start(|mut s| async move {
        assert_eq!(ScriptedServer::read(&mut s, 4).await, vec![5, 2, 0, 2]);
        s.write_all(&[5, 2]).await.unwrap();
        let login = ScriptedServer::read(&mut s, 3 + 5 + 6).await;
        assert_eq!(&login[..2], &[1, 5]);
        assert_eq!(&login[2..7], b"alice");
        assert_eq!(login[7], 6);
        assert_eq!(&login[8..14], b"s3cret");
        s.write_all(&[1, 0]).await.unwrap();
        ScriptedServer::read(&mut s, 10).await; // CONNECT to an IPv4 address
        let mut reply = vec![5, 0, 0, 1, 1, 2, 3, 4, 0, 21];
        reply.extend_from_slice(GREETING);
        s.write_all(&reply).await.unwrap();
    })
    .await;

    let mut stream = proxy_tunnel::connect("192.0.2.10", 21, Some(&endpoint(ProxyKind::Socks5, server.port, Some(("alice", "s3cret"))))).await.unwrap();
    assert_eq!(first_line(&mut stream).await, "220 ready\r\n");
    server.completion.await.unwrap();
}

#[tokio::test]
async fn a_rejected_socks5_login_is_a_proxy_login_failure_not_a_network_one() {
    let server = ScriptedServer::start(|mut s| async move {
        ScriptedServer::read(&mut s, 4).await;
        s.write_all(&[5, 2]).await.unwrap();
        ScriptedServer::read(&mut s, 3 + 5 + 5).await;
        s.write_all(&[1, 1]).await.unwrap();
    })
    .await;

    let error = proxy_tunnel::connect("ftp.example.com", 21, Some(&endpoint(ProxyKind::Socks5, server.port, Some(("alice", "wrong"))))).await.unwrap_err();
    assert!(matches!(&error, CoreError::ProxyAuthentication(m) if m == CoreError::PROXY_AUTH_REJECTED));
}

#[tokio::test]
async fn a_socks5_proxy_that_wants_a_login_nobody_gave_says_so() {
    let server = ScriptedServer::start(|mut s| async move {
        ScriptedServer::read(&mut s, 3).await;
        s.write_all(&[5, 0xFF]).await.unwrap();
    })
    .await;

    let error = proxy_tunnel::connect("ftp.example.com", 21, Some(&endpoint(ProxyKind::Socks5, server.port, None))).await.unwrap_err();
    assert!(matches!(&error, CoreError::ProxyAuthentication(m) if m == CoreError::PROXY_AUTH_NEEDED));
}

#[tokio::test]
async fn a_socks5_refusal_names_the_reason() {
    let server = ScriptedServer::start(|mut s| async move {
        ScriptedServer::read(&mut s, 3).await;
        s.write_all(&[5, 0]).await.unwrap();
        ScriptedServer::read(&mut s, 5 + 15 + 2).await;
        s.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
    })
    .await;

    let error = proxy_tunnel::connect("ftp.example.com", 21, Some(&endpoint(ProxyKind::Socks5, server.port, None))).await.unwrap_err();
    assert!(matches!(&error, CoreError::Io(m) if m.contains("refused")), "{error}");
}

#[tokio::test]
async fn socks4a_sends_the_host_name_after_the_user_id() {
    let server = ScriptedServer::start(|mut s| async move {
        let request = ScriptedServer::read(&mut s, 8 + "bob".len() + 1 + "ftp.example.com".len() + 1).await;
        assert_eq!(&request[..8], &[4, 1, (2121u16 >> 8) as u8, (2121u16 & 0xFF) as u8, 0, 0, 0, 1]);
        assert_eq!(&request[8..], b"bob\0ftp.example.com\0");
        let mut reply = vec![0, 0x5A, 0, 0, 0, 0, 0, 0];
        reply.extend_from_slice(GREETING);
        s.write_all(&reply).await.unwrap();
    })
    .await;

    let mut stream = proxy_tunnel::connect("ftp.example.com", 2121, Some(&endpoint(ProxyKind::Socks4, server.port, Some(("bob", ""))))).await.unwrap();
    assert_eq!(first_line(&mut stream).await, "220 ready\r\n");
    server.completion.await.unwrap();
}

#[tokio::test]
async fn http_connect_asks_for_the_host_with_the_proxy_login_and_leaves_the_greeting_unread() {
    let head = Arc::new(parking_lot::Mutex::new(String::new()));
    let seen = Arc::clone(&head);
    let server = ScriptedServer::start(move |mut s| async move {
        *seen.lock() = ScriptedServer::read_head(&mut s).await;
        let mut reply = b"HTTP/1.1 200 Connection established\r\nVia: test\r\n\r\n".to_vec();
        reply.extend_from_slice(GREETING);
        s.write_all(&reply).await.unwrap();
    })
    .await;

    let mut stream = proxy_tunnel::connect("ftp.example.com", 21, Some(&endpoint(ProxyKind::Http, server.port, Some(("alice", "pw"))))).await.unwrap();
    assert_eq!(first_line(&mut stream).await, "220 ready\r\n");
    server.completion.await.unwrap();

    let head = head.lock().clone();
    assert!(head.starts_with("CONNECT ftp.example.com:21 HTTP/1.1\r\nHost: ftp.example.com:21\r\n"), "{head}");
    let pair = base64::engine::general_purpose::STANDARD.encode("alice:pw");
    assert!(head.contains(&format!("Proxy-Authorization: Basic {pair}")));
}

#[tokio::test]
async fn an_http_proxy_that_will_not_connect_says_why() {
    for (status_line, wants_login) in [("HTTP/1.1 407 Proxy Authentication Required", true), ("HTTP/1.1 403 Forbidden", false)] {
        let server = ScriptedServer::start(move |mut s| async move {
            ScriptedServer::read_head(&mut s).await;
            s.write_all(format!("{status_line}\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
        })
        .await;

        let error = proxy_tunnel::connect("ftp.example.com", 21, Some(&endpoint(ProxyKind::Http, server.port, None))).await.unwrap_err();
        if wants_login {
            assert!(matches!(&error, CoreError::ProxyAuthentication(m) if m == CoreError::PROXY_AUTH_NEEDED));
        } else {
            assert!(matches!(&error, CoreError::Io(m) if m.contains("403")), "{error}");
        }
    }
}

#[tokio::test]
async fn a_proxy_that_is_not_there_is_reported_as_the_proxy() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let error = proxy_tunnel::connect("ftp.example.com", 21, Some(&endpoint(ProxyKind::Socks5, port, None))).await.unwrap_err();
    assert!(matches!(&error, CoreError::Io(m) if m.contains("proxy")), "{error}");
}

#[tokio::test]
async fn an_ftp_download_runs_through_a_socks5_proxy_control_and_data_alike() {
    let file: Vec<u8> = (0..70_000u32).map(|i| (i * 7) as u8).collect();
    let ftp = FakeFtpServer::start(file.clone(), FtpBehaviour::default()).await;
    let socks = FakeSocks5Proxy::start(Some(("carol", "pw"))).await;
    let proxy = AppProxy::new(None);
    proxy.configure(Some(&manual_socks5(socks.port, "carol", "pw")));

    let url = parse_url(&format!("ftp://ftp.test:{}/pub/file.bin", ftp.port)).unwrap();
    let cancel = CancellationToken::new();
    let mut channel = FtpControlChannel::connect(&url, None, &cancel, Some(&proxy)).await.unwrap();
    let mut data = channel.open_read("/pub/file.bin", 0, &cancel).await.unwrap();
    let mut received = Vec::new();
    data.read_to_end(&mut received).await.unwrap();

    assert_eq!(received, file);

    // Both connections were the proxy's to make, to the name in the address —
    // never the private address the server announced for passive mode.
    let requests = socks.requests.lock().clone();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|(host, _)| host == "ftp.test"));
    assert_eq!(requests[0].1, ftp.port);
}
