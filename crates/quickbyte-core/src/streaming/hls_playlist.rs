//! Port of `HlsPlaylist.cs`: an HLS playlist (RFC 8216) — either a master
//! playlist naming the qualities a video is offered in, or a media playlist
//! listing the segments of one of them.
//!
//! Only what a download needs is kept. Timing, discontinuities and program dates
//! matter to a player; a download concatenates the segments in order, which is
//! how an HLS transport stream is meant to be joined.

use std::collections::HashMap;
use std::sync::Arc;

use url::Url;

use crate::error::{CoreError, Result};

/// A byte range within a resource: `length` bytes from `offset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ByteRange {
    pub offset: i64,
    pub length: i64,
}

impl ByteRange {
    pub fn new(offset: i64, length: i64) -> Self {
        Self { offset, length }
    }
    /// The last byte of the range.
    pub fn end(&self) -> i64 {
        self.offset + self.length - 1
    }
}

/// How a segment is encrypted in an HLS playlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HlsKey {
    pub method: String,
    pub uri: Option<Url>,
    pub iv: Option<[u8; 16]>,
}

impl HlsKey {
    pub fn is_aes128(&self) -> bool {
        self.method.eq_ignore_ascii_case("AES-128")
    }
    pub fn is_none(&self) -> bool {
        self.method.eq_ignore_ascii_case("NONE")
    }
}

/// An `#EXT-X-MAP` initialization section, which fragmented-MP4 segments need in
/// front of them. `key` is the key in force where the tag appears: RFC 8216
/// encrypts the section with it, with an explicit IV.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HlsInit {
    pub uri: Url,
    pub range: Option<ByteRange>,
    pub key: Option<Arc<HlsKey>>,
}

/// One piece of a media playlist. The key and the initialization section are
/// shared (`Arc`) by every segment they apply to, so "the same one as the piece
/// before" is an identity check, as it was in C#.
#[derive(Clone, Debug, PartialEq)]
pub struct HlsSegment {
    pub uri: Url,
    pub duration: f64,
    pub range: Option<ByteRange>,
    pub key: Option<Arc<HlsKey>>,
    pub sequence_number: i64,
    pub init: Option<Arc<HlsInit>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HlsVariant {
    pub uri: Url,
    pub bandwidth: i64,
    pub resolution: Option<String>,
    pub codecs: Option<String>,
    pub audio_group: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HlsRendition {
    pub kind: String,
    pub group_id: String,
    pub name: String,
    pub uri: Option<Url>,
    pub is_default: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HlsPlaylist {
    pub variants: Vec<HlsVariant>,
    pub renditions: Vec<HlsRendition>,
    pub segments: Vec<HlsSegment>,
    /// `#EXT-X-ENDLIST`: the playlist is complete. Without it the stream is live and never ends.
    pub is_complete: bool,
}

struct PendingVariant {
    bandwidth: i64,
    resolution: Option<String>,
    codecs: Option<String>,
    audio: Option<String>,
}

impl HlsPlaylist {
    pub fn is_master(&self) -> bool {
        !self.variants.is_empty()
    }

    /// Segments come with `#EXT-X-MAP`: fragmented MP4 rather than MPEG-TS.
    pub fn is_fragmented_mp4(&self) -> bool {
        self.segments.iter().any(|s| s.init.is_some())
    }

    pub fn looks_like_playlist(text: &str) -> bool {
        text.trim_start_matches(['\u{FEFF}', ' ', '\r', '\n', '\t']).starts_with("#EXTM3U")
    }

    /// Fails (`CoreError::Invalid`) when `text` is not an HLS playlist, or names an
    /// address that cannot be resolved against `base`.
    pub fn parse(text: &str, base: &Url) -> Result<HlsPlaylist> {
        if !Self::looks_like_playlist(text) {
            return Err(CoreError::invalid("This is not an HLS playlist (it does not start with #EXTM3U)."));
        }

        let mut playlist = HlsPlaylist::default();
        let mut sequence: i64 = 0;
        let mut duration: f64 = 0.0;
        let mut range: Option<ByteRange> = None;
        let mut next_range_offset: i64 = 0;
        let mut key: Option<Arc<HlsKey>> = None;
        let mut init: Option<Arc<HlsInit>> = None;
        let mut variant: Option<PendingVariant> = None;

        // A byte order mark is not whitespace to trim, and left in place it turns
        // the header line into the address of a piece.
        for raw in text.trim_start_matches('\u{FEFF}').split('\n') {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }

            if !line.starts_with('#') {
                let uri = resolve(base, line)?;
                match variant.take() {
                    Some(v) => playlist.variants.push(HlsVariant {
                        uri,
                        bandwidth: v.bandwidth,
                        resolution: v.resolution,
                        codecs: v.codecs,
                        audio_group: v.audio,
                    }),
                    None => {
                        playlist.segments.push(HlsSegment {
                            uri,
                            duration,
                            range,
                            key: key.clone(),
                            sequence_number: sequence,
                            init: init.clone(),
                        });
                        if let Some(used) = range {
                            next_range_offset = used.offset + used.length;
                        }
                        sequence += 1;
                        duration = 0.0;
                        range = None;
                    }
                }
                continue;
            }

            let (tag, value) = match line.find(':') {
                Some(colon) => (&line[..colon], &line[colon + 1..]),
                None => (line, ""),
            };

            match tag {
                "#EXT-X-STREAM-INF" => {
                    let mut attributes = attributes(value);
                    variant = Some(PendingVariant {
                        bandwidth: attributes.get("BANDWIDTH").and_then(|b| parse_digits(b)).unwrap_or(0),
                        resolution: attributes.remove("RESOLUTION"),
                        codecs: attributes.remove("CODECS"),
                        audio: attributes.remove("AUDIO"),
                    });
                }
                "#EXT-X-MEDIA" => {
                    let mut attributes = attributes(value);
                    let uri = match attributes.get("URI") {
                        Some(u) => Some(resolve(base, u)?),
                        None => None,
                    };
                    playlist.renditions.push(HlsRendition {
                        kind: attributes.remove("TYPE").unwrap_or_default(),
                        group_id: attributes.remove("GROUP-ID").unwrap_or_default(),
                        name: attributes.remove("NAME").unwrap_or_default(),
                        uri,
                        is_default: attributes.get("DEFAULT").is_some_and(|d| d.eq_ignore_ascii_case("YES")),
                    });
                }
                // TryParse leaves zero behind when it fails, and so does this.
                "#EXT-X-MEDIA-SEQUENCE" => sequence = parse_digits(value).unwrap_or(0),
                "#EXTINF" => {
                    duration = value.split(',').next().unwrap_or_default().trim().parse::<f64>().unwrap_or(0.0);
                }
                "#EXT-X-BYTERANGE" => range = parse_range(value, next_range_offset),
                "#EXT-X-KEY" => {
                    let attributes = attributes(value);
                    let method = attributes.get("METHOD").cloned().unwrap_or_else(|| "NONE".to_string());
                    key = if method.eq_ignore_ascii_case("NONE") {
                        None
                    } else {
                        let uri = match attributes.get("URI") {
                            Some(u) => Some(resolve(base, u)?),
                            None => None,
                        };
                        Some(Arc::new(HlsKey { method, uri, iv: attributes.get("IV").and_then(|iv| parse_iv(iv)) }))
                    };
                }
                "#EXT-X-MAP" => {
                    let attributes = attributes(value);
                    if let Some(map_uri) = attributes.get("URI") {
                        init = Some(Arc::new(HlsInit {
                            uri: resolve(base, map_uri)?,
                            range: attributes.get("BYTERANGE").and_then(|r| parse_range(r, 0)),
                            key: key.clone(),
                        }));
                    }
                }
                "#EXT-X-ENDLIST" => playlist.is_complete = true,
                _ => {}
            }
        }

        Ok(playlist)
    }

    /// The IV an AES-128 segment is decrypted with: the one the playlist gives,
    /// else the segment's media sequence number as a 128-bit big-endian integer
    /// (RFC 8216 §5.2).
    pub fn iv_for(segment: &HlsSegment) -> [u8; 16] {
        Self::iv_with(segment.key.as_deref(), segment.sequence_number)
    }

    /// [`iv_for`](Self::iv_for) for a segment under `key` (which may be another
    /// key than its own — an initialization section's, say).
    pub fn iv_with(key: Option<&HlsKey>, sequence_number: i64) -> [u8; 16] {
        if let Some(iv) = key.and_then(|k| k.iv) {
            return iv;
        }
        let mut bytes = [0u8; 16];
        bytes[8..].copy_from_slice(&sequence_number.to_be_bytes());
        bytes
    }
}

/// An attribute list: `KEY=VALUE` pairs separated by commas, where a quoted value
/// may itself contain commas (a CODECS list always does). Names are upper-cased:
/// they are matched without regard to case.
pub(crate) fn attributes(list: &str) -> HashMap<String, String> {
    let mut attributes = HashMap::new();
    let bytes = list.as_bytes();
    let mut i = 0;
    while i < list.len() {
        let Some(equals) = list[i..].find('=').map(|p| p + i) else { break };
        let name = list[i..equals].trim().trim_start_matches(',').trim();

        let start = equals + 1;
        let value;
        if bytes.get(start) == Some(&b'"') {
            let close = list[start + 1..].find('"').map_or(list.len(), |p| p + start + 1);
            value = &list[start + 1..close];
            i = (close + 1).min(list.len());
        } else {
            let comma = list[start..].find(',').map_or(list.len(), |p| p + start);
            value = list[start..comma].trim();
            i = comma;
        }

        if !name.is_empty() {
            attributes.insert(name.to_ascii_uppercase(), value.to_string());
        }
        while bytes.get(i) == Some(&b',') {
            i += 1;
        }
    }
    attributes
}

/// `long.TryParse(..., NumberStyles.None)`: ASCII digits only — no sign, no spaces.
fn parse_digits(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn parse_range(value: &str, default_offset: i64) -> Option<ByteRange> {
    let mut parts = value.trim_matches('"').split('@');
    let length = parse_digits(parts.next().unwrap_or_default()).filter(|&l| l > 0)?;
    let offset = match parts.next() {
        Some(offset) => parse_digits(offset)?,
        None => default_offset,
    };
    Some(ByteRange { offset, length })
}

fn parse_iv(value: &str) -> Option<[u8; 16]> {
    let hex = match value.get(..2) {
        Some(prefix) if prefix.eq_ignore_ascii_case("0x") => &value[2..],
        _ => value,
    };
    if hex.len() != 32 || !hex.is_ascii() {
        return None;
    }
    let mut iv = [0u8; 16];
    for (i, byte) in iv.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(iv)
}

/// `new Uri(baseUri, reference.Trim())`.
pub(crate) fn resolve(base: &Url, reference: &str) -> Result<Url> {
    base.join(reference.trim())
        .map_err(|e| CoreError::invalid(format!("The playlist names an address that cannot be read ({e}).")))
}

#[cfg(test)]
mod tests {
    //! Every mistake here produces a file that looks complete and will not play:
    //! a byte range taken from the wrong offset, an IV that is off by one, a
    //! relative address resolved against the wrong base.

    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example.com/video/master.m3u8?token=abc").unwrap()
    }

    fn parse(text: &str) -> HlsPlaylist {
        HlsPlaylist::parse(text, &base()).unwrap()
    }

    #[test]
    fn a_master_playlist_lists_its_qualities_with_attributes_whose_values_contain_commas() {
        let playlist = parse(
            r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aac",NAME="English",DEFAULT=YES,URI="audio/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=5200000,RESOLUTION=1920x1080,CODECS="avc1.640028,mp4a.40.2",AUDIO="aac"
1080/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.4d401e,mp4a.40.2",AUDIO="aac"
https://other.example.com/360/index.m3u8"#,
        );

        assert!(playlist.is_master());
        assert_eq!(playlist.variants.len(), 2);

        let best = &playlist.variants[0];
        assert_eq!(best.bandwidth, 5_200_000);
        assert_eq!(best.resolution.as_deref(), Some("1920x1080"));
        assert_eq!(best.codecs.as_deref(), Some("avc1.640028,mp4a.40.2"));
        assert_eq!(best.audio_group.as_deref(), Some("aac"));
        assert_eq!(best.uri.as_str(), "https://cdn.example.com/video/1080/index.m3u8");
        assert_eq!(playlist.variants[1].uri.as_str(), "https://other.example.com/360/index.m3u8");

        assert_eq!(playlist.renditions.len(), 1);
        let audio = &playlist.renditions[0];
        assert_eq!(audio.kind, "AUDIO");
        assert!(audio.is_default);
        assert_eq!(audio.uri.as_ref().unwrap().as_str(), "https://cdn.example.com/video/audio/en.m3u8");
    }

    #[test]
    fn a_media_playlist_lists_its_pieces_in_order_with_their_sequence_numbers() {
        let playlist = parse(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:40\n#EXTINF:6.006,\nseg40.ts\n#EXTINF:5.5,title\nseg41.ts\n#EXT-X-ENDLIST",
        );
        assert!(!playlist.is_master());
        assert!(playlist.is_complete);
        assert_eq!(playlist.segments.iter().map(|s| s.sequence_number).collect::<Vec<_>>(), [40, 41]);
        assert!((playlist.segments[0].duration - 6.006).abs() < 1e-9);
        assert_eq!(playlist.segments[1].uri.as_str(), "https://cdn.example.com/video/seg41.ts");
    }

    #[test]
    fn a_playlist_without_an_end_is_live() {
        assert!(!parse("#EXTM3U\n#EXTINF:4,\na.ts\n").is_complete);
    }

    #[test]
    fn a_byte_range_without_an_offset_starts_where_the_previous_one_ended() {
        let playlist =
            parse("#EXTM3U\n#EXTINF:4,\n#EXT-X-BYTERANGE:1000@500\nall.ts\n#EXTINF:4,\n#EXT-X-BYTERANGE:2000\nall.ts\n#EXT-X-ENDLIST");
        assert_eq!(playlist.segments[0].range, Some(ByteRange::new(500, 1000)));
        assert_eq!(playlist.segments[1].range, Some(ByteRange::new(1500, 2000)));
        assert_eq!(playlist.segments[1].range.unwrap().end(), 3499);
    }

    #[test]
    fn a_key_applies_to_every_piece_after_it_until_another_key_replaces_it() {
        let playlist = parse(
            r#"#EXTM3U
#EXTINF:4,
clear.ts
#EXT-X-KEY:METHOD=AES-128,URI="https://keys.example.com/k1",IV=0x000102030405060708090A0B0C0D0E0F
#EXTINF:4,
locked1.ts
#EXTINF:4,
locked2.ts
#EXT-X-KEY:METHOD=NONE
#EXTINF:4,
clear2.ts
#EXT-X-ENDLIST"#,
        );
        let s = &playlist.segments;
        assert!(s[0].key.is_none());
        let key = s[1].key.as_ref().unwrap();
        assert!(key.is_aes128());
        assert!(Arc::ptr_eq(key, s[2].key.as_ref().unwrap()));
        assert_eq!(key.uri.as_ref().unwrap().as_str(), "https://keys.example.com/k1");
        assert_eq!(key.iv, Some([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]));
        assert!(s[3].key.is_none());
    }

    #[test]
    fn without_an_iv_a_piece_is_decrypted_with_its_sequence_number_big_endian() {
        let playlist = parse("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:258\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST");
        let iv = HlsPlaylist::iv_for(&playlist.segments[0]);
        assert_eq!(iv[..14], [0u8; 14]);
        assert_eq!(iv[14..], [0x01, 0x02]);
    }

    #[test]
    fn an_initialization_section_travels_with_the_pieces_after_it() {
        let playlist = parse(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n#EXTINF:4,\ns1.m4s\n#EXTINF:4,\ns2.m4s\n#EXT-X-ENDLIST",
        );
        assert!(playlist.is_fragmented_mp4());
        let init = playlist.segments[0].init.as_ref().unwrap();
        assert!(Arc::ptr_eq(init, playlist.segments[1].init.as_ref().unwrap()));
        assert_eq!(init.uri.as_str(), "https://cdn.example.com/video/init.mp4");
        assert_eq!(init.range, Some(ByteRange::new(0, 720)));
    }

    #[test]
    fn windows_line_endings_and_a_byte_order_mark_are_read_like_any_playlist() {
        let playlist = parse("\u{FEFF}#EXTM3U\r\n#EXTINF:4,\r\na.ts\r\n#EXT-X-ENDLIST\r\n");
        assert_eq!(playlist.segments.len(), 1);
        assert_eq!(playlist.segments[0].uri.as_str(), "https://cdn.example.com/video/a.ts");
        assert!(playlist.is_complete);
    }

    #[test]
    fn anything_that_does_not_start_with_the_header_is_not_a_playlist() {
        assert!(!HlsPlaylist::looks_like_playlist("<html>sign in</html>"));
        assert!(matches!(HlsPlaylist::parse("<html>sign in</html>", &base()), Err(CoreError::Invalid(_))));
    }

    #[test]
    fn attribute_lists_tolerate_odd_spacing_and_unterminated_quotes() {
        let a = attributes(r#" bandwidth=1 , NAME="a,b",URI="unterminated"#);
        assert_eq!(a.get("BANDWIDTH").map(String::as_str), Some("1"));
        assert_eq!(a.get("NAME").map(String::as_str), Some("a,b"));
        assert_eq!(a.get("URI").map(String::as_str), Some("unterminated"));
        assert!(attributes("").is_empty());
        assert!(attributes("novalue").is_empty());
    }

    #[test]
    fn a_bad_number_reads_as_zero_and_a_bad_range_as_none() {
        let playlist = parse("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:-3\n#EXTINF:abc,\n#EXT-X-BYTERANGE:-5\na.ts\n#EXT-X-ENDLIST");
        assert_eq!(playlist.segments[0].sequence_number, 0);
        assert_eq!(playlist.segments[0].duration, 0.0);
        assert_eq!(playlist.segments[0].range, None);
    }
}
