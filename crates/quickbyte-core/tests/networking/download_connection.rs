//! Port of `DownloadConnectionTests.cs`: one connection against a scripted server.
//!
//! Most of what is checked here is what happens when the server does *not* do
//! as it was asked. A response that ignores the Range header, or stops short of
//! the range it promised, is not an error anywhere in HTTP — the bytes simply
//! land at the wrong offset in a chunk file, the pool sees a connection that ran
//! to the end of its loop, and the merge writes the damage into the finished
//! file. Nothing downstream can tell, so it has to be caught here.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use quickbyte_core::CoreError;
use quickbyte_core::enums::ConnectionStatus;
use quickbyte_core::helpers::range_splitter::UNBOUNDED_END;
use quickbyte_core::models::{DownloadCredentials, DownloadSettings, Headers, RemoteFileVersion, RequestOptions};
use quickbyte_core::net::download_connection::DownloadConnection;
use quickbyte_core::traits::{ConnectionSpec, DownloadConnection as _};
use tokio_util::sync::CancellationToken;

use crate::support::{Asked, Reply, TempFolder, TestServer, direct_client};

fn file100() -> Vec<u8> {
    (0..100u8).collect()
}

fn settings(max_retries: i32) -> Arc<DownloadSettings> {
    Arc::new(DownloadSettings { max_retries, retry_delay_milliseconds: 0, ..Default::default() })
}

fn connection(server: &TestServer, chunk: &str, start: i64, end: i64, already: i64, settings: Arc<DownloadSettings>, options: RequestOptions) -> DownloadConnection {
    DownloadConnection::new(
        direct_client(),
        ConnectionSpec {
            connection_id: 0,
            url: server.url("/file.bin"),
            range_start: start,
            range_end: end,
            already_downloaded: already,
            chunk_file_path: chunk.to_string(),
            settings,
            bandwidth_limiter: None,
            options,
        },
    )
}

fn simple(server: &TestServer, chunk: &str, start: i64, end: i64, already: i64) -> DownloadConnection {
    connection(server, chunk, start, end, already, settings(0), RequestOptions::default())
}

fn ranges(server: &TestServer) -> Vec<String> {
    server.requests().iter().map(|r| r.header("Range").unwrap_or_default()).collect()
}

async fn run(connection: &DownloadConnection) -> Result<(), CoreError> {
    connection.run(CancellationToken::new()).await
}

fn v1(server: &TestServer) -> RemoteFileVersion {
    RemoteFileVersion::new(Some("\"v1\"".into()), None, Some(server.authority()))
}

// ------------------------------------------------------------ the happy path --

#[tokio::test]
async fn a_segment_the_server_honours_lands_in_its_chunk() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, _| Reply::partial(&file100()[50..], 50, 99, 100)).await;
    let connection = simple(&server, &folder.file("part1.tmp"), 50, 99, 0);

    run(&connection).await.unwrap();

    assert_eq!(std::fs::read(folder.file("part1.tmp")).unwrap(), file100()[50..]);
    assert_eq!(connection.bytes_downloaded(), 50);
    assert_eq!(connection.status(), ConnectionStatus::Finished);
}

#[tokio::test]
async fn a_resumed_segment_asks_for_what_is_missing_and_appends_it() {
    let folder = TempFolder::new();
    folder.write("part0.tmp", &file100()[..40]);
    let server = TestServer::start(|_, _| Reply::partial(&file100()[40..], 40, 99, 100)).await;

    run(&simple(&server, &folder.file("part0.tmp"), 0, 99, 40)).await.unwrap();

    assert_eq!(ranges(&server), vec!["bytes=40-99"]);
    assert_eq!(std::fs::read(folder.file("part0.tmp")).unwrap(), file100());
}

#[tokio::test]
async fn a_segment_already_complete_on_disk_sends_no_request() {
    let folder = TempFolder::new();
    folder.write("part0.tmp", &file100());
    let server = TestServer::start(|_, _| panic!("should not be asked")).await;

    let connection = simple(&server, &folder.file("part0.tmp"), 0, 99, 100);
    run(&connection).await.unwrap();

    assert!(server.requests().is_empty());
    assert_eq!(connection.status(), ConnectionStatus::Finished);
}

// ---------------------------------------------- a server that ignores the range --

#[tokio::test]
async fn a_resumed_first_segment_answered_with_the_whole_file_starts_again_from_zero() {
    // A 200 means the body begins at byte 0, and writing it at the resume offset
    // appends the whole file to the partial.
    let folder = TempFolder::new();
    folder.write("part0.tmp", &file100()[..40]);
    let server = TestServer::start(|_, _| Reply::ok(&file100())).await;

    let connection = simple(&server, &folder.file("part0.tmp"), 0, 99, 40);
    run(&connection).await.unwrap();

    assert_eq!(std::fs::read(folder.file("part0.tmp")).unwrap(), file100());
    assert_eq!(connection.bytes_downloaded(), 100);
    assert_eq!(connection.status(), ConnectionStatus::Finished);
}

#[tokio::test]
async fn a_restart_from_zero_leaves_no_tail_of_the_longer_chunk_it_replaced() {
    let folder = TempFolder::new();
    folder.write("part0.tmp", &[0xEE; 90]);
    let server = TestServer::start(|_, _| Reply::ok(&file100()[..60])).await;

    run(&simple(&server, &folder.file("part0.tmp"), 0, UNBOUNDED_END, 90)).await.unwrap();

    assert_eq!(std::fs::read(folder.file("part0.tmp")).unwrap(), file100()[..60]);
}

#[tokio::test]
async fn a_chunk_longer_than_its_range_is_read_as_a_finished_segment() {
    let folder = TempFolder::new();
    folder.write("part0.tmp", &file100());
    let server = TestServer::start(|_, _| panic!("should not be asked")).await;

    let connection = simple(&server, &folder.file("part0.tmp"), 0, 49, 100);
    run(&connection).await.unwrap();

    assert!(server.requests().is_empty());
    assert_eq!(connection.status(), ConnectionStatus::Finished);
}

#[tokio::test]
async fn a_later_segment_answered_with_the_whole_file_fails_instead_of_writing_it() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, _| Reply::ok(&file100())).await;
    let connection = simple(&server, &folder.file("part1.tmp"), 50, 99, 0);

    assert!(matches!(run(&connection).await, Err(CoreError::Io(_))));
    assert_ne!(connection.status(), ConnectionStatus::Finished);
    assert!(!std::path::Path::new(&folder.file("part1.tmp")).exists());
}

#[tokio::test]
async fn a_partial_response_that_starts_somewhere_else_is_refused() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, _| Reply::partial(&file100()[..50], 0, 49, 100)).await;

    let error = run(&simple(&server, &folder.file("part1.tmp"), 50, 99, 0)).await.unwrap_err();

    assert!(matches!(&error, CoreError::Io(m) if m.contains("50")), "{error}");
}

// ----------------------------------------------- a server that sends too much --

#[tokio::test]
async fn a_response_longer_than_the_segment_is_cut_off_at_its_end() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, _| Reply::partial(&file100(), 0, 49, 100)).await;
    let connection = simple(&server, &folder.file("part0.tmp"), 0, 49, 0);

    run(&connection).await.unwrap();

    assert_eq!(std::fs::read(folder.file("part0.tmp")).unwrap(), file100()[..50]);
    assert_eq!(connection.bytes_downloaded(), 50);
}

// ---------------------------------------------- a server that stops too early --

#[tokio::test]
async fn a_transfer_that_ends_early_is_reported_rather_than_accepted() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, _| Reply::partial(&file100()[..60], 0, 99, 100)).await;
    let connection = simple(&server, &folder.file("part0.tmp"), 0, 99, 0);

    let error = run(&connection).await.unwrap_err();

    assert!(error.to_string().contains("40 bytes early"), "{error}");
    assert_ne!(connection.status(), ConnectionStatus::Finished);
}

#[tokio::test]
async fn a_retry_after_an_early_end_carries_on_from_the_bytes_that_arrived() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, index| {
        if index == 0 { Reply::partial(&file100()[..60], 0, 99, 100) } else { Reply::partial(&file100()[60..], 60, 99, 100) }
    })
    .await;

    let connection = connection(&server, &folder.file("part0.tmp"), 0, 99, 0, settings(3), RequestOptions::default());
    run(&connection).await.unwrap();

    assert_eq!(ranges(&server), vec!["bytes=0-99", "bytes=60-99"]);
    assert_eq!(std::fs::read(folder.file("part0.tmp")).unwrap(), file100());
    assert_eq!(connection.status(), ConnectionStatus::Finished);
    assert_eq!(connection.retry_count(), 1);
}

// ------------------------------------------------------------ an unknown length --

#[tokio::test]
async fn a_segment_with_no_known_end_asks_for_an_open_ended_range() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, _| Reply::ok(&file100())).await;

    let connection = simple(&server, &folder.file("part0.tmp"), 0, UNBOUNDED_END, 0);
    run(&connection).await.unwrap();

    assert_eq!(ranges(&server), vec!["bytes=0-"]);
    assert_eq!(std::fs::read(folder.file("part0.tmp")).unwrap(), file100());
    // No "ended early" — there was nothing to end early against.
    assert_eq!(connection.status(), ConnectionStatus::Finished);
}

// -------------------------------------------------- a file that changed meanwhile --

#[tokio::test]
async fn a_resumed_segment_asks_on_condition_that_the_file_is_still_the_same() {
    let folder = TempFolder::new();
    folder.write("part1.tmp", &file100()[50..70]);
    let server = TestServer::start(|_, _| Reply::partial(&file100()[70..], 70, 99, 100)).await;
    let options = RequestOptions { resume_validator: Some(v1(&server)), ..Default::default() };

    run(&connection(&server, &folder.file("part1.tmp"), 50, 99, 20, settings(0), options)).await.unwrap();

    assert_eq!(server.requests()[0].header("If-Range").as_deref(), Some("\"v1\""));
    assert_eq!(std::fs::read(folder.file("part1.tmp")).unwrap(), file100()[50..]);
}

#[tokio::test]
async fn a_whole_file_answer_to_the_condition_reports_a_changed_file_and_writes_nothing() {
    let folder = TempFolder::new();
    folder.write("part0.tmp", &file100()[..40]);
    let server = TestServer::start(|_, _| Reply::ok(&[0xAB; 100])).await;
    let options = RequestOptions { resume_validator: Some(v1(&server)), ..Default::default() };

    let connection = connection(&server, &folder.file("part0.tmp"), 0, 99, 40, settings(3), options);
    let error = run(&connection).await.unwrap_err();

    assert!(matches!(error, CoreError::RemoteFileChanged(_)));
    assert_eq!(std::fs::read(folder.file("part0.tmp")).unwrap(), file100()[..40]);
    // And not retried: asking again gets the same answer.
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn a_mirror_the_request_was_redirected_to_is_asked_again_without_the_condition() {
    // A mirror redirector hands the request to a server that minted its own ETag
    // for the identical file ("localhost" is another host than "127.0.0.1").
    let folder = TempFolder::new();
    let port = Arc::new(AtomicUsize::new(0));
    let seen_port = Arc::clone(&port);
    let server = TestServer::start(move |asked: &Asked, index| match (asked.path.as_str(), index) {
        ("/file.bin", 0) => Reply::redirect(&format!("http://localhost:{}/mirror.bin", seen_port.load(Ordering::SeqCst))),
        ("/mirror.bin", _) => Reply::ok(&file100()),
        _ => Reply::partial(&file100()[50..], 50, 99, 100),
    })
    .await;
    port.store(server.port as usize, Ordering::SeqCst);
    let options = RequestOptions { resume_validator: Some(v1(&server)), ..Default::default() };

    run(&connection(&server, &folder.file("part1.tmp"), 50, 99, 0, settings(0), options)).await.unwrap();

    let to_origin: Vec<Option<String>> = server.requests().iter().filter(|r| r.path == "/file.bin").map(|r| r.header("If-Range")).collect();
    assert_eq!(to_origin, vec![Some("\"v1\"".to_string()), None]);
    assert_eq!(std::fs::read(folder.file("part1.tmp")).unwrap(), file100()[50..]);
}

#[tokio::test]
async fn no_condition_is_sent_without_a_version_to_hold_the_server_to() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, _| Reply::partial(&file100(), 0, 99, 100)).await;
    run(&simple(&server, &folder.file("part0.tmp"), 0, 99, 0)).await.unwrap();
    assert_eq!(server.requests()[0].header("If-Range"), None);
}

// ------------------------------------------------------------- who is asking --

#[tokio::test]
async fn a_segment_says_who_is_asking() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, _| Reply::partial(&file100(), 0, 99, 100)).await;
    run(&simple(&server, &folder.file("part0.tmp"), 0, 99, 0)).await.unwrap();
    let agents = server.requests()[0].headers_named("User-Agent");
    assert_eq!(agents.len(), 1);
    assert!(!agents[0].trim().is_empty());
}

#[tokio::test]
async fn a_segment_presents_the_browsers_agent_when_the_link_was_captured() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, _| Reply::partial(&file100(), 0, 99, 100)).await;
    let mut headers = Headers::new();
    headers.insert("Cookie".into(), "session=abc".into());
    headers.insert("User-Agent".into(), "Mozilla/5.0 (a browser)".into());
    let options = RequestOptions { headers: Some(headers), ..Default::default() };

    run(&connection(&server, &folder.file("part0.tmp"), 0, 99, 0, settings(0), options)).await.unwrap();

    assert_eq!(server.requests()[0].headers_named("User-Agent"), vec!["Mozilla/5.0 (a browser)"]);
}

// ---------------------------------------------------------------- challenges --

#[tokio::test]
async fn a_challenge_is_raised_as_the_one_error_the_user_can_act_on() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, _| Reply::status(401)).await;
    let connection = connection(&server, &folder.file("part0.tmp"), 0, 99, 0, settings(3), RequestOptions::default());

    let error = run(&connection).await.unwrap_err();

    assert!(matches!(error, CoreError::AuthenticationRequired { credentials_were_supplied: false, .. }));
    // And it is not retried.
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn a_proxy_challenge_points_at_the_proxy_settings_and_is_not_retried() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, _| Reply::status(407)).await;
    let connection = connection(&server, &folder.file("part0.tmp"), 0, 99, 0, settings(3), RequestOptions::default());

    let error = run(&connection).await.unwrap_err();

    assert!(matches!(&error, CoreError::ProxyAuthentication(m) if m == CoreError::PROXY_AUTH_NEEDED));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn a_rejected_password_says_so() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, _| Reply::status(401).header("WWW-Authenticate", "Basic realm=\"files\"")).await;
    let options = RequestOptions { credentials: Some(DownloadCredentials::new("alice", "wrong")), ..Default::default() };

    let error = run(&connection(&server, &folder.file("part0.tmp"), 0, 99, 0, settings(0), options)).await.unwrap_err();

    assert!(matches!(error, CoreError::AuthenticationRequired { credentials_were_supplied: true, .. }));
}

#[tokio::test]
async fn an_ordinary_failure_is_retried_and_then_surfaced() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, _| Reply::status(500)).await;
    let connection = connection(&server, &folder.file("part0.tmp"), 0, 99, 0, settings(2), RequestOptions::default());

    let error = run(&connection).await.unwrap_err();

    assert!(matches!(error, CoreError::Http { status: Some(500), .. }));
    assert_eq!(server.requests().len(), 3);
    assert_eq!(connection.retry_count(), 2);
    assert!(connection.last_error().is_some());
    assert_eq!(connection.status(), ConnectionStatus::Failed);
}

#[tokio::test]
async fn a_cancelled_connection_stops_without_calling_itself_failed() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, _| Reply::partial(&file100(), 0, 99, 100)).await;
    let connection = simple(&server, &folder.file("part0.tmp"), 0, 99, 0);
    let cancel = CancellationToken::new();
    cancel.cancel();

    assert!(matches!(connection.run(cancel).await, Err(CoreError::Cancelled)));
    assert_ne!(connection.status(), ConnectionStatus::Failed);
}

// ----------------------------------------------------- the shrinkable end --

#[tokio::test]
async fn a_shortened_range_stops_where_its_new_end_is() {
    let folder = TempFolder::new();
    let server = TestServer::start(|_, _| Reply::partial(&file100(), 0, 99, 100)).await;
    let connection = simple(&server, &folder.file("part0.tmp"), 0, 99, 0);
    let resizable = quickbyte_core::traits::DownloadConnection::as_resizable(&connection).unwrap();

    assert!(resizable.try_shrink_end(59));
    assert_eq!(resizable.committed_end(), -1);
    run(&connection).await.unwrap();

    assert_eq!(std::fs::read(folder.file("part0.tmp")).unwrap(), file100()[..60]);
    assert_eq!(connection.range_end(), 59);
    assert!(!resizable.try_shrink_end(30), "nothing written may be cut off");
}
