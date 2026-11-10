//! Port of `src/QuickByte.Core/Streaming/StreamResolver.cs` — the qualities a
//! playlist offers.
//!
//! Turns a playlist or manifest address (see [`super::stream_detector`]) into the
//! qualities it offers.
//!
//! A master playlist is read, and so is the best quality's own playlist: that is
//! the only place a live stream, a DRM scheme, or the container (MPEG-TS or
//! fragmented MP4, and so the file's extension) shows itself. Refusing a stream
//! in the Add window beats adding a download that fails the moment it runs. The
//! other qualities of the same video are packaged the same way, so one look is
//! taken for all of them; the engine checks each for itself when it runs.
//!
//! The decisions — which qualities, in which order, labelled how, with which
//! sound and which extension — are made here on the small `*Info` shapes below.
//! Filling those from a parsed playlist (the HLS/DASH parsers and the stream
//! plan, which decides what is refused) is the job of the `integration` module
//! at the bottom, which keeps these rules testable without a playlist to parse.

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::{CoreError, Result};
use crate::helpers::file_name_helper;
use crate::models::{RequestOptions, StreamFormat, StreamSource};
use crate::net::http_client::HttpClient;
use crate::streaming::stream_fetch::{self, is_web};

/// One quality a playlist offers: `1080p · 5.2 Mbps`.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamChoice {
    /// As offered to the user: "1080p · 5.2 Mbps".
    pub label: String,
    pub bandwidth: i64,
    pub source: StreamSource,
    /// The main file's extension, with its dot.
    pub extension: String,
    /// What the main file will be, for its category and icon.
    pub content_type: String,
}

/// Every quality, best first.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamDescription {
    pub format: StreamFormat,
    pub choices: Vec<StreamChoice>,
}

// ---------------------------------------------------------------------------
// What the resolver needs from a parsed playlist
// ---------------------------------------------------------------------------

/// One `#EXT-X-STREAM-INF` of a master playlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HlsVariantInfo {
    pub uri: Url,
    pub bandwidth: i64,
    /// `1920x1080`.
    pub resolution: Option<String>,
    pub codecs: Option<String>,
    pub audio_group: Option<String>,
}

/// One `#EXT-X-MEDIA` of a master playlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HlsRenditionInfo {
    /// `AUDIO`, `SUBTITLES`, …
    pub kind: String,
    pub group_id: String,
    pub uri: Option<Url>,
    pub is_default: bool,
}

/// A media playlist that has passed the engine's checks (complete, not
/// protected, every piece on the web): what its container is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HlsMediaInfo {
    /// Segments come with `#EXT-X-MAP`: fragmented MP4 rather than MPEG-TS.
    pub is_fragmented_mp4: bool,
    /// The last piece's address, whose extension names the container otherwise.
    pub last_piece: Url,
}

/// One `Representation` of a DASH manifest's first period.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DashRepresentationInfo {
    pub id: String,
    pub content_type: String,
    pub mime_type: String,
    pub bandwidth: i64,
    pub height: i32,
    pub segment_count: usize,
    pub all_segments_on_web: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DashInfo {
    pub is_live: bool,
    pub is_protected: bool,
    pub representations: Vec<DashRepresentationInfo>,
}

/// What an address turned out to hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamDocument {
    HlsMaster { variants: Vec<HlsVariantInfo>, renditions: Vec<HlsRenditionInfo> },
    HlsMedia(HlsMediaInfo),
    Dash(DashInfo),
}

// ---------------------------------------------------------------------------
// The resolver
// ---------------------------------------------------------------------------

pub struct StreamResolver {
    http: Arc<HttpClient>,
}

impl Default for StreamResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamResolver {
    pub fn new() -> Self {
        Self { http: HttpClient::shared() }
    }

    /// For the tests: a playlist that is live, protected or oddly packaged cannot
    /// be summoned from a real server.
    pub fn with_client(http: Arc<HttpClient>) -> Self {
        Self { http }
    }

    async fn read_media(&self, url: &Url, options: Option<&RequestOptions>, cancel: &CancellationToken) -> Result<HlsMediaInfo> {
        let (text, final_url) = stream_fetch::get_text(&self.http, url, options, cancel).await?;
        integration::read_media(&text, &final_url)
    }
}

#[async_trait]
impl crate::traits::StreamResolver for StreamResolver {
    async fn describe(&self, url: &str, options: &RequestOptions, cancel: &CancellationToken) -> Result<StreamDescription> {
        let parsed = Url::parse(url).map_err(|_| CoreError::invalid("This is not a web address."))?;
        let options = Some(options);
        let (text, final_url) = stream_fetch::get_text(&self.http, &parsed, options, cancel).await?;

        match integration::read_document(&text, &final_url)? {
            StreamDocument::HlsMedia(media) => Ok(describe_hls_media(&media, &final_url)),
            StreamDocument::HlsMaster { variants, renditions } => {
                describe_hls_master(&variants, &renditions, |uri| async move { self.read_media(&uri, options, cancel).await }).await
            }
            StreamDocument::Dash(manifest) => describe_dash(&manifest),
        }
    }
}

// ------------------------------------------------------------------- HLS --

/// A single quality: the address is its own media playlist.
pub fn describe_hls_media(media: &HlsMediaInfo, playlist_url: &Url) -> StreamDescription {
    let source = StreamSource {
        format: StreamFormat::Hls,
        main_playlist_url: Some(playlist_url.to_string()),
        quality: "Original".into(),
        ..Default::default()
    };
    let (extension, content_type) = hls_container(media, false);
    StreamDescription {
        format: StreamFormat::Hls,
        choices: vec![StreamChoice { label: "Original".into(), bandwidth: 0, source, extension: extension.into(), content_type: content_type.into() }],
    }
}

/// A master playlist: every quality, best first, each with its separate sound,
/// plus the sound on its own. `read_media` fetches and checks one media playlist.
pub async fn describe_hls_master<F, Fut>(variants: &[HlsVariantInfo], renditions: &[HlsRenditionInfo], mut read_media: F) -> Result<StreamDescription>
where
    F: FnMut(Url) -> Fut,
    Fut: Future<Output = Result<HlsMediaInfo>>,
{
    let mut ordered: Vec<&HlsVariantInfo> = variants.iter().filter(|v| is_web(&v.uri)).collect();
    ordered.sort_by_key(|v| std::cmp::Reverse(v.bandwidth));
    let mut seen = Vec::new();
    let variants: Vec<&HlsVariantInfo> = ordered
        .into_iter()
        .filter(|v| {
            let key = (v.resolution.clone(), v.bandwidth, audio_for(renditions, v).map(|u| u.to_string()));
            if seen.contains(&key) {
                false
            } else {
                seen.push(key);
                true
            }
        })
        .collect();
    let Some(best) = variants.first().copied() else { return Err(CoreError::invalid("The playlist lists no qualities.")) };

    // The best quality is read, and its sound if separate: that is where a live
    // or protected stream, and the container, show themselves.
    let best_media = read_media(best.uri.clone()).await?;
    let (video_extension, video_type) = hls_container(&best_media, is_audio_only(best));

    let best_audio = audio_for(renditions, best);
    let audio_container = match best_audio {
        Some(uri) => Some(hls_container(&read_media(uri.clone()).await?, true)),
        None => None,
    };
    let audio_extension = audio_container.map(|(extension, _)| extension);

    // The same picture offered with different sound — AAC, AC-3, E-AC-3 — reads
    // as the same line three times unless the label says which is which.
    let mut sounds: Vec<Option<String>> = Vec::new();
    for variant in &variants {
        let sound = audio_for(renditions, variant).map(|u| u.to_string());
        if !sounds.contains(&sound) {
            sounds.push(sound);
        }
    }
    let several_sounds = sounds.len() > 1;

    let mut choices: Vec<StreamChoice> = variants
        .iter()
        .map(|variant| {
            let audio = audio_for(renditions, variant);
            let mut label = label_for(height(variant.resolution.as_deref()), variant.bandwidth, is_audio_only(variant));
            if several_sounds && let Some(codec) = audio_codec_name(variant.codecs.as_deref()) {
                label.push_str(&format!(" · {codec}"));
            }
            let source = StreamSource {
                format: StreamFormat::Hls,
                main_playlist_url: Some(variant.uri.to_string()),
                audio_playlist_url: audio.map(|u| u.to_string()),
                audio_extension: audio.map(|_| audio_extension.unwrap_or(".m4a").to_string()),
                quality: label.clone(),
                ..Default::default()
            };
            // Sound alone in fragmented MP4 is an .m4a; in MPEG-TS it is still a .ts.
            let (extension, content_type) =
                if is_audio_only(variant) && video_extension == ".mp4" { (".m4a", "audio/mp4") } else { (video_extension, video_type) };
            StreamChoice { label, bandwidth: variant.bandwidth, source, extension: extension.into(), content_type: content_type.into() }
        })
        .collect();

    // The sound on its own, for a talk or a concert — offered when it is a
    // playlist of its own and no quality already is only sound.
    if let (Some(audio), Some((extension, content_type))) = (best_audio, audio_container)
        && !variants.iter().any(|v| is_audio_only(v))
    {
        let label = match audio_codec_name(best.codecs.as_deref()) {
            Some(codec) if several_sounds => format!("Audio only · {codec}"),
            _ => "Audio only".to_string(),
        };
        choices.push(StreamChoice {
            label: label.clone(),
            bandwidth: 0,
            source: StreamSource { format: StreamFormat::Hls, main_playlist_url: Some(audio.to_string()), quality: label, ..Default::default() },
            extension: extension.into(),
            content_type: content_type.into(),
        });
    }

    Ok(StreamDescription { format: StreamFormat::Hls, choices })
}

/// The separate sound a variant plays with: its group's default rendition that
/// has a playlist, else the first that does. No playlist in the group means the
/// sound is inside the variant's own pieces.
pub fn audio_for<'a>(renditions: &'a [HlsRenditionInfo], variant: &HlsVariantInfo) -> Option<&'a Url> {
    let group = variant.audio_group.as_deref().filter(|g| !g.is_empty())?;
    let candidates: Vec<&HlsRenditionInfo> = renditions
        .iter()
        .filter(|r| r.kind.eq_ignore_ascii_case("AUDIO") && r.group_id == group && r.uri.as_ref().is_some_and(is_web))
        .collect();
    candidates.iter().find(|r| r.is_default).or_else(|| candidates.first()).and_then(|r| r.uri.as_ref())
}

/// The sound codec named in a variant's CODECS, as a person would name it.
pub fn audio_codec_name(codecs: Option<&str>) -> Option<&'static str> {
    for codec in codecs.unwrap_or("").split(',') {
        let c = codec.trim().to_ascii_lowercase();
        if c.starts_with("mp4a.40.34") {
            return Some("MP3");
        }
        if c.starts_with("mp4a") {
            return Some("AAC");
        }
        if c.starts_with("ac-3") {
            return Some("AC-3");
        }
        if c.starts_with("ec-3") {
            return Some("E-AC-3");
        }
        if c.starts_with("opus") {
            return Some("Opus");
        }
        if c.starts_with("flac") {
            return Some("FLAC");
        }
    }
    None
}

fn is_audio_only(variant: &HlsVariantInfo) -> bool {
    variant.resolution.is_none()
        && variant.codecs.as_deref().is_some_and(|codecs| {
            codecs.split(',').all(|c| {
                let c = c.trim().to_ascii_lowercase();
                c.starts_with("mp4a") || c.starts_with("ac-3") || c.starts_with("ec-3")
            })
        })
}

/// What joining a playlist's pieces produces. An initialization section means
/// fragmented MP4, which plays as an .mp4; otherwise the pieces' own extension
/// says (packed audio is .aac), and MPEG-TS is the default HLS has always had.
pub fn hls_container(media: &HlsMediaInfo, audio_only: bool) -> (&'static str, &'static str) {
    if media.is_fragmented_mp4 {
        return if audio_only { (".m4a", "audio/mp4") } else { (".mp4", "video/mp4") };
    }
    let name = media.last_piece.path().rsplit('/').next().unwrap_or("");
    match file_name_helper::extension_of(name).to_ascii_lowercase().as_str() {
        ".aac" => (".aac", "audio/aac"),
        ".mp3" => (".mp3", "audio/mpeg"),
        ".ac3" => (".ac3", "audio/ac3"),
        ".m4s" | ".mp4" => {
            if audio_only {
                (".m4a", "audio/mp4")
            } else {
                (".mp4", "video/mp4")
            }
        }
        _ => (".ts", "video/mp2t"),
    }
}

// ------------------------------------------------------------------ DASH --

pub fn describe_dash(manifest: &DashInfo) -> Result<StreamDescription> {
    if manifest.is_live {
        return Err(CoreError::StreamNotSupported(LIVE_MESSAGE.into()));
    }
    if manifest.is_protected {
        return Err(CoreError::StreamNotSupported(PROTECTED_MESSAGE.into()));
    }

    let usable: Vec<&DashRepresentationInfo> = manifest.representations.iter().filter(|r| r.segment_count > 0 && r.all_segments_on_web).collect();
    let mut videos: Vec<&DashRepresentationInfo> = usable.iter().copied().filter(|r| is_kind(r, "video")).collect();
    videos.sort_by_key(|r| std::cmp::Reverse(r.bandwidth));
    let mut audios: Vec<&DashRepresentationInfo> = usable.iter().copied().filter(|r| is_kind(r, "audio")).collect();
    audios.sort_by_key(|r| std::cmp::Reverse(r.bandwidth));
    let best_audio = audios.first().copied();

    let mut choices = Vec::new();
    let mut seen = Vec::new();
    for video in videos {
        if seen.contains(&(video.height, video.bandwidth)) {
            continue;
        }
        seen.push((video.height, video.bandwidth));
        let label = label_for(video.height, video.bandwidth, false);
        choices.push(StreamChoice {
            label: label.clone(),
            bandwidth: video.bandwidth,
            source: StreamSource {
                format: StreamFormat::Dash,
                main_representation: Some(video.id.clone()),
                audio_representation: best_audio.map(|a| a.id.clone()),
                audio_extension: best_audio.map(|a| dash_extension(a).to_string()),
                quality: label,
                ..Default::default()
            },
            extension: dash_extension(video).into(),
            content_type: if video.mime_type.is_empty() { "video/mp4".into() } else { video.mime_type.clone() },
        });
    }

    if let Some(audio) = best_audio {
        let label = label_for(0, audio.bandwidth, true);
        choices.push(StreamChoice {
            label: label.clone(),
            bandwidth: audio.bandwidth,
            source: StreamSource { format: StreamFormat::Dash, main_representation: Some(audio.id.clone()), quality: label, ..Default::default() },
            extension: dash_extension(audio).into(),
            content_type: if audio.mime_type.is_empty() { "audio/mp4".into() } else { audio.mime_type.clone() },
        });
    }

    if choices.is_empty() {
        return Err(CoreError::invalid("The manifest offers nothing that can be downloaded."));
    }
    Ok(StreamDescription { format: StreamFormat::Dash, choices })
}

fn is_kind(representation: &DashRepresentationInfo, kind: &str) -> bool {
    representation.content_type.eq_ignore_ascii_case(kind)
        || representation.mime_type.to_ascii_lowercase().starts_with(&format!("{}/", kind.to_ascii_lowercase()))
}

pub fn dash_extension(representation: &DashRepresentationInfo) -> &'static str {
    let audio = is_kind(representation, "audio") && !is_kind(representation, "video");
    if representation.mime_type.to_ascii_lowercase().ends_with("/webm") {
        ".webm"
    } else if audio {
        ".m4a"
    } else {
        ".mp4"
    }
}

// ---------------------------------------------------------------- labels --

/// `RESOLUTION=1920x1080` → 1080.
fn height(resolution: Option<&str>) -> i32 {
    let parts: Vec<&str> = resolution.unwrap_or("").split(['x', 'X']).collect();
    match parts.as_slice() {
        [_, h] if !h.is_empty() && h.bytes().all(|b| b.is_ascii_digit()) => h.parse().unwrap_or(0),
        _ => 0,
    }
}

pub fn label_for(height: i32, bandwidth: i64, audio_only: bool) -> String {
    let mut parts = Vec::new();
    if audio_only {
        parts.push("Audio only".to_string());
    } else if height > 0 {
        parts.push(format!("{height}p"));
    }
    if bandwidth > 0 {
        parts.push(bitrate(bandwidth));
    }
    if parts.is_empty() { "Default".into() } else { parts.join(" · ") }
}

/// `5200000` → `5.2 Mbps`, `800000` → `800 kbps`. Always with a '.' decimal
/// point: the UI is English-only.
pub fn bitrate(bits_per_second: i64) -> String {
    if bits_per_second >= 1_000_000 {
        let tenths = (bits_per_second as f64 / 100_000.0).round() as i64;
        if tenths % 10 == 0 { format!("{} Mbps", tenths / 10) } else { format!("{}.{} Mbps", tenths / 10, tenths % 10) }
    } else {
        format!("{} kbps", (bits_per_second / 1000).max(1))
    }
}

pub use super::stream_plan::{LIVE_MESSAGE, PROTECTED_MESSAGE};

// ---------------------------------------------------------------------------
// Reading the playlists themselves, through the HLS/DASH parsers.
// ---------------------------------------------------------------------------
mod integration {
    use super::*;
    use crate::streaming::dash_manifest::DashManifest;
    use crate::streaming::hls_playlist::HlsPlaylist;
    use crate::streaming::stream_plan;

    /// What a playlist or manifest address holds.
    pub(super) fn read_document(text: &str, base_url: &Url) -> Result<StreamDocument> {
        if HlsPlaylist::looks_like_playlist(text) {
            let playlist = HlsPlaylist::parse(text, base_url)?;
            if !playlist.is_master() {
                return Ok(StreamDocument::HlsMedia(media_info(&playlist)?));
            }
            let variants = playlist
                .variants
                .iter()
                .map(|v| HlsVariantInfo {
                    uri: v.uri.clone(),
                    bandwidth: v.bandwidth,
                    resolution: v.resolution.clone(),
                    codecs: v.codecs.clone(),
                    audio_group: v.audio_group.clone(),
                })
                .collect();
            let renditions = playlist
                .renditions
                .iter()
                .map(|r| HlsRenditionInfo { kind: r.kind.clone(), group_id: r.group_id.clone(), uri: r.uri.clone(), is_default: r.is_default })
                .collect();
            return Ok(StreamDocument::HlsMaster { variants, renditions });
        }

        if DashManifest::looks_like_manifest(text) {
            let manifest = DashManifest::parse(text, base_url)?;
            return Ok(StreamDocument::Dash(DashInfo {
                is_live: manifest.is_live,
                is_protected: manifest.is_protected,
                representations: manifest
                    .representations
                    .iter()
                    .map(|r| DashRepresentationInfo {
                        id: r.id.clone(),
                        content_type: r.content_type.clone(),
                        mime_type: r.mime_type.clone(),
                        bandwidth: r.bandwidth,
                        height: r.height,
                        segment_count: r.segments.len(),
                        all_segments_on_web: r.segments.iter().all(|s| is_web(&s.uri)),
                    })
                    .collect(),
            }));
        }

        Err(CoreError::invalid("This address is not a video playlist QuickByte can read."))
    }

    /// One quality's own media playlist, checked as the engine will check it.
    pub(super) fn read_media(text: &str, base_url: &Url) -> Result<HlsMediaInfo> {
        media_info(&HlsPlaylist::parse(text, base_url)?)
    }

    /// The live / protected / empty / off-the-web refusals are the plan's, so the
    /// Add window refuses exactly what the engine would.
    fn media_info(playlist: &HlsPlaylist) -> Result<HlsMediaInfo> {
        let track = stream_plan::for_hls(playlist)?;
        let last_piece = track.pieces.last().map(|p| p.uri.clone()).ok_or_else(|| CoreError::invalid("The playlist lists no pieces."))?;
        Ok(HlsMediaInfo { is_fragmented_mp4: playlist.is_fragmented_mp4(), last_piece })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "https://cdn.example.com/show/";

    fn at(path: &str) -> Url {
        Url::parse(ROOT).unwrap().join(path).unwrap()
    }

    fn variant(path: &str, bandwidth: i64, resolution: Option<&str>, codecs: Option<&str>, audio: Option<&str>) -> HlsVariantInfo {
        HlsVariantInfo {
            uri: at(path),
            bandwidth,
            resolution: resolution.map(String::from),
            codecs: codecs.map(String::from),
            audio_group: audio.map(String::from),
        }
    }

    fn audio(group: &str, path: &str, is_default: bool) -> HlsRenditionInfo {
        HlsRenditionInfo { kind: "AUDIO".into(), group_id: group.into(), uri: Some(at(path)), is_default }
    }

    fn fmp4(prefix: &str) -> HlsMediaInfo {
        HlsMediaInfo { is_fragmented_mp4: true, last_piece: at(&format!("{prefix}1.m4s")) }
    }

    #[test]
    fn labels_read_as_people_name_qualities() {
        assert_eq!(label_for(1080, 5_200_000, false), "1080p · 5.2 Mbps");
        assert_eq!(label_for(360, 800_000, false), "360p · 800 kbps");
        assert_eq!(label_for(0, 128_000, true), "Audio only · 128 kbps");
        assert_eq!(label_for(0, 0, false), "Default");
        assert_eq!(bitrate(8_000_000), "8 Mbps");
        assert_eq!(bitrate(7_900_000), "7.9 Mbps");
        assert_eq!(bitrate(999), "1 kbps");
    }

    #[test]
    fn codecs_are_named_as_a_person_would() {
        assert_eq!(audio_codec_name(Some("avc1.640028,mp4a.40.2")), Some("AAC"));
        assert_eq!(audio_codec_name(Some("avc1.640028,ac-3")), Some("AC-3"));
        assert_eq!(audio_codec_name(Some("mp4a.40.34")), Some("MP3"));
        assert_eq!(audio_codec_name(Some("avc1.640028")), None);
        assert_eq!(audio_codec_name(None), None);
    }

    #[tokio::test]
    async fn a_master_playlist_offers_each_quality_best_first_with_its_separate_sound() {
        let variants = vec![
            variant("360.m3u8", 800_000, Some("640x360"), None, Some("a")),
            variant("1080.m3u8", 5_200_000, Some("1920x1080"), None, Some("a")),
        ];
        let renditions = vec![audio("a", "audio/commentary.m3u8", false), audio("a", "audio/main.m3u8", true)];
        let read = parking_lot::Mutex::new(Vec::new());

        let description = describe_hls_master(&variants, &renditions, |uri| {
            read.lock().push(uri.to_string());
            async move { Ok(fmp4(if uri.path().contains("audio") { "a" } else { "v" })) }
        })
        .await
        .unwrap();

        assert_eq!(description.format, StreamFormat::Hls);
        let labels: Vec<&str> = description.choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["1080p · 5.2 Mbps", "360p · 800 kbps", "Audio only"]);

        let best = &description.choices[0];
        assert_eq!(best.source.main_playlist_url.as_deref(), Some(&*format!("{ROOT}1080.m3u8")));
        assert_eq!(best.source.audio_playlist_url.as_deref(), Some(&*format!("{ROOT}audio/main.m3u8")));
        assert_eq!(best.extension, ".mp4");
        assert_eq!(best.source.audio_extension.as_deref(), Some(".m4a"));
        assert!(best.source.has_separate_audio());

        // Only the best quality and its sound are read; the rest are packaged alike.
        assert!(!read.lock().iter().any(|u| u.ends_with("360.m3u8")));

        let sound = &description.choices[2];
        assert_eq!(sound.source.main_playlist_url.as_deref(), Some(&*format!("{ROOT}audio/main.m3u8")));
        assert!(!sound.source.has_separate_audio());
        assert_eq!(sound.extension, ".m4a");
    }

    #[tokio::test]
    async fn the_same_picture_with_different_sound_is_labelled_by_its_sound() {
        let variants = vec![
            variant("1080-ac3.m3u8", 8_000_000, Some("1920x1080"), Some("avc1.640028,ac-3"), Some("ac3")),
            variant("1080-aac.m3u8", 7_900_000, Some("1920x1080"), Some("avc1.640028,mp4a.40.2"), Some("aac")),
        ];
        let renditions = vec![audio("aac", "aac.m3u8", true), audio("ac3", "ac3.m3u8", true)];
        let description = describe_hls_master(&variants, &renditions, |_| async { Ok(fmp4("x")) }).await.unwrap();
        let labels: Vec<&str> = description.choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["1080p · 8 Mbps · AC-3", "1080p · 7.9 Mbps · AAC", "Audio only · AC-3"]);
    }

    #[test]
    fn a_single_media_playlist_is_offered_as_it_is() {
        let media = HlsMediaInfo { is_fragmented_mp4: false, last_piece: at("b.ts") };
        let description = describe_hls_media(&media, &at("index.m3u8"));
        assert_eq!(description.choices.len(), 1);
        let only = &description.choices[0];
        assert_eq!(only.source.main_playlist_url.as_deref(), Some(&*format!("{ROOT}index.m3u8")));
        assert_eq!(only.extension, ".ts");
        assert_eq!(only.content_type, "video/mp2t");
        assert!(!only.source.has_separate_audio());
    }

    #[tokio::test]
    async fn a_refusal_reading_the_best_quality_is_the_answer() {
        let variants = vec![variant("live.m3u8", 1, None, None, None)];
        let error = describe_hls_master(&variants, &[], |_| async { Err::<HlsMediaInfo, _>(CoreError::StreamNotSupported(LIVE_MESSAGE.into())) })
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), LIVE_MESSAGE);
    }

    fn representation(id: &str, content_type: &str, mime: &str, bandwidth: i64, height: i32) -> DashRepresentationInfo {
        DashRepresentationInfo {
            id: id.into(),
            content_type: content_type.into(),
            mime_type: mime.into(),
            bandwidth,
            height,
            segment_count: 2,
            all_segments_on_web: true,
        }
    }

    #[test]
    fn a_dash_manifest_offers_each_video_with_the_best_sound_and_the_sound_alone() {
        let manifest = DashInfo {
            is_live: false,
            is_protected: false,
            representations: vec![
                representation("v720", "video", "video/mp4", 2_500_000, 720),
                representation("v1080", "video", "video/mp4", 5_000_000, 1080),
                representation("a64", "audio", "audio/mp4", 64_000, 0),
                representation("a128", "audio", "audio/mp4", 128_000, 0),
            ],
        };
        let description = describe_dash(&manifest).unwrap();
        let labels: Vec<&str> = description.choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["1080p · 5 Mbps", "720p · 2.5 Mbps", "Audio only · 128 kbps"]);
        assert!(description.choices[..2].iter().all(|c| c.source.audio_representation.as_deref() == Some("a128")));
        assert_eq!(description.choices[0].source.main_representation.as_deref(), Some("v1080"));
        let audio_only = &description.choices[2];
        assert_eq!(audio_only.source.main_representation.as_deref(), Some("a128"));
        assert_eq!(audio_only.source.audio_representation, None);
        assert_eq!(audio_only.extension, ".m4a");
    }

    #[test]
    fn a_protected_or_live_dash_manifest_is_refused() {
        let protected = DashInfo { is_live: false, is_protected: true, representations: vec![representation("v", "", "video/mp4", 1, 0)] };
        assert!(matches!(describe_dash(&protected), Err(CoreError::StreamNotSupported(m)) if m == PROTECTED_MESSAGE));
        let live = DashInfo { is_live: true, is_protected: false, representations: vec![] };
        assert!(matches!(describe_dash(&live), Err(CoreError::StreamNotSupported(m)) if m == LIVE_MESSAGE));
    }

    #[test]
    fn containers_follow_the_pieces() {
        let ts = HlsMediaInfo { is_fragmented_mp4: false, last_piece: at("a.ts") };
        assert_eq!(hls_container(&ts, false), (".ts", "video/mp2t"));
        let aac = HlsMediaInfo { is_fragmented_mp4: false, last_piece: at("a.aac") };
        assert_eq!(hls_container(&aac, true), (".aac", "audio/aac"));
        assert_eq!(hls_container(&fmp4("a"), true), (".m4a", "audio/mp4"));
    }

    #[test]
    fn a_page_where_a_playlist_should_be_is_not_a_stream() {
        let error = integration::read_document("<html>Please sign in</html>", &at("expired.m3u8")).unwrap_err();
        assert!(matches!(error, CoreError::Invalid(_)));
    }
}
