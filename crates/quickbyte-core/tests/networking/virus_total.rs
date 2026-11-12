//! The VirusTotal client against a local stand-in for its API: what is sent,
//! how answers are read, and how the client paces itself and waits out a quota.

use std::time::{Duration, Instant};

use quickbyte_core::error::CoreError;
use quickbyte_core::helpers::virus_total::url_id;
use quickbyte_core::net::virus_total::{QUOTA_USED, VirusTotalClient};
use quickbyte_core::traits::VirusTotalApi;
use tokio_util::sync::CancellationToken;

use crate::support::{Reply, TempFolder, TestServer, direct_client};

const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn client_for(server: &TestServer, interval: Duration) -> VirusTotalClient {
    VirusTotalClient::with(&server.url("/api/v3"), direct_client(), interval, Duration::from_millis(10))
}

fn json(body: &str) -> Reply {
    Reply::ok(body.as_bytes()).header("Content-Type", "application/json")
}

#[tokio::test]
async fn a_lookup_sends_the_key_and_reads_the_tally() {
    let server = TestServer::start(|_, _| {
        json(r#"{"data":{"id":"7c9d","attributes":{"last_analysis_stats":{"malicious":2,"suspicious":0,"harmless":60,"undetected":8}}}}"#)
    })
    .await;
    let client = client_for(&server, Duration::ZERO);

    let found = client.url_report(KEY, "https://files.example.com/a.exe", &CancellationToken::new()).await.unwrap().unwrap();

    assert_eq!(found.stats.malicious, 2);
    assert_eq!(found.stats.engines(), 70);
    assert_eq!(found.object_id.as_deref(), Some("7c9d"));
    let asked = &server.requests()[0];
    assert_eq!(asked.method, "GET");
    assert_eq!(asked.path, format!("/api/v3/urls/{}", url_id("https://files.example.com/a.exe")));
    assert_eq!(asked.header("x-apikey").as_deref(), Some(KEY));
}

#[tokio::test]
async fn something_virus_total_has_never_seen_is_no_report_rather_than_an_error() {
    let server = TestServer::start(|_, _| {
        Reply::status(404).with_body(br#"{"error":{"code":"NotFoundError","message":"not found"}}"#)
    })
    .await;
    let client = client_for(&server, Duration::ZERO);
    let cancel = CancellationToken::new();

    assert_eq!(client.url_report(KEY, "https://a.example/", &cancel).await.unwrap(), None);
    assert_eq!(client.file_report(KEY, &"AB".repeat(32), &cancel).await.unwrap(), None);
    assert_eq!(server.requests()[1].path, format!("/api/v3/files/{}", "ab".repeat(32)));
}

#[tokio::test]
async fn an_address_is_submitted_as_a_form_and_its_analysis_followed_until_it_completes() {
    let server = TestServer::start(|asked, _| match (asked.method.as_str(), asked.path.as_str()) {
        ("POST", "/api/v3/urls") => json(r#"{"data":{"type":"analysis","id":"u-7c9d-1700000000"}}"#),
        _ if asked.path.starts_with("/api/v3/analyses/") => json(r#"{"data":{"attributes":{"status":"queued","stats":{}}}}"#),
        _ => Reply::status(500),
    })
    .await;
    let client = client_for(&server, Duration::ZERO);
    let cancel = CancellationToken::new();

    let analysis = client.submit_url(KEY, "https://files.example.com/a b.exe?x=1&y=2", &cancel).await.unwrap();
    assert_eq!(analysis, "u-7c9d-1700000000");
    assert_eq!(client.analysis(KEY, &analysis, &cancel).await.unwrap(), None, "queued is not finished");

    let posted = &server.requests()[0];
    assert_eq!(posted.header("Content-Type").as_deref(), Some("application/x-www-form-urlencoded"));
    assert_eq!(String::from_utf8_lossy(&posted.body), "url=https%3A%2F%2Ffiles.example.com%2Fa+b.exe%3Fx%3D1%26y%3D2");
    assert_eq!(server.requests()[1].path, "/api/v3/analyses/u%2D7c9d%2D1700000000");
}

#[tokio::test]
async fn a_completed_analysis_carries_its_tally_and_the_id_of_what_it_analysed() {
    let server = TestServer::start(|_, _| {
        json(
            r#"{"data":{"attributes":{"status":"completed","stats":{"malicious":0,"suspicious":1,"harmless":50,"undetected":10}}},
                "meta":{"url_info":{"id":"7c9d","url":"https://a.example/"}}}"#,
        )
    })
    .await;
    let client = client_for(&server, Duration::ZERO);

    let done = client.analysis(KEY, "u-7c9d-1", &CancellationToken::new()).await.unwrap().unwrap();
    assert_eq!(done.stats.suspicious, 1);
    assert_eq!(done.object_id.as_deref(), Some("7c9d"));
}

#[tokio::test]
async fn an_upload_sends_the_file_and_nothing_else() {
    let server = TestServer::start(|_, _| json(r#"{"data":{"type":"analysis","id":"ZmlsZQ=="}}"#)).await;
    let client = client_for(&server, Duration::ZERO);
    let folder = TempFolder::new();
    let path = folder.write("setup.exe", b"MZ\x90\x00payload");

    let analysis = client.upload_file(KEY, &path, &CancellationToken::new()).await.unwrap();

    assert_eq!(analysis, "ZmlsZQ==");
    let posted = &server.requests()[0];
    assert_eq!((posted.method.as_str(), posted.path.as_str()), ("POST", "/api/v3/files"));
    let content_type = posted.header("Content-Type").unwrap();
    let boundary = content_type.strip_prefix("multipart/form-data; boundary=").expect(&content_type);
    let body = &posted.body;
    let expected_head = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"setup.exe\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    );
    assert!(body.starts_with(expected_head.as_bytes()));
    assert!(body.ends_with(format!("\r\n--{boundary}--\r\n").as_bytes()));
    let file: &[u8] = b"MZ\x90\x00payload";
    assert!(body.windows(file.len()).any(|w| w == file), "the file's bytes, unaltered");
    assert_eq!(posted.header("Cookie"), None);
    assert_eq!(posted.header("Authorization"), None);
}

#[tokio::test]
async fn a_rejected_key_is_an_authentication_error() {
    let server = TestServer::start(|_, _| {
        Reply::status(401).with_body(br#"{"error":{"code":"WrongCredentialsError","message":"Wrong API key"}}"#)
    })
    .await;
    let client = client_for(&server, Duration::ZERO);

    let error = client.check_key("nope", &CancellationToken::new()).await.unwrap_err();
    assert!(matches!(error, CoreError::AuthenticationRequired { .. }), "{error:?}");
}

#[tokio::test]
async fn a_key_is_checked_against_a_file_every_account_can_look_up() {
    // Found or not found both mean the key was accepted.
    let server = TestServer::start(|_, index| if index == 0 { Reply::status(404) } else { json(r#"{"data":{}}"#) }).await;
    let client = client_for(&server, Duration::ZERO);
    let cancel = CancellationToken::new();

    client.check_key(KEY, &cancel).await.unwrap();
    client.check_key(KEY, &cancel).await.unwrap();
    assert!(server.requests()[0].path.starts_with("/api/v3/files/e3b0c442"));
    assert!(client.check_key("  ", &cancel).await.is_err(), "a blank key is not sent at all");
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn a_used_up_quota_is_waited_out_and_asked_again() {
    let server = TestServer::start(|_, index| if index < 2 { Reply::status(429) } else { Reply::status(404) }).await;
    let client = client_for(&server, Duration::ZERO);

    assert_eq!(client.url_report(KEY, "https://a.example/", &CancellationToken::new()).await.unwrap(), None);
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn a_quota_still_used_up_after_waiting_is_an_error() {
    let server = TestServer::start(|_, _| Reply::status(429)).await;
    let client = client_for(&server, Duration::ZERO);

    let error = client.url_report(KEY, "https://a.example/", &CancellationToken::new()).await.unwrap_err();
    assert_eq!(error.to_string(), QUOTA_USED);
    assert_eq!(server.requests().len(), 3, "the first ask and two retries");
}

#[tokio::test]
async fn requests_are_spaced_to_keep_within_the_quota() {
    let server = TestServer::start(|_, _| Reply::status(404)).await;
    let client = client_for(&server, Duration::from_millis(150));
    let cancel = CancellationToken::new();

    let started = Instant::now();
    for _ in 0..3 {
        client.url_report(KEY, "https://a.example/", &cancel).await.unwrap();
    }
    // The first goes at once; each after it waits its turn.
    assert!(started.elapsed() >= Duration::from_millis(300), "{:?}", started.elapsed());
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn waiting_for_a_turn_stops_when_the_check_is_cancelled() {
    let server = TestServer::start(|_, _| Reply::status(404)).await;
    let client = client_for(&server, Duration::from_secs(60));
    let cancel = CancellationToken::new();
    client.url_report(KEY, "https://a.example/", &cancel).await.unwrap();

    cancel.cancel();
    let error = client.url_report(KEY, "https://a.example/", &cancel).await.unwrap_err();
    assert!(error.is_cancelled());
    assert_eq!(server.requests().len(), 1);
}
