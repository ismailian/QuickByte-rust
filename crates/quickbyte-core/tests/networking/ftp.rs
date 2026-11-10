//! The FTP client against a local fake server: the probe (`SIZE`, `MDTM`,
//! `FEAT`), a segment fetched with `REST` and cut off at its end, the recovery
//! when `REST` is refused, and a refused login.

use std::sync::Arc;

use quickbyte_core::CoreError;
use quickbyte_core::enums::ConnectionStatus;
use quickbyte_core::ftp::download_connection::FtpDownloadConnection;
use quickbyte_core::ftp::file_info_provider::FtpFileInfoProvider;
use quickbyte_core::models::{DownloadCredentials, DownloadSettings, RequestOptions};
use quickbyte_core::net::app_proxy::AppProxy;
use quickbyte_core::traits::{ConnectionSpec, DownloadConnection as _, RemoteFileInfoProvider as _};
use tokio_util::sync::CancellationToken;

use crate::support::{FakeFtpServer, FtpBehaviour, TempFolder};

fn file() -> Vec<u8> {
    (0..10_000u32).map(|i| (i % 253) as u8).collect()
}

fn direct() -> Arc<AppProxy> {
    Arc::new(AppProxy::new(None))
}

fn connection(url: &str, start: i64, end: i64, already: i64, chunk: &str, options: RequestOptions) -> FtpDownloadConnection {
    FtpDownloadConnection::with_proxy(
        ConnectionSpec {
            connection_id: 0,
            url: url.to_string(),
            range_start: start,
            range_end: end,
            already_downloaded: already,
            chunk_file_path: chunk.to_string(),
            settings: Arc::new(DownloadSettings { max_retries: 0, retry_delay_milliseconds: 0, ..Default::default() }),
            bandwidth_limiter: None,
            options,
        },
        direct(),
    )
}

#[tokio::test]
async fn the_probe_reads_size_date_and_restart_support() {
    let server = FakeFtpServer::start(file(), FtpBehaviour::default()).await;
    let info = FtpFileInfoProvider::with_proxy(direct())
        .get_file_info(&server.url("/pub/my%20disk.iso"), &RequestOptions::none(), &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(info.content_length, 10_000);
    assert_eq!(info.file_name, "my disk.iso");
    assert_eq!(info.content_type, "application/octet-stream");
    assert_eq!(info.last_modified.unwrap().to_string(), "2026-09-01T08:30:00+00:00");
    assert!(info.supports_range_requests);
    assert!(info.server_claims_range_support);
    // Anonymous, binary, and the path as the server wants it: decoded.
    let commands = server.commands();
    assert!(commands.contains(&"USER anonymous".to_string()));
    assert!(commands.contains(&"TYPE I".to_string()));
    assert!(commands.contains(&"SIZE /pub/my disk.iso".to_string()));
}

#[tokio::test]
async fn a_server_that_does_not_advertise_rest_stream_runs_on_one_connection() {
    let server = FakeFtpServer::start(file(), FtpBehaviour { feat_rest_stream: false, ..Default::default() }).await;
    let info = FtpFileInfoProvider::with_proxy(direct())
        .get_file_info(&server.url("/a.bin"), &RequestOptions::none(), &CancellationToken::new())
        .await
        .unwrap();
    assert!(!info.supports_range_requests);
}

#[tokio::test]
async fn a_refused_login_is_a_credentials_prompt() {
    let behaviour = FtpBehaviour { login: Some(("alice".into(), "right".into())), ..Default::default() };
    let server = FakeFtpServer::start(file(), behaviour).await;
    let provider = FtpFileInfoProvider::with_proxy(direct());

    let anonymous = provider.get_file_info(&server.url("/a.bin"), &RequestOptions::none(), &CancellationToken::new()).await.unwrap_err();
    assert!(matches!(anonymous, CoreError::AuthenticationRequired { credentials_were_supplied: false, .. }));
    assert_eq!(anonymous.to_string(), "This FTP server does not allow anonymous access — a user name and password are needed.");

    let options = RequestOptions { credentials: Some(DownloadCredentials::new("alice", "wrong")), ..Default::default() };
    let wrong = provider.get_file_info(&server.url("/a.bin"), &options, &CancellationToken::new()).await.unwrap_err();
    assert!(matches!(wrong, CoreError::AuthenticationRequired { credentials_were_supplied: true, .. }));
    assert!(wrong.to_string().starts_with("The FTP server rejected that user name or password ("), "{wrong}");

    let options = RequestOptions { credentials: Some(DownloadCredentials::new("alice", "right")), ..Default::default() };
    assert!(provider.get_file_info(&server.url("/a.bin"), &options, &CancellationToken::new()).await.is_ok());
}

#[tokio::test]
async fn a_segment_starts_with_rest_and_stops_at_its_end() {
    let server = FakeFtpServer::start(file(), FtpBehaviour::default()).await;
    let folder = TempFolder::new();
    let connection = connection(&server.url("/a.bin"), 1000, 2999, 0, &folder.file("part1.tmp"), RequestOptions::none());

    connection.run(CancellationToken::new()).await.unwrap();

    assert_eq!(std::fs::read(folder.file("part1.tmp")).unwrap(), file()[1000..3000]);
    assert_eq!(connection.status(), ConnectionStatus::Finished);
    assert!(server.commands().contains(&"REST 1000".to_string()));
}

#[tokio::test]
async fn a_resumed_segment_carries_on_from_its_chunk() {
    let server = FakeFtpServer::start(file(), FtpBehaviour::default()).await;
    let folder = TempFolder::new();
    folder.write("part0.tmp", &file()[..400]);

    connection(&server.url("/a.bin"), 0, 9999, 400, &folder.file("part0.tmp"), RequestOptions::none()).run(CancellationToken::new()).await.unwrap();

    assert_eq!(std::fs::read(folder.file("part0.tmp")).unwrap(), file());
}

#[tokio::test]
async fn a_refused_rest_throws_the_partial_away_and_starts_from_zero() {
    let server = FakeFtpServer::start(file(), FtpBehaviour { honours_rest: false, ..Default::default() }).await;
    let folder = TempFolder::new();
    folder.write("part0.tmp", &[0xEE; 400]);

    connection(&server.url("/a.bin"), 0, 9999, 400, &folder.file("part0.tmp"), RequestOptions::none()).run(CancellationToken::new()).await.unwrap();

    assert_eq!(std::fs::read(folder.file("part0.tmp")).unwrap(), file());
}

#[tokio::test]
async fn a_data_connection_that_ends_early_is_reported() {
    let server = FakeFtpServer::start(file()[..500].to_vec(), FtpBehaviour::default()).await;
    let folder = TempFolder::new();
    let connection = connection(&server.url("/a.bin"), 0, 999, 0, &folder.file("part0.tmp"), RequestOptions::none());

    let error = connection.run(CancellationToken::new()).await.unwrap_err();

    assert_eq!(error.to_string(), "The FTP data connection ended 500 bytes early.");
    assert_eq!(connection.status(), ConnectionStatus::Failed);
}
