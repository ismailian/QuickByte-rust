//! Port of `src/QuickByte.Core/Streaming/StreamDetector.cs`.
//!
//! Recognises the address of a streaming video's playlist or manifest — the one
//! question the Add window asks before it offers a choice of quality instead of
//! saving a text file of addresses.

use percent_encoding::percent_decode_str;
use url::Url;

use crate::helpers::file_name_helper;

const PLAYLIST_TYPES: [&str; 4] = ["application/vnd.apple.mpegurl", "application/x-mpegurl", "audio/mpegurl", "audio/x-mpegurl"];

/// By its content type or, when the server does not say (or says `text/plain`,
/// which many do), by its extension.
pub fn is_stream(url: &str, content_type: Option<&str>) -> bool {
    let kind = content_type.unwrap_or("").split(';').next().unwrap_or("").trim();
    if PLAYLIST_TYPES.iter().any(|t| t.eq_ignore_ascii_case(kind)) || kind.eq_ignore_ascii_case("application/dash+xml") {
        return true;
    }
    let Ok(parsed) = Url::parse(url) else { return false };
    let extension = file_name_helper::extension_of(parsed.path().rsplit('/').next().unwrap_or(""));
    extension.eq_ignore_ascii_case(".m3u8") || extension.eq_ignore_ascii_case(".mpd")
}

/// The names players give every playlist, which say nothing about the video.
const GENERIC_NAMES: [&str; 15] = [
    "master", "index", "playlist", "manifest", "main", "stream", "chunklist", "prog_index", "video", "media", "hls", "dash",
    "output", "source", "default",
];

fn is_generic(name: &str) -> bool {
    GENERIC_NAMES.iter().any(|g| g.eq_ignore_ascii_case(name))
}

/// A name for the saved video, with `extension`. A playlist is nearly always
/// called `master.m3u8` or `manifest.mpd`, and a hundred videos saved as
/// `master (99).mp4` are a hundred files nobody can tell apart — so a generic
/// name gives way to the folder the playlist sits in, which is usually the
/// video's own (`/talks/keynote-2026/master.m3u8`).
pub fn suggest_file_name(url: &str, current_name: &str, extension: &str) -> String {
    let mut stem = file_name_helper::stem_of(current_name).to_string();

    if (stem.is_empty() || is_generic(&stem))
        && let Ok(parsed) = Url::parse(url)
    {
        let folders: Vec<String> = parsed
            .path_segments()
            .map(|segments| {
                let all: Vec<&str> = segments.collect();
                all[..all.len().saturating_sub(1)].iter().map(|s| percent_decode_str(s).decode_utf8_lossy().trim_matches('/').to_string()).collect()
            })
            .unwrap_or_default();
        stem = folders.into_iter().rev().find(|s| is_descriptive(s)).unwrap_or_else(|| "video".into());
    }

    if stem.is_empty() {
        stem = "video".into();
    }
    file_name_helper::sanitize_file_name(&format!("{stem}{extension}"))
}

/// Not empty, not generic, and not a bare number or hash — "1080", "v1", "a3f9c0e1...".
fn is_descriptive(segment: &str) -> bool {
    let length = segment.chars().count();
    length >= 3
        && !is_generic(segment)
        && segment.chars().filter(|c| c.is_alphabetic()).count() >= 3
        && !(length >= 16 && segment.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlists_are_recognised_by_type_or_by_extension() {
        for (url, content_type, expected) in [
            ("https://x.example.com/a/master.m3u8?sig=1", None, true),
            ("https://x.example.com/a/manifest.MPD", Some("text/plain"), true),
            ("https://x.example.com/play?id=4", Some("application/vnd.apple.mpegurl; charset=utf-8"), true),
            ("https://x.example.com/play?id=4", Some("application/dash+xml"), true),
            ("https://x.example.com/video.mp4", Some("video/mp4"), false),
            ("https://x.example.com/list.m3u", Some("audio/x-scpls"), false),
        ] {
            assert_eq!(is_stream(url, content_type), expected, "{url}");
        }
    }

    #[test]
    fn a_generic_playlist_name_gives_way_to_the_folder_the_video_sits_in() {
        for (url, name, extension, expected) in [
            ("https://cdn.example.com/talks/keynote-2026/master.m3u8", "master.m3u8", ".mp4", "keynote-2026.mp4"),
            ("https://cdn.example.com/talks/keynote-2026/hls/1080/index.m3u8", "index.m3u8", ".ts", "keynote-2026.ts"),
            ("https://cdn.example.com/v/3f9c0e1a2b4d6f80a1c3e5f7/manifest.mpd", "manifest.mpd", ".mp4", "video.mp4"),
            ("https://cdn.example.com/show/Launch%20Day/master.m3u8", "master.m3u8", ".mp4", "Launch Day.mp4"),
            ("https://cdn.example.com/show/master.m3u8", "Our Launch Day.m3u8", ".mp4", "Our Launch Day.mp4"),
        ] {
            assert_eq!(suggest_file_name(url, name, extension), expected, "{url}");
        }
    }
}
