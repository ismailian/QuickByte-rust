//! Playlist fetching for `StreamResolver` (the parts of `StreamResolverTests.cs`
//! that do not need the HLS/DASH parsers): the playlist is asked for as the
//! download will be, and a page where a playlist should be is not one.

use std::collections::HashMap;
use std::sync::Arc;

use quickbyte_core::CoreError;
use quickbyte_core::models::{Headers, RequestOptions, StreamFormat};
use quickbyte_core::streaming::StreamDescription;
use quickbyte_core::streaming::stream_plan::LIVE_MESSAGE;
use quickbyte_core::streaming::stream_fetch;
use quickbyte_core::streaming::stream_resolver::StreamResolver;
use quickbyte_core::traits::StreamResolver as _;
use tokio_util::sync::CancellationToken;

use crate::support::{Reply, TestServer, direct_client};

const PLAYLIST: &str = "#EXTM3U\n#EXTINF:4,\na.ts\n#EXTINF:4,\nb.ts\n#EXT-X-ENDLIST\n";

#[tokio::test]
async fn the_playlist_is_asked_for_with_the_pages_cookies() {
    let server = TestServer::start(|_, _| Reply::ok(PLAYLIST.as_bytes()).header("Content-Type", "application/vnd.apple.mpegurl")).await;
    let mut headers = Headers::new();
    headers.insert("Cookie".into(), "session=42".into());
    let options = RequestOptions { headers: Some(headers), ..Default::default() };

    let url = quickbyte_core::net::http_client::parse_url(&server.url("/show/index.m3u8")).unwrap();
    let (text, final_url) = stream_fetch::get_text(&direct_client(), &url, Some(&options), &CancellationToken::new()).await.unwrap();

    assert_eq!(text, PLAYLIST);
    assert_eq!(final_url, url);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].header("Cookie").as_deref(), Some("session=42"));
}

#[tokio::test]
async fn relative_pieces_resolve_against_where_the_playlist_finally_came_from() {
    let server = TestServer::start(|asked, _| match asked.path.as_str() {
        "/play" => Reply::redirect("/cdn/v1/index.m3u8"),
        _ => Reply::ok(PLAYLIST.as_bytes()),
    })
    .await;
    let url = quickbyte_core::net::http_client::parse_url(&server.url("/play")).unwrap();
    let (_, final_url) = stream_fetch::get_text(&direct_client(), &url, None, &CancellationToken::new()).await.unwrap();
    assert!(final_url.as_str().ends_with("/cdn/v1/index.m3u8"));
}

#[tokio::test]
async fn a_playlist_behind_a_login_asks_for_one() {
    let server = TestServer::start(|_, _| Reply::status(401)).await;
    let url = quickbyte_core::net::http_client::parse_url(&server.url("/index.m3u8")).unwrap();
    let error = stream_fetch::get_text(&direct_client(), &url, None, &CancellationToken::new()).await.unwrap_err();
    assert!(matches!(error, CoreError::AuthenticationRequired { credentials_were_supplied: false, .. }));
}

#[tokio::test]
async fn something_too_large_to_be_a_playlist_is_refused_before_it_is_read() {
    let server = TestServer::start(|_, _| Reply::ok(b"#EXTM3U").declared(20 * 1024 * 1024)).await;
    let url = quickbyte_core::net::http_client::parse_url(&server.url("/huge.m3u8")).unwrap();
    let error = stream_fetch::get_text(&direct_client(), &url, None, &CancellationToken::new()).await.unwrap_err();
    assert_eq!(error.to_string(), "This is too large to be a playlist.");
}

#[tokio::test]
async fn a_playlist_off_the_web_is_refused() {
    let url = url::Url::parse("file:///C:/Windows/win.ini").unwrap();
    let error = stream_fetch::get_text(&direct_client(), &url, None, &CancellationToken::new()).await.unwrap_err();
    assert_eq!(error.to_string(), "A stream's playlist has to be on the web (http or https).");
}

#[tokio::test]
async fn a_page_where_a_playlist_should_be_is_not_a_stream() {
    let server = TestServer::start(|_, _| Reply::ok(b"<html>Please sign in</html>").header("Content-Type", "text/html")).await;
    let error = StreamResolver::with_client(direct_client())
        .describe(&server.url("/show/expired.m3u8"), &RequestOptions::none(), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, CoreError::Invalid(_)), "{error}");
}

// ------------------------------- the resolver, end to end (StreamResolverTests.cs) --

/// Addresses served from memory under `/show/`, as the C# `StubWeb` served them.
async fn web(files: &[(&str, &str, &str)]) -> TestServer {
    let files: Arc<HashMap<String, (String, String)>> =
        Arc::new(files.iter().map(|(path, body, kind)| (format!("/show/{path}"), (body.to_string(), kind.to_string()))).collect());
    TestServer::start(move |asked, _| match files.get(&asked.path) {
        Some((body, kind)) => Reply::ok(body.as_bytes()).header("Content-Type", kind),
        None => Reply::status(404),
    })
    .await
}

const HLS: &str = "application/vnd.apple.mpegurl";

fn fmp4_media(prefix: &str) -> String {
    format!("#EXTM3U\n#EXT-X-MAP:URI=\"{prefix}-init.mp4\"\n#EXTINF:4,\n{prefix}1.m4s\n#EXT-X-ENDLIST\n")
}

async fn describe(server: &TestServer, path: &str) -> Result<StreamDescription, CoreError> {
    StreamResolver::with_client(direct_client())
        .describe(&server.url(&format!("/show/{path}")), &RequestOptions::none(), &CancellationToken::new())
        .await
}

#[tokio::test]
async fn a_master_playlist_offers_each_quality_best_first_with_its_separate_sound() {
    let master = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Commentary\",URI=\"audio/commentary.m3u8\"\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Main\",DEFAULT=YES,URI=\"audio/main.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,AUDIO=\"a\"\n360.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=5200000,RESOLUTION=1920x1080,AUDIO=\"a\"\n1080.m3u8\n";
    let (video, sound) = (fmp4_media("v"), fmp4_media("a"));
    let server = web(&[("master.m3u8", master, HLS), ("1080.m3u8", &video, HLS), ("audio/main.m3u8", &sound, HLS)]).await;
    let root = server.url("/show/");

    let description = describe(&server, "master.m3u8").await.unwrap();

    assert_eq!(description.format, StreamFormat::Hls);
    let labels: Vec<&str> = description.choices.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, vec!["1080p · 5.2 Mbps", "360p · 800 kbps", "Audio only"]);

    let best = &description.choices[0];
    assert_eq!(best.source.main_playlist_url, Some(format!("{root}1080.m3u8")));
    assert_eq!(best.source.audio_playlist_url, Some(format!("{root}audio/main.m3u8")));
    assert_eq!(best.extension, ".mp4");
    assert_eq!(best.source.audio_extension.as_deref(), Some(".m4a"));
    assert!(best.source.has_separate_audio());

    // Only the best quality and its sound are read; the rest are packaged alike.
    assert!(!server.requests().iter().any(|r| r.path == "/show/360.m3u8"));

    let sound = &description.choices[2];
    assert_eq!(sound.source.main_playlist_url, Some(format!("{root}audio/main.m3u8")));
    assert!(!sound.source.has_separate_audio());
    assert_eq!(sound.extension, ".m4a");
}

#[tokio::test]
async fn the_same_picture_with_different_sound_is_labelled_by_its_sound() {
    let master = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",DEFAULT=YES,URI=\"aac.m3u8\"\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"ac3\",NAME=\"English\",DEFAULT=YES,URI=\"ac3.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,ac-3\",AUDIO=\"ac3\"\n1080-ac3.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=7900000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\",AUDIO=\"aac\"\n1080-aac.m3u8\n";
    let (video, sound) = (fmp4_media("v"), fmp4_media("a"));
    let server = web(&[("master.m3u8", master, HLS), ("1080-ac3.m3u8", &video, HLS), ("ac3.m3u8", &sound, HLS)]).await;

    let description = describe(&server, "master.m3u8").await.unwrap();

    let labels: Vec<&str> = description.choices.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, vec!["1080p · 8 Mbps · AC-3", "1080p · 7.9 Mbps · AAC", "Audio only · AC-3"]);
}

#[tokio::test]
async fn a_single_media_playlist_is_offered_as_it_is() {
    let server = web(&[("index.m3u8", PLAYLIST, HLS)]).await;

    let description = describe(&server, "index.m3u8").await.unwrap();

    assert_eq!(description.choices.len(), 1);
    let only = &description.choices[0];
    assert_eq!(only.source.main_playlist_url, Some(server.url("/show/index.m3u8")));
    assert_eq!(only.extension, ".ts");
    assert_eq!(only.content_type, "video/mp2t");
    assert!(!only.source.has_separate_audio());
}

#[tokio::test]
async fn a_live_stream_is_refused_before_it_is_added() {
    let server = web(&[
        ("master.m3u8", "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nlive.m3u8\n", HLS),
        ("live.m3u8", "#EXTM3U\n#EXTINF:4,\na.ts\n", HLS),
    ])
    .await;

    let error = describe(&server, "master.m3u8").await.unwrap_err();

    assert!(matches!(&error, CoreError::StreamNotSupported(m) if m == LIVE_MESSAGE), "{error}");
}

#[tokio::test]
async fn a_dash_manifest_offers_each_video_with_the_best_sound_and_the_sound_alone() {
    let manifest = r#"<MPD mediaPresentationDuration="PT8S"><Period>
          <AdaptationSet contentType="video" mimeType="video/mp4">
            <SegmentTemplate duration="4" media="$RepresentationID$/$Number$.m4s"/>
            <Representation id="v720" bandwidth="2500000" height="720"/>
            <Representation id="v1080" bandwidth="5000000" height="1080"/>
          </AdaptationSet>
          <AdaptationSet contentType="audio" mimeType="audio/mp4">
            <SegmentTemplate duration="4" media="$RepresentationID$/$Number$.m4s"/>
            <Representation id="a64" bandwidth="64000"/>
            <Representation id="a128" bandwidth="128000"/>
          </AdaptationSet>
        </Period></MPD>"#;
    let server = web(&[("manifest.mpd", manifest, "application/dash+xml")]).await;

    let description = describe(&server, "manifest.mpd").await.unwrap();

    let labels: Vec<&str> = description.choices.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, vec!["1080p · 5 Mbps", "720p · 2.5 Mbps", "Audio only · 128 kbps"]);
    assert!(description.choices[..2].iter().all(|c| c.source.audio_representation.as_deref() == Some("a128")));
    assert_eq!(description.choices[0].source.main_representation.as_deref(), Some("v1080"));
    let audio_only = &description.choices[2];
    assert_eq!(audio_only.source.main_representation.as_deref(), Some("a128"));
    assert_eq!(audio_only.source.audio_representation, None);
    assert_eq!(audio_only.extension, ".m4a");
}

#[tokio::test]
async fn a_protected_dash_manifest_is_refused() {
    let manifest = r#"<MPD><Period><AdaptationSet mimeType="video/mp4">
          <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011" value="cenc"/>
          <Representation id="v" bandwidth="1"><BaseURL>v.mp4</BaseURL></Representation>
        </AdaptationSet></Period></MPD>"#;
    let server = web(&[("drm.mpd", manifest, "application/dash+xml")]).await;

    assert!(matches!(describe(&server, "drm.mpd").await, Err(CoreError::StreamNotSupported(_))));
}

#[tokio::test]
async fn the_resolver_asks_for_the_playlist_with_the_pages_cookies() {
    let server = web(&[("index.m3u8", PLAYLIST, HLS)]).await;
    let mut headers = Headers::new();
    headers.insert("Cookie".into(), "session=42".into());
    let options = RequestOptions { headers: Some(headers), ..Default::default() };

    StreamResolver::with_client(direct_client()).describe(&server.url("/show/index.m3u8"), &options, &CancellationToken::new()).await.unwrap();

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].header("Cookie").as_deref(), Some("session=42"));
}
