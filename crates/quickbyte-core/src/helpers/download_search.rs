//! Port of `DownloadSearch.cs`: the search box above the downloads list — which
//! downloads a query finds.
//!
//! Every word of the query has to appear somewhere in the file name or in the
//! address — its host and path, unescaped, so "my file" finds `my%20file.zip`.
//! Case is ignored and word order does not matter, which is how people search a
//! list they half remember ("ubuntu iso", "github setup"). A query string is left
//! out: on a signed link it is a page of random tokens, and every S3 download
//! would match "amz".

use percent_encoding::percent_decode_str;

use crate::models::DownloadItem;

/// The words of a query; an empty query has none and matches everything.
pub fn terms(query: Option<&str>) -> Vec<String> {
    query.unwrap_or_default().split_whitespace().map(str::to_string).collect()
}

/// Whether `item` holds every one of `terms` (as [`terms`] split them).
pub fn matches_terms<S: AsRef<str>>(item: &DownloadItem, terms: &[S]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let haystack = format!("{}\n{}", item.file_name, searchable_address(&item.url)).to_lowercase();
    terms.iter().all(|term| haystack.contains(&term.as_ref().to_lowercase()))
}

pub fn matches(item: &DownloadItem, query: Option<&str>) -> bool {
    matches_terms(item, &terms(query))
}

fn searchable_address(url: &str) -> String {
    let Ok(uri) = url::Url::parse(url) else {
        return url.to_string();
    };
    let host = uri.host_str().unwrap_or_default();
    match percent_decode_str(uri.path()).decode_utf8() {
        Ok(path) => format!("{host}{path}"),
        Err(_) => format!("{host}{}", uri.path()),
    }
}

#[cfg(test)]
mod tests {
    //! People search a list they half remember — a word from the name, the site
    //! it came from — in whatever order the words occur to them.

    use super::*;

    fn ubuntu() -> DownloadItem {
        DownloadItem {
            file_name: "ubuntu-24.04-desktop-amd64.iso".into(),
            url: "https://releases.ubuntu.com/24.04/ubuntu-24.04-desktop-amd64.iso".into(),
            ..Default::default()
        }
    }

    fn signed() -> DownloadItem {
        DownloadItem {
            file_name: "report final.pdf".into(),
            url: "https://bucket.s3.amazonaws.com/docs/report%20final.pdf?X-Amz-Signature=deadbeef&X-Amz-Credential=secret".into(),
            ..Default::default()
        }
    }

    #[test]
    fn an_empty_search_finds_everything() {
        for query in [Some(""), Some("   "), None] {
            assert!(matches(&ubuntu(), query));
        }
    }

    #[test]
    fn every_word_is_found_in_any_order_and_case() {
        for query in ["ubuntu", "UBUNTU ISO", "iso ubuntu", "releases.ubuntu.com", "24.04"] {
            assert!(matches(&ubuntu(), Some(query)), "{query}");
        }
    }

    #[test]
    fn a_word_that_is_not_there_rules_the_download_out() {
        assert!(!matches(&ubuntu(), Some("ubuntu fedora")));
    }

    #[test]
    fn an_escaped_address_is_searched_the_way_it_reads() {
        assert!(matches(&signed(), Some("docs/report final")));
        assert!(matches(&signed(), Some("amazonaws")));
    }

    #[test]
    fn a_signed_link_s_query_string_is_not_searched() {
        // A page of random tokens that would match nearly anything.
        assert!(!matches(&signed(), Some("Signature")));
        assert!(!matches(&signed(), Some("deadbeef")));
    }

    #[test]
    fn a_download_with_an_address_that_does_not_parse_is_still_found_by_name() {
        let odd = DownloadItem { file_name: "thing.bin".into(), url: "not a url".into(), ..Default::default() };
        assert!(matches(&odd, Some("thing")));
        assert!(matches(&odd, Some("not")));
    }
}
