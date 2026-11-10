//! Port of `BatchLinks.cs`: turns what a user pastes into a batch download — a
//! list of links, a web page's source, or a numbered pattern — into the links
//! themselves.
//!
//! A pattern is a URL with a bracketed range in it:
//! `https://example.com/photo[001-250].jpg` is 250 links, zero-padded to the
//! width of the first number, and `…/disc[a-c].iso` three. Several ranges in one
//! URL multiply. Only a range of two numbers or two single letters counts, which
//! is what keeps an IPv6 host such as `http://[::1]/` from being read as one.
//!
//! The C# used two regular expressions; they are simple enough to match by hand
//! here, which keeps a regex engine out of the binary.

use std::collections::HashSet;
use std::path::Path;

use crate::error::{CoreError, Result};
use crate::helpers::byte_formatter::format_bytes;
use crate::helpers::clipboard_links::parse_web_or_ftp_link;

/// The most links one batch will produce. A pattern is one typo away from
/// `[1-99999999]`, and a hundred million rows is a frozen window rather than a
/// download list.
pub const MAX_LINKS: usize = 5000;

/// The largest file [`read_import_file`] is asked to read; a page source is far smaller.
pub const MAX_IMPORT_BYTES: u64 = 10 * 1024 * 1024;

/// Every link in `text`, in the order they first appear, each once, with
/// patterns expanded. Anything that is not an http(s) or ftp(s) URL is ignored —
/// a pasted page source has plenty of those.
///
/// Fails when the text would produce more than [`MAX_LINKS`] links.
pub fn extract_links(text: &str) -> Result<Vec<String>> {
    let mut links = Vec::new();
    let mut seen = HashSet::new();

    for raw in find_link_matches(text) {
        let candidate = trim_trailing_punctuation(&web_utility_decode(raw));
        for link in expand_pattern(&candidate)? {
            if parse_web_or_ftp_link(&link).is_none() || !seen.insert(link.clone()) {
                continue;
            }
            links.push(link);
            if links.len() > MAX_LINKS {
                return Err(CoreError::invalid(format!(
                    "That is more than {} links. Split it into smaller batches.",
                    n0(MAX_LINKS as i64)
                )));
            }
        }
    }
    Ok(links)
}

/// The links a pattern stands for; a URL with no range in it is just itself.
///
/// Fails when the pattern stands for more than [`MAX_LINKS`] links.
pub fn expand_pattern(template: &str) -> Result<Vec<String>> {
    let Some(range) = find_range(template) else {
        return Ok(vec![template.to_string()]);
    };

    let values = range_values(&range, template)?;
    let head = &template[..range.start];
    let tail_template = &template[range.end..];

    // The tail may hold further ranges; each value of this one multiplies them.
    let tails = expand_pattern(tail_template)?;
    let count = values.len() as i64 * tails.len() as i64;
    if count > MAX_LINKS as i64 {
        return Err(CoreError::invalid(format!(
            "That pattern stands for {} links; the most a batch takes is {}.",
            n0(count),
            n0(MAX_LINKS as i64)
        )));
    }

    let mut expanded = Vec::with_capacity(count as usize);
    for value in &values {
        for tail in &tails {
            expanded.push(format!("{head}{value}{tail}"));
        }
    }
    Ok(expanded)
}

/// Whether `text` holds a range a pattern would expand.
pub fn has_pattern(text: &str) -> bool {
    find_range(text).is_some()
}

/// Reads a file of links, refusing one too large to be a list of links or a
/// page source — see [`MAX_IMPORT_BYTES`].
pub fn read_import_file(path: &Path) -> Result<String> {
    let len = std::fs::metadata(path)?.len();
    if len > MAX_IMPORT_BYTES {
        return Err(CoreError::invalid(format!(
            "That file is {}; a list of links is never that big.",
            format_bytes(len as i64)
        )));
    }
    let bytes = std::fs::read(path)?;
    let bytes = crate::serde_compat::strip_bom(&bytes);
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

// ---------------------------------------------------------------------------
// The range pattern: \[(?:(\d{1,9})-(\d{1,9})|([a-zA-Z])-([a-zA-Z]))\]
// ---------------------------------------------------------------------------

enum RangeKind<'a> {
    Numbers { from: &'a str, to: &'a str },
    Letters { first: u8, last: u8 },
}

struct RangeMatch<'a> {
    start: usize,
    end: usize,
    kind: RangeKind<'a>,
}

/// The leftmost bracketed range in `text`.
fn find_range(text: &str) -> Option<RangeMatch<'_>> {
    let bytes = text.as_bytes();
    for (start, _) in text.match_indices('[') {
        if let Some(found) = range_at(text, bytes, start) {
            return Some(found);
        }
    }
    None
}

fn range_at<'a>(text: &'a str, bytes: &[u8], start: usize) -> Option<RangeMatch<'a>> {
    let mut i = start + 1;
    let digits = |from: usize| bytes[from..].iter().take_while(|b| b.is_ascii_digit()).count();

    let from_len = digits(i);
    if (1..=9).contains(&from_len) {
        let from = &text[i..i + from_len];
        i += from_len;
        if bytes.get(i) != Some(&b'-') {
            return None;
        }
        i += 1;
        let to_len = digits(i);
        if !(1..=9).contains(&to_len) {
            return None;
        }
        let to = &text[i..i + to_len];
        i += to_len;
        if bytes.get(i) != Some(&b']') {
            return None;
        }
        return Some(RangeMatch { start, end: i + 1, kind: RangeKind::Numbers { from, to } });
    }

    let first = *bytes.get(i)?;
    let last = *bytes.get(i + 2)?;
    if first.is_ascii_alphabetic() && bytes.get(i + 1) == Some(&b'-') && last.is_ascii_alphabetic() && bytes.get(i + 3) == Some(&b']') {
        return Some(RangeMatch { start, end: i + 4, kind: RangeKind::Letters { first, last } });
    }
    None
}

fn range_values(range: &RangeMatch<'_>, template: &str) -> Result<Vec<String>> {
    match range.kind {
        RangeKind::Numbers { from: from_text, to: to_text } => {
            // Nine digits at most, so these always parse.
            let from: i64 = from_text.parse().unwrap_or(0);
            let to: i64 = to_text.parse().unwrap_or(0);

            // "[001-250]" pads to three digits; "[1-250]" does not pad at all.
            let width = if from_text.len() > 1 && from_text.starts_with('0') { from_text.len() } else { 0 };
            let step = if from <= to { 1 } else { -1 };
            let count = (to - from).abs() + 1;
            if count > MAX_LINKS as i64 {
                return Err(CoreError::invalid(format!(
                    "A range of {} is more than a batch takes ({}).",
                    n0(count),
                    n0(MAX_LINKS as i64)
                )));
            }
            let mut numbers = Vec::with_capacity(count as usize);
            let mut n = from;
            loop {
                numbers.push(format!("{n:0width$}"));
                if n == to {
                    break;
                }
                n += step;
            }
            Ok(numbers)
        }
        RangeKind::Letters { first, last } => {
            // A range that changes case half way ("[a-Z]") has no obvious meaning,
            // and the ASCII table between them is punctuation.
            if first.is_ascii_uppercase() != last.is_ascii_uppercase() {
                return Ok(vec![template[range.start..range.end].to_string()]);
            }
            let mut letters = Vec::new();
            let mut c = first;
            loop {
                letters.push((c as char).to_string());
                if c == last {
                    break;
                }
                if first <= last { c += 1 } else { c -= 1 }
            }
            Ok(letters)
        }
    }
}

// ---------------------------------------------------------------------------
// The link pattern: (?:https?|ftps?)://[^\s"'<>`]+, case-insensitive
// ---------------------------------------------------------------------------

/// Every non-overlapping link-shaped run of `text`, leftmost first.
fn find_link_matches(text: &str) -> Vec<&str> {
    const SCHEMES: [&str; 4] = ["https://", "http://", "ftps://", "ftp://"];
    let is_body = |c: char| !c.is_whitespace() && !matches!(c, '"' | '\'' | '<' | '>' | '`');

    let mut found = Vec::new();
    let mut pos = 0;
    while pos < text.len() {
        let rest = &text[pos..];
        let scheme = SCHEMES
            .iter()
            .find(|s| rest.len() >= s.len() && rest.as_bytes()[..s.len()].eq_ignore_ascii_case(s.as_bytes()));
        if let Some(scheme) = scheme {
            let body_start = pos + scheme.len();
            let body_len: usize = text[body_start..].chars().take_while(|&c| is_body(c)).map(char::len_utf8).sum();
            if body_len > 0 {
                found.push(&text[pos..body_start + body_len]);
                pos = body_start + body_len;
                continue;
            }
        }
        pos += rest.chars().next().map_or(1, char::len_utf8);
    }
    found
}

/// A link at the end of a sentence, or inside parentheses, picks up the
/// punctuation around it. Trailing `.`, `,`, `;` and `:` are never the end of a
/// file URL worth keeping; a closing bracket is kept only when the link opened one.
fn trim_trailing_punctuation(link: &str) -> String {
    let mut link = link;
    while let Some(last) = link.chars().last() {
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?') {
            link = &link[..link.len() - 1];
            continue;
        }
        let count = |c: char| link.chars().filter(|&x| x == c).count();
        if (last == ')' && count('(') < count(')')) || (last == ']' && count('[') < count(']')) {
            link = &link[..link.len() - 1];
            continue;
        }
        break;
    }
    link.to_string()
}

/// A page source writes `&amp;` for the ampersand between query parameters.
/// (`WebUtility.HtmlDecode`, for the entities a link can realistically carry.)
fn web_utility_decode(link: &str) -> String {
    if !link.contains('&') {
        return link.to_string();
    }
    let mut out = String::with_capacity(link.len());
    let mut rest = link;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let decoded = after.find(';').filter(|&semi| semi > 0 && semi <= 10).and_then(|semi| {
            let entity = &after[..semi];
            decode_entity(entity).map(|c| (c, semi + 1))
        });
        match decoded {
            Some((c, consumed)) => {
                out.push(c);
                rest = &after[consumed..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_entity(entity: &str) -> Option<char> {
    if let Some(num) = entity.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse::<u32>().ok()?,
        };
        return char::from_u32(code);
    }
    Some(match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        _ => return None,
    })
}

/// `{0:N0}`: digits grouped in threes with commas.
fn n0(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(d);
    }
    out
}

#[cfg(test)]
mod tests {
    //! The traps are the ones a URL sets for a naive parser: an IPv6 host looks
    //! like a range, a sentence ends a link with a full stop, a page source
    //! escapes its ampersands, and a typo in a range is a hundred million rows.

    use super::*;

    // ------------------------------------------------------------ patterns --

    #[test]
    fn a_numbered_range_pads_to_the_width_of_its_first_number() {
        assert_eq!(
            expand_pattern("https://example.com/photo[008-011].jpg").unwrap(),
            [
                "https://example.com/photo008.jpg",
                "https://example.com/photo009.jpg",
                "https://example.com/photo010.jpg",
                "https://example.com/photo011.jpg"
            ]
        );
    }

    #[test]
    fn a_range_without_a_leading_zero_is_not_padded() {
        assert_eq!(expand_pattern("https://x/[9-10].bin").unwrap(), ["https://x/9.bin", "https://x/10.bin"]);
    }

    #[test]
    fn a_letter_range_and_a_reversed_range_both_expand() {
        assert_eq!(
            expand_pattern("https://x/disc-[a-c].iso").unwrap(),
            ["https://x/disc-a.iso", "https://x/disc-b.iso", "https://x/disc-c.iso"]
        );
        assert_eq!(expand_pattern("https://x/[3-1]").unwrap(), ["https://x/3", "https://x/2", "https://x/1"]);
    }

    #[test]
    fn several_ranges_in_one_address_multiply() {
        let links = expand_pattern("https://x/s[1-2]/e[01-03].mkv").unwrap();
        assert_eq!(links.len(), 6);
        assert_eq!(links[0], "https://x/s1/e01.mkv");
        assert_eq!(links[5], "https://x/s2/e03.mkv");
    }

    #[test]
    fn brackets_that_are_not_a_range_are_left_alone() {
        for url in ["http://[::1]/file.bin", "http://[2001:db8::1]:8080/file.bin", "https://x/[a-Z].bin"] {
            assert_eq!(expand_pattern(url).unwrap(), [url]);
        }
    }

    #[test]
    fn a_pattern_that_stands_for_too_many_links_is_refused() {
        // One typo away from a hundred million rows.
        assert!(matches!(expand_pattern("https://x/[1-99999999].jpg"), Err(CoreError::Invalid(_))));
        let err = expand_pattern("https://x/[1-1000]/[1-1000].jpg").unwrap_err();
        assert_eq!(err.to_string(), "That pattern stands for 1,000,000 links; the most a batch takes is 5,000.");
    }

    #[test]
    fn has_pattern_finds_only_real_ranges() {
        assert!(has_pattern("https://x/[1-3].bin"));
        assert!(!has_pattern("http://[::1]/file.bin"));
        assert!(!has_pattern(""));
    }

    // ------------------------------------------------------------ extraction --

    #[test]
    fn links_are_found_one_per_line_in_order_and_once_each() {
        let links = extract_links(
            "https://example.com/a.zip\n  ftp://files.example.com/b.iso\n\nhttps://example.com/a.zip\nnot a link",
        )
        .unwrap();
        assert_eq!(links, ["https://example.com/a.zip", "ftp://files.example.com/b.iso"]);
    }

    #[test]
    fn links_are_found_inside_a_page_source_with_their_ampersands_unescaped() {
        let html = r#"<a href="https://example.com/get?id=1&amp;file=a.zip">A</a> <a href='https://example.com/b.zip'>B</a>"#;
        assert_eq!(extract_links(html).unwrap(), ["https://example.com/get?id=1&file=a.zip", "https://example.com/b.zip"]);
    }

    #[test]
    fn the_punctuation_a_sentence_puts_after_a_link_is_not_part_of_it() {
        let links = extract_links(
            "Get https://example.com/a.zip, or (https://example.com/b.zip). Also https://example.com/wiki/Foo_(bar).",
        )
        .unwrap();
        assert_eq!(
            links,
            ["https://example.com/a.zip", "https://example.com/b.zip", "https://example.com/wiki/Foo_(bar)"]
        );
    }

    #[test]
    fn only_web_and_ftp_links_are_taken() {
        let links = extract_links("javascript:alert(1) file:///C:/x.exe mailto:a@b.c https://ok.example.com/f").unwrap();
        assert_eq!(links, ["https://ok.example.com/f"]);
    }

    #[test]
    fn a_pasted_pattern_is_expanded_where_it_stands() {
        let links = extract_links("https://x/first.bin\nhttps://x/part[1-3].rar\nhttps://x/last.bin").unwrap();
        assert_eq!(
            links,
            ["https://x/first.bin", "https://x/part1.rar", "https://x/part2.rar", "https://x/part3.rar", "https://x/last.bin"]
        );
    }

    #[test]
    fn nothing_to_find_is_an_empty_list_not_an_error() {
        assert!(extract_links("").unwrap().is_empty());
    }

    #[test]
    fn an_import_file_too_big_to_be_a_list_of_links_is_refused_unread() {
        let dir = std::env::temp_dir().join(format!("qb-batch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge.txt");
        std::fs::File::create(&path).unwrap().set_len(MAX_IMPORT_BYTES + 1).unwrap();

        assert!(matches!(read_import_file(&path), Err(CoreError::Invalid(_))));

        std::fs::write(&path, b"\xEF\xBB\xBFhttps://x/a.zip").unwrap();
        assert_eq!(read_import_file(&path).unwrap(), "https://x/a.zip");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn n0_groups_thousands() {
        assert_eq!(n0(0), "0");
        assert_eq!(n0(999), "999");
        assert_eq!(n0(5000), "5,000");
        assert_eq!(n0(99_999_999), "99,999,999");
    }
}
