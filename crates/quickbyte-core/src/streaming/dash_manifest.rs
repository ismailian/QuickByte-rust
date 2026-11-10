//! Port of `DashManifest.cs`: a DASH manifest (MPD), reduced to what a download
//! needs — the representations of the first period and, for each, the exact list
//! of segment URLs to fetch.
//!
//! Segments are addressed three ways in the wild, and all three are handled —
//! `SegmentTemplate` (by number, or by time with a `SegmentTimeline`),
//! `SegmentList`, and a plain `BaseURL` holding the whole representation. Two
//! kinds of manifest are recognised and refused rather than half-downloaded: a
//! live one (`type="dynamic"`), which never ends, and one whose content is
//! protected (`ContentProtection`), which cannot be played once downloaded.

use roxmltree::{Document, Node, ParsingOptions};
use url::Url;

use super::hls_playlist::{ByteRange, resolve};
use crate::error::{CoreError, Result};

/// One piece of a DASH representation: a URL, and a byte range within it when
/// the manifest names one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DashSegment {
    pub uri: Url,
    pub range: Option<ByteRange>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DashRepresentation {
    pub id: String,
    pub content_type: String,
    pub mime_type: String,
    pub bandwidth: i64,
    pub width: i32,
    pub height: i32,
    pub codecs: Option<String>,
    pub initialization: Option<DashSegment>,
    pub segments: Vec<DashSegment>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DashManifest {
    pub is_live: bool,
    pub is_protected: bool,
    pub representations: Vec<DashRepresentation>,
}

/// More segments than this in one representation is a malformed or hostile
/// manifest, not a video.
pub const MAX_SEGMENTS: usize = 100_000;

const TOO_MANY: &str = "The DASH manifest lists more segments than a video could have.";

impl DashManifest {
    pub fn looks_like_manifest(text: &str) -> bool {
        text.contains("<MPD")
    }

    /// Fails (`CoreError::Invalid`) for anything that is not a DASH manifest, or
    /// one that cannot be read.
    pub fn parse(xml: &str, manifest_uri: &Url) -> Result<DashManifest> {
        // No DTDs and no external entities: this is a document from the internet.
        let options = ParsingOptions { allow_dtd: false, ..ParsingOptions::default() };
        let document = Document::parse_with_options(xml, options)
            .map_err(|e| CoreError::invalid(format!("The DASH manifest is not valid XML: {e}")))?;
        let mpd = document.root_element();

        // roxmltree lets an empty `<!DOCTYPE MPD>` through; the C# reader
        // (DtdProcessing.Prohibit) refused any declaration at all, and so does this.
        if xml[..mpd.range().start].contains("<!DOCTYPE") {
            return Err(CoreError::invalid(
                "The DASH manifest is not valid XML: For security reasons DTD is prohibited in this XML document.",
            ));
        }
        if mpd.tag_name().name() != "MPD" {
            return Err(CoreError::invalid("This is not a DASH manifest."));
        }

        let live = mpd.attribute("type").is_some_and(|t| t.eq_ignore_ascii_case("dynamic"));
        let Some(period) = children(mpd, "Period").next() else {
            return Ok(DashManifest { is_live: live, ..Default::default() });
        };

        let period_duration =
            duration(period.attribute("duration")).or_else(|| duration(mpd.attribute("mediaPresentationDuration")));

        let base = base_url(&base_url(manifest_uri, mpd)?, period)?;
        let mut is_protected = false;
        let mut representations = Vec::new();

        for set in children(period, "AdaptationSet") {
            if children(set, "ContentProtection").next().is_some() {
                is_protected = true;
            }
            let set_base = base_url(&base, set)?;
            for representation in children(set, "Representation") {
                if children(representation, "ContentProtection").next().is_some() {
                    is_protected = true;
                }
                let id = representation.attribute("id").unwrap_or_default().to_string();
                let mime = inherited(representation, set, "mimeType").unwrap_or_default().to_string();
                let content_type = set
                    .attribute("contentType")
                    .map(str::to_string)
                    .unwrap_or_else(|| mime.split('/').next().unwrap_or_default().to_string());
                let bandwidth = attr_i64(representation, "bandwidth")?.unwrap_or(0);
                let rep_base = base_url(&set_base, representation)?;

                let (initialization, segments) = segments_of(representation, set, &rep_base, &id, bandwidth, period_duration)?;
                representations.push(DashRepresentation {
                    id,
                    content_type,
                    mime_type: mime,
                    bandwidth,
                    width: attr_i32(representation, "width")?.unwrap_or(0),
                    height: attr_i32(representation, "height")?.unwrap_or(0),
                    codecs: inherited(representation, set, "codecs").map(str::to_string),
                    initialization,
                    segments,
                });
            }
        }

        Ok(DashManifest { is_live: live, is_protected, representations })
    }
}

type Segments = (Option<DashSegment>, Vec<DashSegment>);

fn segments_of(
    representation: Node<'_, '_>,
    set: Node<'_, '_>,
    base: &Url,
    id: &str,
    bandwidth: i64,
    period_duration: Option<f64>,
) -> Result<Segments> {
    let template = children(representation, "SegmentTemplate").next().or_else(|| children(set, "SegmentTemplate").next());
    if let Some(template) = template {
        return from_template(template, base, id, bandwidth, period_duration);
    }

    let list = children(representation, "SegmentList").next().or_else(|| children(set, "SegmentList").next());
    if let Some(list) = list {
        let init = match children(list, "Initialization").next() {
            Some(initialization) => Some(DashSegment {
                uri: match initialization.attribute("sourceURL") {
                    Some(source) => resolve(base, source)?,
                    None => base.clone(),
                },
                range: range_attribute(initialization.attribute("range")),
            }),
            None => None,
        };
        let segments = children(list, "SegmentURL")
            .map(|url| {
                Ok(DashSegment {
                    uri: match url.attribute("media") {
                        Some(media) => resolve(base, media)?,
                        None => base.clone(),
                    },
                    range: range_attribute(url.attribute("mediaRange")),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok((init, segments));
    }

    // SegmentBase, or nothing at all: the representation is one file. Its
    // initialization and index live inside it, so the whole file is the download.
    Ok((None, vec![DashSegment { uri: base.clone(), range: None }]))
}

fn from_template(template: Node<'_, '_>, base: &Url, id: &str, bandwidth: i64, period_duration: Option<f64>) -> Result<Segments> {
    let start_number = attr_i64(template, "startNumber")?.unwrap_or(1);
    let timescale = attr_i64(template, "timescale")?.unwrap_or(1).max(1);
    let media = template.attribute("media");
    let initialization = template.attribute("initialization");

    let init = match initialization {
        Some(init) => Some(DashSegment { uri: resolve(base, &fill(init, id, bandwidth, None, None))?, range: None }),
        None => None,
    };
    let Some(media) = media else { return Ok((init, Vec::new())) };

    let mut segments = Vec::new();
    if let Some(timeline) = children(template, "SegmentTimeline").next() {
        let mut number = start_number;
        let mut time: i64 = 0;
        let period_end = period_duration.map(|seconds| (seconds * timescale as f64) as i64);

        for s in children(timeline, "S") {
            if let Some(t) = attr_i64(s, "t")? {
                time = t;
            }
            let d = attr_i64(s, "d")?.unwrap_or(0);
            let r = attr_i64(s, "r")?.unwrap_or(0);
            if d <= 0 {
                break;
            }

            // r = -1 repeats until the next S, or the end of the period.
            let repeats = if r >= 0 {
                r
            } else {
                period_end.map_or(0, |end| (end.saturating_sub(time) / d - 1).max(0))
            };

            let mut i = 0;
            while i <= repeats {
                segments.push(DashSegment { uri: resolve(base, &fill(media, id, bandwidth, Some(number), Some(time)))?, range: None });
                if segments.len() > MAX_SEGMENTS {
                    return Err(CoreError::invalid(TOO_MANY));
                }
                number = number.saturating_add(1);
                time = time.saturating_add(d);
                i += 1;
            }
        }
        return Ok((init, segments));
    }

    let duration = attr_i64(template, "duration")?.unwrap_or(0);
    let Some(period) = period_duration.filter(|_| duration > 0) else {
        return Ok((init, segments));
    };

    let count = (period * timescale as f64 / duration as f64).ceil();
    if !count.is_finite() || count > MAX_SEGMENTS as f64 {
        return Err(CoreError::invalid(TOO_MANY));
    }
    for i in 0..(count.max(0.0) as i64) {
        segments.push(DashSegment {
            uri: resolve(base, &fill(media, id, bandwidth, Some(start_number + i), Some(i * duration)))?,
            range: None,
        });
    }
    Ok((init, segments))
}

/// Substitutes a SegmentTemplate's identifiers — `$RepresentationID$`,
/// `$Number$`, `$Time$`, `$Bandwidth$`, including the `%05d` width form, and
/// `$$` for a literal dollar.
pub fn fill(template: &str, id: &str, bandwidth: i64, number: Option<i64>, time: Option<i64>) -> String {
    const IDENTIFIERS: [&str; 4] = ["RepresentationID", "Number", "Time", "Bandwidth"];
    let mut filled = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(dollar) = rest.find('$') {
        filled.push_str(&rest[..dollar]);
        let candidate = &rest[dollar + 1..];
        let matched = IDENTIFIERS.iter().find_map(|name| {
            let after = candidate.strip_prefix(name)?;
            // Optional "%0<digits>d", then the closing '$'.
            let (width, after) = match after.strip_prefix("%0") {
                Some(fmt) => {
                    let digits = fmt.bytes().take_while(u8::is_ascii_digit).count();
                    let tail = fmt[digits..].strip_prefix('d')?;
                    if digits == 0 {
                        return None;
                    }
                    (fmt[..digits].parse::<usize>().ok(), tail)
                }
                None => (None, after),
            };
            let after = after.strip_prefix('$')?;
            Some((*name, width, candidate.len() - after.len()))
        });
        match matched {
            Some((name, width, consumed)) => {
                let whole = &rest[dollar..dollar + 1 + consumed];
                let value = match name {
                    "RepresentationID" => id.to_string(),
                    "Number" => number.map_or_else(|| whole.to_string(), |n| n.to_string()),
                    "Time" => time.map_or_else(|| whole.to_string(), |t| t.to_string()),
                    _ => bandwidth.to_string(),
                };
                match width {
                    Some(w) if w > value.chars().count() => {
                        filled.extend(std::iter::repeat_n('0', w - value.chars().count()));
                        filled.push_str(&value);
                    }
                    _ => filled.push_str(&value),
                }
                rest = &rest[dollar + 1 + consumed..];
            }
            None => {
                filled.push('$');
                rest = candidate;
            }
        }
    }
    filled.push_str(rest);
    filled.replace("$$", "$")
}

fn children<'a, 'input>(parent: Node<'a, 'input>, local_name: &'static str) -> impl Iterator<Item = Node<'a, 'input>> {
    parent.children().filter(move |e| e.is_element() && e.tag_name().name() == local_name)
}

fn inherited<'a>(representation: Node<'a, '_>, set: Node<'a, '_>, attribute: &str) -> Option<&'a str> {
    representation.attribute(attribute).or_else(|| set.attribute(attribute))
}

fn base_url(parent: &Url, element: Node<'_, '_>) -> Result<Url> {
    let Some(node) = children(element, "BaseURL").next() else { return Ok(parent.clone()) };
    let text: String = node.descendants().filter(|n| n.is_text()).filter_map(|n| n.text()).collect();
    if text.trim().is_empty() { Ok(parent.clone()) } else { resolve(parent, text.trim()) }
}

/// `(long?)attribute`: absent is `None`; present but not a number fails the
/// whole manifest, as `XmlConvert.ToInt64` did.
fn attr_i64(node: Node<'_, '_>, name: &str) -> Result<Option<i64>> {
    match node.attribute(name) {
        None => Ok(None),
        Some(text) => text
            .trim()
            .parse::<i64>()
            .map(Some)
            .map_err(|_| CoreError::invalid(format!("The DASH manifest's {name}=\"{text}\" is not a number."))),
    }
}

fn attr_i32(node: Node<'_, '_>, name: &str) -> Result<Option<i32>> {
    match node.attribute(name) {
        None => Ok(None),
        Some(text) => text
            .trim()
            .parse::<i32>()
            .map(Some)
            .map_err(|_| CoreError::invalid(format!("The DASH manifest's {name}=\"{text}\" is not a number."))),
    }
}

/// An `xs:duration` (`PT1H2M3.5S`, `P365D`) in seconds, as `XmlConvert.ToTimeSpan`
/// reads it — a year is 365 days and a month 30. `None` for anything else.
fn duration(value: Option<&str>) -> Option<f64> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let (negative, rest) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    let rest = rest.strip_prefix('P')?;
    let (date, time) = match rest.split_once('T') {
        Some((date, time)) => {
            if time.is_empty() {
                return None;
            }
            (date, Some(time))
        }
        None => (rest, None),
    };
    if date.is_empty() && time.is_none() {
        return None;
    }

    // Each part is digits followed by its designator, in this order, each at most once.
    fn parts(text: &str, designators: &[char], allow_fraction_on_last: bool) -> Option<Vec<(char, f64)>> {
        let mut found = Vec::new();
        let mut rest = text;
        let mut next = 0;
        while !rest.is_empty() {
            let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
            let number = &rest[..end];
            let designator = rest[end..].chars().next()?;
            let position = designators[next..].iter().position(|&d| d == designator)? + next;
            let is_last = position == designators.len() - 1;
            if number.is_empty() || (number.contains('.') && !(allow_fraction_on_last && is_last)) {
                return None;
            }
            found.push((designator, number.parse::<f64>().ok()?));
            next = position + 1;
            rest = &rest[end + designator.len_utf8()..];
        }
        Some(found)
    }

    let mut seconds = 0.0;
    let mut years = 0.0;
    let mut months = 0.0;
    for (designator, n) in parts(date, &['Y', 'M', 'D'], false)? {
        match designator {
            'Y' => years = n,
            'M' => months = n,
            _ => seconds += n * 86_400.0,
        }
    }
    let whole_years = years + (months / 12.0).floor();
    seconds += (whole_years * 365.0 + (months % 12.0) * 30.0) * 86_400.0;
    if let Some(time) = time {
        for (designator, n) in parts(time, &['H', 'M', 'S'], true)? {
            seconds += match designator {
                'H' => n * 3600.0,
                'M' => n * 60.0,
                _ => n,
            };
        }
    }
    Some(if negative { -seconds } else { seconds })
}

fn range_attribute(value: Option<&str>) -> Option<ByteRange> {
    let value = value?;
    if value.trim().is_empty() {
        return None;
    }
    let mut parts = value.split('-');
    let (start, end) = (parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let digits = |s: &str| (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<i64>().ok()).flatten();
    let (start, end) = (digits(start)?, digits(end)?);
    (end >= start).then(|| ByteRange::new(start, end - start + 1))
}

#[cfg(test)]
mod tests {
    //! Three ways of addressing segments, the base-address hierarchy they
    //! resolve against, and the manifests that must be refused.

    use super::*;

    const NS: &str = "urn:mpeg:dash:schema:mpd:2011";
    const PREFIX: &str = "https://media.example.com/show/ep1/";

    fn manifest_uri() -> Url {
        Url::parse("https://media.example.com/show/ep1/manifest.mpd").unwrap()
    }

    fn parse(xml: &str) -> Result<DashManifest> {
        DashManifest::parse(xml, &manifest_uri())
    }

    fn relative(segments: &[DashSegment]) -> Vec<String> {
        segments.iter().map(|s| s.uri.as_str().replace(PREFIX, "")).collect()
    }

    #[test]
    fn a_numbered_template_is_expanded_over_the_period_with_padding() {
        let manifest = parse(&format!(
            r#"<MPD xmlns="{NS}" type="static" mediaPresentationDuration="PT10S">
              <Period>
                <AdaptationSet contentType="video" mimeType="video/mp4">
                  <SegmentTemplate timescale="1000" duration="4000" startNumber="1"
                                   initialization="$RepresentationID$/init.mp4"
                                   media="$RepresentationID$/seg-$Number%05d$.m4s"/>
                  <Representation id="v1080" bandwidth="5000000" width="1920" height="1080" codecs="avc1.640028"/>
                </AdaptationSet>
              </Period>
            </MPD>"#
        ))
        .unwrap();

        assert_eq!(manifest.representations.len(), 1);
        let video = &manifest.representations[0];
        assert_eq!(video.content_type, "video");
        assert_eq!(video.height, 1080);
        assert_eq!(video.codecs.as_deref(), Some("avc1.640028"));
        assert_eq!(video.initialization.as_ref().unwrap().uri.as_str(), "https://media.example.com/show/ep1/v1080/init.mp4");
        // 10 s in 4 s pieces is three pieces, the last one short.
        assert_eq!(relative(&video.segments), ["v1080/seg-00001.m4s", "v1080/seg-00002.m4s", "v1080/seg-00003.m4s"]);
    }

    #[test]
    fn a_timeline_repeats_each_entry_and_addresses_pieces_by_time() {
        let manifest = parse(&format!(
            r#"<MPD xmlns="{NS}" mediaPresentationDuration="PT20S">
              <Period>
                <AdaptationSet mimeType="audio/mp4">
                  <Representation id="a" bandwidth="128000">
                    <SegmentTemplate timescale="10" media="a/$Time$.m4s" startNumber="7">
                      <SegmentTimeline>
                        <S t="0" d="40" r="1"/>
                        <S d="30"/>
                        <S d="45" r="-1"/>
                      </SegmentTimeline>
                    </SegmentTemplate>
                  </Representation>
                </AdaptationSet>
              </Period>
            </MPD>"#
        ))
        .unwrap();

        let audio = &manifest.representations[0];
        assert_eq!(audio.content_type, "audio");
        // 0, 40 (r=1); 80 (d=30); then 45s from 110 until the 200 the period ends at: 110, 155.
        assert_eq!(relative(&audio.segments), ["a/0.m4s", "a/40.m4s", "a/80.m4s", "a/110.m4s", "a/155.m4s"]);
    }

    #[test]
    fn a_segment_list_keeps_each_pieces_byte_range_and_base_addresses_nest() {
        let manifest = parse(&format!(
            r#"<MPD xmlns="{NS}">
              <BaseURL>https://cdn.example.net/root/</BaseURL>
              <Period>
                <AdaptationSet mimeType="video/mp4">
                  <BaseURL>video/</BaseURL>
                  <Representation id="v" bandwidth="1">
                    <BaseURL>file.mp4</BaseURL>
                    <SegmentList>
                      <Initialization range="0-799"/>
                      <SegmentURL mediaRange="800-1799"/>
                      <SegmentURL media="other.mp4" mediaRange="0-99"/>
                    </SegmentList>
                  </Representation>
                </AdaptationSet>
              </Period>
            </MPD>"#
        ))
        .unwrap();

        let video = &manifest.representations[0];
        let init = video.initialization.as_ref().unwrap();
        assert_eq!(init.uri.as_str(), "https://cdn.example.net/root/video/file.mp4");
        assert_eq!(init.range, Some(ByteRange::new(0, 800)));
        assert_eq!(video.segments[0].range, Some(ByteRange::new(800, 1000)));
        assert_eq!(video.segments[1].uri.as_str(), "https://cdn.example.net/root/video/other.mp4");
    }

    #[test]
    fn a_representation_that_is_one_file_is_one_piece() {
        let manifest = parse(&format!(
            r#"<MPD xmlns="{NS}">
              <Period>
                <AdaptationSet mimeType="audio/webm">
                  <Representation id="opus" bandwidth="160000">
                    <BaseURL>audio.webm</BaseURL>
                    <SegmentBase indexRange="100-400"/>
                  </Representation>
                </AdaptationSet>
              </Period>
            </MPD>"#
        ))
        .unwrap();

        let segments = &manifest.representations[0].segments;
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].uri.as_str(), "https://media.example.com/show/ep1/audio.webm");
        assert_eq!(segments[0].range, None);
    }

    #[test]
    fn live_and_protected_manifests_say_so() {
        let live = parse(&format!(r#"<MPD xmlns="{NS}" type="dynamic"><Period/></MPD>"#)).unwrap();
        let drm = parse(&format!(
            r#"<MPD xmlns="{NS}"><Period><AdaptationSet mimeType="video/mp4">
              <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/>
              <Representation id="v" bandwidth="1"><BaseURL>v.mp4</BaseURL></Representation>
            </AdaptationSet></Period></MPD>"#
        ))
        .unwrap();
        assert!(live.is_live);
        assert!(drm.is_protected);
    }

    #[test]
    fn a_document_type_declaration_is_refused_rather_than_resolved() {
        // An entity that reads a local file is the classic way to turn an XML
        // parser on a document from the internet against the machine reading it.
        let hostile = r#"<?xml version="1.0"?>
<!DOCTYPE MPD [<!ENTITY secret SYSTEM "file:///C:/Windows/win.ini">]>
<MPD><Period><AdaptationSet><Representation id="&secret;"/></AdaptationSet></Period></MPD>"#;
        assert!(matches!(parse(hostile), Err(CoreError::Invalid(_))));

        // Not even an empty one.
        assert!(matches!(parse("<!DOCTYPE MPD><MPD><Period/></MPD>"), Err(CoreError::Invalid(_))));
    }

    #[test]
    fn a_template_claiming_more_pieces_than_a_video_could_have_is_refused() {
        // A one-millisecond piece over a year would otherwise allocate for minutes.
        let result = parse(&format!(
            r#"<MPD xmlns="{NS}" mediaPresentationDuration="P365D">
              <Period><AdaptationSet mimeType="video/mp4">
                <SegmentTemplate timescale="1000" duration="1" media="$Number$.m4s"/>
                <Representation id="v" bandwidth="1"/>
              </AdaptationSet></Period>
            </MPD>"#
        ));
        assert!(matches!(result, Err(CoreError::Invalid(_))));
    }

    #[test]
    fn a_timeline_that_repeats_forever_is_refused_too() {
        let result = parse(&format!(
            r#"<MPD xmlns="{NS}"><Period><AdaptationSet mimeType="video/mp4">
                <SegmentTemplate media="$Number$.m4s"><SegmentTimeline><S d="1" r="999999999"/></SegmentTimeline></SegmentTemplate>
                <Representation id="v" bandwidth="1"/>
              </AdaptationSet></Period></MPD>"#
        ));
        assert!(matches!(result, Err(CoreError::Invalid(_))));
    }

    #[test]
    fn template_identifiers_are_substituted() {
        for (template, expected) in [
            ("$RepresentationID$/$Bandwidth$/$Number$", "v/800/12"),
            ("t$Time%08d$.m4s", "t00004000.m4s"),
            ("price$$5-$Number%03d$", "price$5-012"),
        ] {
            assert_eq!(fill(template, "v", 800, Some(12), Some(4000)), expected, "{template}");
        }
        assert_eq!(fill("$Number$-$Unknown$", "v", 1, None, None), "$Number$-$Unknown$");
    }

    #[test]
    fn durations_read_the_way_xml_schema_writes_them() {
        assert_eq!(duration(Some("PT10S")), Some(10.0));
        assert_eq!(duration(Some("PT1H2M3.5S")), Some(3723.5));
        assert_eq!(duration(Some("P1DT1S")), Some(86_401.0));
        assert_eq!(duration(Some("P365D")), Some(365.0 * 86_400.0));
        assert_eq!(duration(Some("P1Y")), Some(365.0 * 86_400.0));
        assert_eq!(duration(Some("P2M")), Some(60.0 * 86_400.0));
        assert_eq!(duration(Some("PT")), None);
        assert_eq!(duration(Some("P")), None);
        assert_eq!(duration(Some("10S")), None);
        assert_eq!(duration(Some("PT5M1H")), None);
        assert_eq!(duration(None), None);
    }

    #[test]
    fn something_that_is_not_xml_or_not_an_mpd_is_refused() {
        assert!(matches!(parse("<MPD"), Err(CoreError::Invalid(_))));
        assert!(matches!(parse("<html/>"), Err(CoreError::Invalid(_))));
        assert!(parse(r#"<MPD><Period><AdaptationSet><Representation bandwidth="lots"/></AdaptationSet></Period></MPD>"#).is_err());
        assert!(DashManifest::looks_like_manifest("<?xml?><MPD>"));
    }
}
