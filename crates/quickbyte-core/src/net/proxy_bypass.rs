//! Port of `src/QuickByte.Core/Helpers/ProxyBypass.cs`.
//!
//! Which addresses skip a manual proxy — the list typed into Options, in the
//! forms Windows' own proxy settings accept, so a list copied from there means
//! the same thing here.

use std::net::IpAddr;

use url::Url;

/// More entries than this is not a bypass list anyone typed.
pub const MAX_ENTRIES: usize = 200;

/// Entries as typed, normalised: lower-case, no scheme, no trailing slash.
pub fn parse(list: Option<&str>) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for raw in list.unwrap_or("").split([';', ',', ' ', '\t', '\r', '\n']) {
        let entry = normalise(raw);
        if entry.is_empty() || entries.contains(&entry) {
            continue;
        }
        entries.push(entry);
        if entries.len() == MAX_ENTRIES {
            break;
        }
    }
    entries
}

/// The host as `Uri.IdnHost` gives it: punycode, without IPv6 brackets, lower-case.
fn host_of(destination: &Url) -> String {
    destination.host_str().unwrap_or("").trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase()
}

/// This machine. Never sent through a proxy: nothing a proxy could reach there
/// is this machine, and a local service asked through one answers for the
/// proxy's host instead.
pub fn is_local(destination: &Url) -> bool {
    let host = host_of(destination);
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

pub fn matches(entries: &[String], destination: &Url) -> bool {
    let host = host_of(destination);
    let port = destination.port_or_known_default().map(i32::from).unwrap_or(-1);

    for entry in entries {
        if entry == "<local>" {
            // Windows' meaning: a name with no dot in it, which only a local
            // network could resolve. An IP address is not such a name.
            if !host.contains('.') && host.parse::<IpAddr>().is_err() {
                return true;
            }
            continue;
        }

        let (pattern, entry_port) = split_port(entry);
        if entry_port.is_some_and(|p| p != port) {
            continue;
        }

        if let Some(domain) = pattern.strip_prefix('.') {
            // ".example.com" — the domain and everything under it.
            if host == domain || host.ends_with(pattern) {
                return true;
            }
            continue;
        }

        if pattern.contains('*') {
            if wildcard_matches(pattern, &host) {
                return true;
            }
            continue;
        }

        if host == pattern {
            return true;
        }
    }
    false
}

fn normalise(entry: &str) -> String {
    let mut e = entry.trim().to_lowercase();
    if let Some(scheme) = e.find("://") {
        e = e[scheme + 3..].to_string();
    }
    e.trim_end_matches('/').to_string()
}

/// "host:8080" → ("host", 8080). An IPv6 address's own colons are not a port.
fn split_port(entry: &str) -> (&str, Option<i32>) {
    if let Some(rest) = entry.strip_prefix('[')
        && let Some(close) = rest.find(']')
    {
        let address = &rest[..close];
        let after = &rest[close + 1..];
        let port = after.strip_prefix(':').and_then(|p| p.parse::<i32>().ok());
        return (address, port);
    }

    if let Some(colon) = entry.rfind(':')
        && colon > 0
        && entry.find(':') == Some(colon)
        && let Ok(port) = entry[colon + 1..].parse::<i32>()
    {
        return (&entry[..colon], Some(port));
    }
    (entry, None)
}

/// A `*` matches any run of characters; everything else matches itself. The
/// whole host must match, as the anchored pattern Windows uses does.
fn wildcard_matches(pattern: &str, host: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let h: Vec<char> = host.chars().collect();
    let (mut pi, mut hi) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while hi < h.len() {
        if pi < p.len() && p[pi] != '*' && p[pi] == h[hi] {
            pi += 1;
            hi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = hi;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            hi = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_is_read_in_any_of_the_separators_windows_accepts() {
        assert_eq!(
            parse(Some(" https://Example.com/;*.LAN, <local>\n10.* ;example.com")),
            vec!["example.com", "*.lan", "<local>", "10.*"]
        );
    }

    #[test]
    fn entries_match_as_windows_reads_them() {
        for (entry, url, expected) in [
            ("example.com", "https://example.com/a", true),
            ("example.com", "https://www.example.com/a", false),
            ("*.example.com", "https://www.example.com/a", true),
            ("*.example.com", "https://notexample.com/a", false),
            (".example.com", "https://example.com/", true),
            (".example.com", "https://deep.cdn.example.com/", true),
            (".example.com", "https://badexample.com/", false),
            ("10.*", "http://10.1.2.3/x", true),
            ("10.*", "http://110.1.2.3/x", false),
            ("<local>", "http://nas/share", true),
            ("<local>", "http://nas.lan/share", false),
            ("<local>", "http://10.0.0.1/", false),
            ("files.example.com:8080", "http://files.example.com:8080/", true),
            ("files.example.com:8080", "http://files.example.com/", false),
            ("[2001:db8::1]", "http://[2001:db8::1]/", true),
        ] {
            assert_eq!(matches(&parse(Some(entry)), &Url::parse(url).unwrap()), expected, "{entry} vs {url}");
        }
    }

    #[test]
    fn a_list_longer_than_anyone_types_is_cut_short() {
        let list = (0..1000).map(|i| format!("host{i}.example.com")).collect::<Vec<_>>().join(";");
        assert_eq!(parse(Some(&list)).len(), MAX_ENTRIES);
    }

    #[test]
    fn this_machine_is_local() {
        for url in ["http://localhost:9614/ping", "http://127.0.0.1/", "http://[::1]/", "http://app.localhost/"] {
            assert!(is_local(&Url::parse(url).unwrap()), "{url}");
        }
        assert!(!is_local(&Url::parse("http://example.com/").unwrap()));
    }
}
