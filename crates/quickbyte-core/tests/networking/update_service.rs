//! The update check against a local manifest endpoint (the manifest address is
//! overridable for exactly this — staging a release against a test endpoint).

use quickbyte_core::models::UpdateManifest;
use quickbyte_core::services::update_service::UpdateService;
use quickbyte_core::traits::UpdateService as _;
use tokio_util::sync::CancellationToken;

use crate::support::{Reply, TempFolder, TestServer, direct_client};

async fn service_for(manifest: &'static str) -> (TestServer, UpdateService, TempFolder) {
    let server = TestServer::start(move |_, _| Reply::ok(manifest.as_bytes()).header("Content-Type", "application/json")).await;
    let folder = TempFolder::new();
    let service = UpdateService::with_client(Some(&server.url("/releases/latest.json")), Some(folder.path.join("updates")), direct_client());
    (server, service, folder)
}

#[tokio::test]
async fn a_newer_release_is_offered() {
    let (server, service, _folder) = service_for(r#"{"version":"v1.30.0","downloadUrl":"https://example.com/QuickByte-1.30.0-Setup.exe","fileSizeBytes":7340032}"#).await;

    let result = service.check_for_update("1.27.0", &CancellationToken::new()).await.unwrap();

    assert!(result.update_available);
    assert_eq!(result.latest_version(), "v1.30.0");
    assert_eq!(result.manifest.unwrap().file_size_bytes, 7_340_032);
    // A cached manifest looks exactly like "no update", so it asks for a fresh one,
    // and says who is asking.
    let asked = &server.requests()[0];
    assert_eq!(asked.header("Cache-Control").as_deref(), Some("no-cache"));
    assert!(asked.header("User-Agent").is_some());
}

#[tokio::test]
async fn the_same_release_or_its_own_beta_is_not_an_update() {
    let (_server, service, _folder) = service_for(r#"{"Version":"1.27.0","DownloadUrl":"https://example.com/a.exe"}"#).await;
    assert!(!service.check_for_update("1.27.0", &CancellationToken::new()).await.unwrap().update_available);

    let (_server, service, _folder) = service_for(r#"{"version":"1.27.0-beta.1","downloadUrl":"https://example.com/a.exe"}"#).await;
    let result = service.check_for_update("1.27.0", &CancellationToken::new()).await.unwrap();
    assert!(!result.update_available);
    assert!(result.manifest.is_some());
}

#[tokio::test]
async fn a_manifest_without_a_download_link_is_nothing_to_act_on() {
    let (_server, service, _folder) = service_for(r#"{"version":"9.0.0"}"#).await;
    let result = service.check_for_update("1.27.0", &CancellationToken::new()).await.unwrap();
    assert!(!result.update_available);
    assert_eq!(result.manifest, None);
}

#[tokio::test]
async fn an_unreadable_manifest_or_an_unreachable_endpoint_is_an_error() {
    let (_server, service, _folder) = service_for("<html>maintenance</html>").await;
    assert_eq!(
        service.check_for_update("1.27.0", &CancellationToken::new()).await.unwrap_err().to_string(),
        "The update manifest could not be read."
    );

    let server = TestServer::start(|_, _| Reply::status(503)).await;
    let service = UpdateService::with_client(Some(&server.url("/latest.json")), None, direct_client());
    assert!(service.check_for_update("1.27.0", &CancellationToken::new()).await.is_err());
}

#[tokio::test]
async fn an_installer_not_served_over_https_is_never_fetched() {
    let server = TestServer::start(|_, _| Reply::ok(b"MZ")).await;
    let folder = TempFolder::new();
    let service = UpdateService::with_client(None, Some(folder.path.join("updates")), direct_client());
    let manifest = UpdateManifest { version: "9.0.0".into(), download_url: server.url("/QuickByte-9.0.0-Setup.exe"), ..Default::default() };

    let error = service.download_installer(&manifest, None, &CancellationToken::new()).await.unwrap_err();

    assert_eq!(error.to_string(), "The update download link is not a valid HTTPS URL.");
    assert!(server.requests().is_empty());
}
