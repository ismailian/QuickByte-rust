//! Port of `RemoteFileInfoProviderTests.cs`: the probe, against servers that
//! answer badly.
//!
//! One question decides whether a download is split across connections or runs
//! on one, and getting it wrong in the optimistic direction does not produce a
//! slow download — it produces a failed one. The answers that cause that are
//! not errors in HTTP and cannot be provoked from a real server on demand, so
//! each one is scripted here.

use quickbyte_core::CoreError;
use quickbyte_core::models::{Headers, RemoteFileInfo, RemoteFileVersion, RequestOptions};
use quickbyte_core::net::remote_file_info_provider::RemoteFileInfoProvider;
use quickbyte_core::traits::RemoteFileInfoProvider as _;
use tokio_util::sync::CancellationToken;

use crate::support::{Reply, Step, TestServer, direct_client};

const SIZE: u64 = 130_079_008;

fn head(size: u64, accept_ranges: bool, file_name: Option<&'static str>) -> Step {
    Box::new(move |_| {
        let mut reply = Reply::status(200).declared(size);
        if accept_ranges {
            reply = reply.header("Accept-Ranges", "bytes");
        }
        if let Some(name) = file_name {
            reply = reply.header("Content-Disposition", &format!("attachment; filename=\"{name}\""));
        }
        reply
    })
}

/// A server that honours the probe: one byte, and the total in Content-Range.
fn partial_probe() -> Step {
    Box::new(|_| Reply::partial(&[0x4D], 0, 0, SIZE).header("Accept-Ranges", "bytes"))
}

/// A cache answering a one-byte request with the entire file and a 200, while
/// passing the origin's Accept-Ranges straight through. An Age header is the tell.
fn whole_file_probe(accept_ranges: bool) -> Step {
    Box::new(move |_| {
        let mut reply = Reply::ok(&[0x4D, 0x5A]).declared(SIZE).header("Age", "1800");
        if accept_ranges {
            reply = reply.header("Accept-Ranges", "bytes");
        }
        reply
    })
}

fn status(code: u16) -> Step {
    Box::new(move |_| Reply::status(code))
}

fn hang_up() -> Step {
    Box::new(|_| Reply::hang_up())
}

fn validated(step: Step, etag: Option<&'static str>, last_modified: Option<&'static str>) -> Step {
    Box::new(move |asked| {
        let mut reply = step(asked);
        if let Some(tag) = etag {
            reply = reply.header("ETag", tag);
        }
        if let Some(date) = last_modified {
            reply = reply.header("Last-Modified", date);
        }
        reply
    })
}

fn named(step: Step, file_name: &'static str) -> Step {
    Box::new(move |asked| step(asked).header("Content-Disposition", &format!("attachment; filename=\"{file_name}\"")))
}

async fn probe(server: &TestServer, options: RequestOptions) -> Result<RemoteFileInfo, CoreError> {
    RemoteFileInfoProvider::with_client(direct_client()).get_file_info(&server.url("/setup.exe"), &options, &CancellationToken::new()).await
}

// ------------------------------------------------------ range support is proven --

#[tokio::test]
async fn a_206_to_the_probe_is_what_makes_a_file_segmentable() {
    let server = TestServer::script(vec![head(SIZE, true, None), partial_probe()]).await;
    let info = probe(&server, RequestOptions::none()).await.unwrap();
    assert!(info.supports_range_requests);
    assert!(!info.bypass_cache);
    assert_eq!(info.content_length, SIZE as i64);
}

#[tokio::test]
async fn a_head_that_answered_everything_is_still_made_to_prove_the_range() {
    let server = TestServer::script(vec![head(SIZE, true, Some("setup.exe")), whole_file_probe(true), whole_file_probe(true)]).await;
    let info = probe(&server, RequestOptions::none()).await.unwrap();

    assert!(!info.supports_range_requests);
    assert!(info.server_claims_range_support);
    // Named and sized as before — only the claim about ranges was refused.
    assert_eq!(info.file_name, "setup.exe");
    assert_eq!(info.content_length, SIZE as i64);
}

#[tokio::test]
async fn every_probe_request_says_who_is_asking() {
    let server = TestServer::script(vec![head(SIZE, true, None), whole_file_probe(true), partial_probe()]).await;
    probe(&server, RequestOptions::none()).await.unwrap();

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|r| r.header("User-Agent").is_some_and(|a| !a.trim().is_empty())));
}

#[tokio::test]
async fn a_captured_browser_agent_is_what_the_probe_presents() {
    let server = TestServer::script(vec![head(SIZE, true, None), partial_probe()]).await;
    let mut headers = Headers::new();
    headers.insert("User-Agent".into(), "Mozilla/5.0 (a browser)".into());
    probe(&server, RequestOptions { headers: Some(headers), ..Default::default() }).await.unwrap();

    assert!(server.requests().iter().all(|r| r.headers_named("User-Agent") == vec!["Mozilla/5.0 (a browser)"]));
}

// ------------------------------------------------- a cache that cannot slice a file --

#[tokio::test]
async fn a_cache_that_answers_a_range_with_the_whole_file_is_asked_past() {
    let server = TestServer::script(vec![head(SIZE, true, None), whole_file_probe(true), partial_probe()]).await;
    let info = probe(&server, RequestOptions::none()).await.unwrap();
    assert!(info.supports_range_requests);
    // Recorded, because the connections have to ask the same way.
    assert!(info.bypass_cache);
}

#[tokio::test]
async fn the_second_probe_is_the_one_that_asks_the_origin() {
    let server = TestServer::script(vec![head(SIZE, true, None), whole_file_probe(true), partial_probe()]).await;
    probe(&server, RequestOptions::none()).await.unwrap();

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].header("Cache-Control"), None);
    assert_eq!(requests[2].header("Cache-Control").as_deref(), Some("no-cache"));
    // Pragma too: the HTTP/1.0 spelling some intermediaries still act on.
    assert_eq!(requests[2].header("Pragma").as_deref(), Some("no-cache"));
}

#[tokio::test]
async fn a_server_that_never_claimed_ranges_is_not_asked_twice() {
    let server = TestServer::script(vec![head(SIZE, false, None), whole_file_probe(false)]).await;
    let info = probe(&server, RequestOptions::none()).await.unwrap();
    assert!(!info.supports_range_requests);
    assert!(!info.bypass_cache);
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn a_download_that_already_knows_to_bypass_the_cache_probes_that_way_once() {
    let server = TestServer::script(vec![head(SIZE, true, None), partial_probe()]).await;
    let info = probe(&server, RequestOptions { bypass_cache: true, ..Default::default() }).await.unwrap();

    assert!(info.supports_range_requests);
    assert!(info.bypass_cache);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|r| r.header("Cache-Control").as_deref() == Some("no-cache")));
}

#[tokio::test]
async fn a_bypassing_probe_still_refused_a_range_leaves_the_file_on_one_connection() {
    let server = TestServer::script(vec![head(SIZE, true, None), whole_file_probe(true), whole_file_probe(true)]).await;
    let info = probe(&server, RequestOptions { bypass_cache: true, ..Default::default() }).await.unwrap();
    assert!(!info.supports_range_requests);
    assert_eq!(server.requests().len(), 2);
}

// ------------------------------------------------------- when the probe cannot --

#[tokio::test]
async fn a_probe_that_fails_after_a_complete_head_keeps_what_the_head_resolved() {
    let server = TestServer::script(vec![head(SIZE, true, Some("setup.exe")), hang_up()]).await;
    let info = probe(&server, RequestOptions::none()).await.unwrap();
    assert_eq!(info.file_name, "setup.exe");
    assert_eq!(info.content_length, SIZE as i64);
    assert!(!info.supports_range_requests);
}

#[tokio::test]
async fn a_probe_that_fails_when_the_head_did_not_answer_is_reported() {
    let server = TestServer::script(vec![status(405), hang_up()]).await;
    assert!(matches!(probe(&server, RequestOptions::none()).await, Err(CoreError::Http { .. })));
}

#[tokio::test]
async fn a_401_on_the_probe_is_a_credentials_prompt_rather_than_a_failure() {
    let server = TestServer::script(vec![status(405), status(401)]).await;
    let error = probe(&server, RequestOptions::none()).await.unwrap_err();
    assert!(matches!(error, CoreError::AuthenticationRequired { credentials_were_supplied: false, .. }));
    assert_eq!(error.to_string(), "This file needs a user name and password.");
}

#[tokio::test]
async fn a_401_survives_a_complete_head_rather_than_being_swallowed_as_a_probe_failure() {
    let server = TestServer::script(vec![head(SIZE, true, Some("setup.exe")), status(401)]).await;
    assert!(matches!(probe(&server, RequestOptions::none()).await, Err(CoreError::AuthenticationRequired { .. })));
}

#[tokio::test]
async fn a_challenge_to_head_alone_is_not_a_credentials_prompt() {
    // github.com, for anyone signed in to it: HEAD goes to a host that refuses
    // everyone, the same GET goes to one that serves the file.
    for challenge in [401, 407] {
        let server = TestServer::script(vec![status(challenge), partial_probe()]).await;
        let info = probe(&server, RequestOptions::none()).await.unwrap();

        assert!(!info.requires_authentication);
        assert!(info.supports_range_requests);
        assert_eq!(info.content_length, SIZE as i64);
        let methods: Vec<String> = server.requests().iter().map(|r| r.method.clone()).collect();
        assert_eq!(methods, vec!["HEAD", "GET"]);
    }
}

#[tokio::test]
async fn a_challenge_the_probe_repeats_is_still_a_credentials_prompt() {
    let server = TestServer::script(vec![status(401), status(401)]).await;
    assert!(matches!(
        probe(&server, RequestOptions::none()).await,
        Err(CoreError::AuthenticationRequired { credentials_were_supplied: false, .. })
    ));
}

#[tokio::test]
async fn a_rejected_login_on_the_probe_says_it_was_rejected() {
    let server = TestServer::script(vec![status(401), status(401)]).await;
    let options = RequestOptions { credentials: Some(quickbyte_core::models::DownloadCredentials::new("alice", "wrong")), ..Default::default() };
    let error = probe(&server, options).await.unwrap_err();
    assert!(matches!(error, CoreError::AuthenticationRequired { credentials_were_supplied: true, .. }));
    assert_eq!(error.to_string(), "The server rejected that user name or password.");
}

#[tokio::test]
async fn a_proxy_that_wants_a_login_is_never_answered_with_the_sites_sign_in_prompt() {
    let server = TestServer::script(vec![status(407), status(407)]).await;
    let error = probe(&server, RequestOptions::none()).await.unwrap_err();
    assert!(matches!(&error, CoreError::ProxyAuthentication(m) if m.contains("Options > Proxy")));
}

// ---------------------------------------------------------------- what it reads --

#[tokio::test]
async fn the_size_comes_from_content_range_not_from_the_one_byte_body() {
    let server = TestServer::script(vec![status(405), partial_probe()]).await;
    assert_eq!(probe(&server, RequestOptions::none()).await.unwrap().content_length, SIZE as i64);
}

#[tokio::test]
async fn a_name_the_probe_supplies_is_used_when_the_head_named_nothing() {
    let server = TestServer::script(vec![head(SIZE, true, None), named(partial_probe(), "ProtonVPN_v5.1.7_x64.exe")]).await;
    assert_eq!(probe(&server, RequestOptions::none()).await.unwrap().file_name, "ProtonVPN_v5.1.7_x64.exe");
}

#[tokio::test]
async fn a_name_comes_from_the_address_the_redirect_ended_at() {
    // SourceForge's shape: an opaque endpoint that redirects to the real file.
    let server = TestServer::start(|asked, _| match asked.path.as_str() {
        "/setup.exe" => Reply::redirect("/files/7z2301-x64.exe"),
        _ if asked.method == "HEAD" => Reply::status(200).declared(SIZE),
        _ => Reply::partial(&[0x4D], 0, 0, SIZE),
    })
    .await;
    let info = probe(&server, RequestOptions::none()).await.unwrap();
    assert_eq!(info.file_name, "7z2301-x64.exe");
    assert!(info.final_url.ends_with("/files/7z2301-x64.exe"));
}

#[tokio::test]
async fn an_extensionless_name_takes_one_from_the_content_type() {
    let server = TestServer::start(|asked, _| {
        if asked.method == "HEAD" {
            Reply::status(405)
        } else {
            Reply::partial(&[0], 0, 0, 10).header("Content-Type", "video/mp4; charset=binary")
        }
    })
    .await;
    let info = RemoteFileInfoProvider::with_client(direct_client())
        .get_file_info(&server.url("/files/download"), &RequestOptions::none(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(info.file_name, "download.mp4");
    assert_eq!(info.content_type, "video/mp4");
}

// ------------------------------------------------------------------- the version --

const MODIFIED: &str = "Tue, 01 Sep 2026 08:30:00 GMT";

#[tokio::test]
async fn the_validators_are_kept_as_sent_weakness_and_all() {
    let server = TestServer::script(vec![
        validated(head(SIZE, true, None), Some("W/\"v1\""), Some(MODIFIED)),
        validated(partial_probe(), Some("W/\"v1\""), Some(MODIFIED)),
    ])
    .await;
    let info = probe(&server, RequestOptions::none()).await.unwrap();

    assert_eq!(info.etag.as_deref(), Some("W/\"v1\""));
    assert_eq!(info.last_modified.unwrap().to_string(), "2026-09-01T08:30:00+00:00");
    assert_eq!(info.version().host, Some(server.authority()));
}

#[tokio::test]
async fn a_validator_the_head_and_the_probe_disagree_about_is_dropped() {
    let server = TestServer::script(vec![
        validated(head(SIZE, true, None), Some("\"request-1\""), Some(MODIFIED)),
        validated(partial_probe(), Some("\"request-2\""), Some(MODIFIED)),
    ])
    .await;
    let info = probe(&server, RequestOptions::none()).await.unwrap();
    assert_eq!(info.etag, None);
    assert!(info.last_modified.is_some());
}

#[tokio::test]
async fn a_tag_weakened_on_one_response_still_agrees_with_itself() {
    let server = TestServer::script(vec![
        validated(head(SIZE, true, None), Some("\"v1\""), None),
        validated(partial_probe(), Some("W/\"v1\""), None),
    ])
    .await;
    assert_eq!(probe(&server, RequestOptions::none()).await.unwrap().etag.as_deref(), Some("\"v1\""));
}

#[tokio::test]
async fn the_probe_itself_is_never_conditional() {
    let server = TestServer::script(vec![head(SIZE, true, None), partial_probe()]).await;
    let options = RequestOptions { resume_validator: Some(RemoteFileVersion::new(Some("\"v1\"".into()), None, None)), ..Default::default() };
    probe(&server, options).await.unwrap();
    assert!(server.requests().iter().all(|r| r.header("If-Range").is_none()));
}
