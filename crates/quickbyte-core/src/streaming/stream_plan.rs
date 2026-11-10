//! Port of `StreamPlan.cs`: turns a playlist into the list of pieces to fetch,
//! refusing — with a message a user can read — the streams that cannot be
//! downloaded at all.
//!
//! Pure, and separate from the service that fetches the pieces, because this is
//! where a stream is judged: a live playlist that would be downloaded forever, a
//! DRM scheme whose pieces are noise without a licence, a key or a piece at an
//! address that is not the web. Getting any of those wrong does not fail loudly;
//! it produces a file that will not play.

use std::sync::Arc;

use url::Url;

use super::dash_manifest::{self, DashManifest};
use super::hls_playlist::{ByteRange, HlsInit, HlsKey, HlsPlaylist, HlsSegment};
use crate::error::{CoreError, Result};

/// The AES-128 key a piece is encrypted with, and the IV for this piece.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamKey {
    pub uri: Url,
    pub iv: [u8; 16],
}

/// One request's worth of a stream: an address, the bytes of it to take (all of
/// them when `range` is `None`), and how to decrypt them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamPiece {
    pub uri: Url,
    pub range: Option<ByteRange>,
    pub key: Option<StreamKey>,
    pub is_initialization: bool,
}

/// Every piece of one output file, in the order they are joined.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamTrack {
    pub pieces: Vec<StreamPiece>,
}

/// More pieces than this in one track is a malformed or hostile playlist, not a video.
pub const MAX_PIECES: usize = dash_manifest::MAX_SEGMENTS;

// Short enough for the Add window's status line, which is where they are read first.
pub const LIVE_MESSAGE: &str = "This is a live stream, which has no end to download to.";
pub const PROTECTED_MESSAGE: &str = "This video is DRM-protected, so it cannot be saved.";

/// The pieces of an HLS media playlist. Fails with `StreamNotSupported` for a
/// live or protected stream, and `Invalid` for a master playlist, an empty one,
/// or a piece that is not on the web.
pub fn for_hls(playlist: &HlsPlaylist) -> Result<StreamTrack> {
    if playlist.is_master() {
        return Err(CoreError::invalid("This is a list of qualities, not the pieces of one."));
    }
    if !playlist.is_complete {
        return Err(CoreError::StreamNotSupported(LIVE_MESSAGE.into()));
    }
    if playlist.segments.is_empty() {
        return Err(CoreError::invalid("The playlist lists no pieces."));
    }
    if playlist.segments.len() > MAX_PIECES {
        return Err(CoreError::invalid("The playlist lists more pieces than a video could have."));
    }

    let mut pieces = Vec::with_capacity(playlist.segments.len() + 1);
    let mut last_init: Option<&Arc<HlsInit>> = None;

    for segment in &playlist.segments {
        // A new initialization section goes in front of the first piece it
        // applies to. It changes only at a discontinuity, typically an advert.
        if let Some(init) = &segment.init {
            if !last_init.is_some_and(|last| Arc::ptr_eq(last, init)) {
                pieces.push(StreamPiece {
                    uri: web(&init.uri)?,
                    range: init.range,
                    key: key_for(init.key.as_deref(), segment)?,
                    is_initialization: true,
                });
                last_init = Some(init);
            }
        }
        pieces.push(StreamPiece {
            uri: web(&segment.uri)?,
            range: segment.range,
            key: key_for(segment.key.as_deref(), segment)?,
            is_initialization: false,
        });
    }
    Ok(StreamTrack { pieces })
}

/// The pieces of one representation of a DASH manifest. Fails with
/// `StreamNotSupported` for a live or protected manifest, and `Invalid` when the
/// representation is gone or lists no pieces.
pub fn for_dash(manifest: &DashManifest, representation_id: &str) -> Result<StreamTrack> {
    if manifest.is_live {
        return Err(CoreError::StreamNotSupported(LIVE_MESSAGE.into()));
    }
    if manifest.is_protected {
        return Err(CoreError::StreamNotSupported(PROTECTED_MESSAGE.into()));
    }
    let representation = manifest
        .representations
        .iter()
        .find(|r| r.id == representation_id)
        .ok_or_else(|| CoreError::invalid("The video no longer offers the quality this download was started with."))?;
    if representation.segments.is_empty() {
        return Err(CoreError::invalid("The manifest lists no pieces for this quality."));
    }

    let mut pieces = Vec::with_capacity(representation.segments.len() + 1);
    if let Some(init) = &representation.initialization {
        pieces.push(StreamPiece { uri: web(&init.uri)?, range: init.range, key: None, is_initialization: true });
    }
    for s in &representation.segments {
        pieces.push(StreamPiece { uri: web(&s.uri)?, range: s.range, key: None, is_initialization: false });
    }
    Ok(StreamTrack { pieces })
}

fn key_for(key: Option<&HlsKey>, segment: &HlsSegment) -> Result<Option<StreamKey>> {
    let Some(key) = key else { return Ok(None) };

    // SAMPLE-AES and its relatives encrypt inside the media, under a key a DRM
    // system hands out: FairPlay's skd:// is the common case.
    if !key.is_aes128() {
        return Err(CoreError::StreamNotSupported(PROTECTED_MESSAGE.into()));
    }
    let Some(uri) = key.uri.as_ref().filter(|u| is_web(u)) else {
        return Err(CoreError::StreamNotSupported(PROTECTED_MESSAGE.into()));
    };
    Ok(Some(StreamKey { uri: uri.clone(), iv: HlsPlaylist::iv_with(Some(key), segment.sequence_number) }))
}

/// Only http and https. A playlist is a document from the internet, and a piece
/// or key it points at `file:` would have this read the user's own disk.
fn web(uri: &Url) -> Result<Url> {
    if is_web(uri) {
        Ok(uri.clone())
    } else {
        Err(CoreError::invalid(format!("A piece of this stream is not on the web ({}:).", uri.scheme())))
    }
}

pub fn is_web(uri: &Url) -> bool {
    matches!(uri.scheme(), "http" | "https")
}

#[cfg(test)]
mod tests {
    //! Each refusal here is a download that would otherwise run to 100% and
    //! produce a file that does not play — or, for a live stream, never finish.

    use super::*;

    fn hls(text: &str) -> HlsPlaylist {
        HlsPlaylist::parse(text, &Url::parse("https://cdn.example.com/v/index.m3u8").unwrap()).unwrap()
    }

    fn file_name(uri: &Url) -> &str {
        uri.path().rsplit('/').next().unwrap_or_default()
    }

    #[test]
    fn a_live_playlist_is_refused() {
        match for_hls(&hls("#EXTM3U\n#EXTINF:4,\na.ts\n")) {
            Err(CoreError::StreamNotSupported(message)) => assert_eq!(message, LIVE_MESSAGE),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn protected_pieces_are_refused() {
        for key_tag in [
            r#"#EXT-X-KEY:METHOD=SAMPLE-AES,URI="https://keys.example.com/k""#,
            r#"#EXT-X-KEY:METHOD=AES-128,URI="skd://fairplay-asset""#,
            "#EXT-X-KEY:METHOD=AES-128",
        ] {
            match for_hls(&hls(&format!("#EXTM3U\n{key_tag}\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n"))) {
                Err(CoreError::StreamNotSupported(message)) => assert_eq!(message, PROTECTED_MESSAGE),
                other => panic!("{key_tag}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_piece_that_is_not_on_the_web_is_refused() {
        // A playlist is a document from the internet; it must not be able to have
        // the download read the user's own files.
        let result = for_hls(&hls("#EXTM3U\n#EXTINF:4,\nfile:///C:/Users/me/secret.txt\n#EXT-X-ENDLIST\n"));
        assert!(matches!(result, Err(CoreError::Invalid(_))));
    }

    #[test]
    fn a_master_or_empty_playlist_is_not_a_track() {
        assert!(matches!(for_hls(&hls("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n")), Err(CoreError::Invalid(_))));
        assert!(matches!(for_hls(&hls("#EXTM3U\n#EXT-X-ENDLIST\n")), Err(CoreError::Invalid(_))));
    }

    #[test]
    fn an_initialization_section_goes_in_front_of_the_pieces_it_applies_to_once() {
        let track = for_hls(&hls(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init-a.mp4\"\n#EXTINF:4,\na1.m4s\n#EXTINF:4,\na2.m4s\n#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"init-b.mp4\"\n#EXTINF:4,\nb1.m4s\n#EXT-X-ENDLIST",
        ))
        .unwrap();
        assert_eq!(
            track.pieces.iter().map(|p| file_name(&p.uri)).collect::<Vec<_>>(),
            ["init-a.mp4", "a1.m4s", "a2.m4s", "init-b.mp4", "b1.m4s"]
        );
        assert_eq!(track.pieces.iter().map(|p| p.is_initialization).collect::<Vec<_>>(), [true, false, false, true, false]);
    }

    #[test]
    fn an_encrypted_piece_carries_its_key_and_the_iv_for_its_own_sequence_number() {
        let track = for_hls(&hls(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:5\n#EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.example.com/k\"\n#EXTINF:4,\na.ts\n#EXTINF:4,\nb.ts\n#EXT-X-ENDLIST",
        ))
        .unwrap();
        for piece in &track.pieces {
            assert_eq!(piece.key.as_ref().unwrap().uri.as_str(), "https://keys.example.com/k");
        }
        assert_eq!(track.pieces[0].key.as_ref().unwrap().iv[15], 5);
        assert_eq!(track.pieces[1].key.as_ref().unwrap().iv[15], 6);
    }

    #[test]
    fn a_dash_representation_starts_with_its_initialization() {
        let manifest = DashManifest::parse(
            r#"<MPD mediaPresentationDuration="PT8S"><Period><AdaptationSet mimeType="video/mp4">
              <SegmentTemplate duration="4" initialization="init.mp4" media="$Number$.m4s"/>
              <Representation id="v" bandwidth="1"/>
            </AdaptationSet></Period></MPD>"#,
            &Url::parse("https://cdn.example.com/m.mpd").unwrap(),
        )
        .unwrap();
        let track = for_dash(&manifest, "v").unwrap();
        assert_eq!(track.pieces.iter().map(|p| file_name(&p.uri)).collect::<Vec<_>>(), ["init.mp4", "1.m4s", "2.m4s"]);
        assert!(track.pieces[0].is_initialization);
    }

    #[test]
    fn a_quality_the_manifest_no_longer_offers_is_reported_as_such() {
        let manifest = DashManifest::parse("<MPD><Period/></MPD>", &Url::parse("https://cdn.example.com/m.mpd").unwrap()).unwrap();
        match for_dash(&manifest, "gone") {
            Err(CoreError::Invalid(message)) => assert!(message.contains("no longer offers")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn live_and_protected_dash_manifests_are_refused_with_their_messages() {
        let live = DashManifest { is_live: true, ..Default::default() };
        let drm = DashManifest { is_protected: true, ..Default::default() };
        assert!(matches!(for_dash(&live, "v"), Err(CoreError::StreamNotSupported(m)) if m == LIVE_MESSAGE));
        assert!(matches!(for_dash(&drm, "v"), Err(CoreError::StreamNotSupported(m)) if m == PROTECTED_MESSAGE));
    }
}
