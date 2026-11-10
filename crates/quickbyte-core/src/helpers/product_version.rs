//! Port of `src/QuickByte.Core/Helpers/ProductVersion.cs`.
//!
//! Compares the product-version strings QuickByte actually deals with: "1.3.0"
//! off the running build, and whatever the release manifest was hand-written
//! with ("v1.3.0", "1.3.0-beta.1", "1.3").
//!
//! A plain four-part version alone can't do this — it rejects a "v" prefix and a
//! pre-release suffix outright, and it has no notion that "1.3.0-beta.1"
//! precedes "1.3.0". Getting that last part wrong is what would offer a beta
//! user an "update" to the build they are already running.

/// `System.Version`: major.minor.build.revision, compared component by component.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: i32,
    pub minor: i32,
    pub build: i32,
    pub revision: i32,
}

impl Version {
    pub const fn new(major: i32, minor: i32, build: i32, revision: i32) -> Self {
        Self { major, minor, build, revision }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}.{}", self.major, self.minor, self.build, self.revision)
    }
}

/// True when `candidate` is a strictly newer release than `current`. Anything
/// unparseable answers false: an update prompt is worth suppressing when the
/// input is nonsense, and the alternative is prompting on every launch forever.
pub fn is_newer(candidate: Option<&str>, current: Option<&str>) -> bool {
    let Some((candidate_number, candidate_pre)) = try_parse(candidate) else { return false };
    let Some((current_number, current_pre)) = try_parse(current) else { return false };

    match candidate_number.cmp(&current_number) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        // Same numbers: a release supersedes a pre-release of itself ("1.3.0"
        // over "1.3.0-beta.1"), and nothing supersedes a release.
        std::cmp::Ordering::Equal => !current_pre.is_empty() && candidate_pre.is_empty(),
    }
}

/// Splits a product version into its numeric part and its pre-release label,
/// normalizing the two things the numeric parser won't take: a leading "v", and
/// a "-suffix"/"+metadata" tail.
pub fn try_parse(text: Option<&str>) -> Option<(Version, String)> {
    let text = text?.trim();
    if text.is_empty() {
        return None;
    }

    let mut trimmed = text;
    if trimmed.len() > 1 && (trimmed.starts_with('v') || trimmed.starts_with('V')) {
        trimmed = &trimmed[1..];
    }

    // Build metadata never affects ordering, so it is dropped before the
    // pre-release label is read off.
    if let Some(plus) = trimmed.find('+') {
        trimmed = &trimmed[..plus];
    }

    let mut prerelease = String::new();
    if let Some(dash) = trimmed.find('-') {
        prerelease = trimmed[dash + 1..].to_string();
        trimmed = &trimmed[..dash];
    }

    // A version wants at least two components; "1" is a legal thing for someone
    // to write in a manifest.
    let padded;
    if !trimmed.contains('.') {
        padded = format!("{trimmed}.0");
        trimmed = &padded;
    }

    let parts: Vec<&str> = trimmed.split('.').collect();
    if !(2..=4).contains(&parts.len()) {
        return None;
    }
    let mut numbers = [0i32; 4];
    for (i, part) in parts.iter().enumerate() {
        let part = part.trim();
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        numbers[i] = part.parse().ok()?;
    }

    // An omitted component would otherwise order "1.3" *before* "1.3.0".
    // Padding the missing parts out to zero makes the two spellings of the same
    // release compare equal, which is what a person writing either one into a
    // manifest means.
    Some((Version::new(numbers[0], numbers[1], numbers[2], numbers[3]), prerelease))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_is_true_for_a_higher_number() {
        for (candidate, current) in [("1.4.0", "1.3.0"), ("1.3.1", "1.3.0"), ("2.0.0", "1.9.9"), ("1.3.0.1", "1.3.0")] {
            assert!(is_newer(Some(candidate), Some(current)), "{candidate} > {current}");
        }
    }

    #[test]
    fn is_newer_is_false_for_the_same_or_a_lower_number() {
        for (candidate, current) in [("1.3.0", "1.4.0"), ("1.3.0", "1.3.0"), ("1.2.9", "1.3.0")] {
            assert!(!is_newer(Some(candidate), Some(current)));
        }
    }

    #[test]
    fn is_newer_tolerates_the_v_a_release_note_is_written_with() {
        for (candidate, current) in [("v1.4.0", "1.3.0"), ("V1.4.0", "1.3.0"), ("1.4.0", "v1.3.0")] {
            assert!(is_newer(Some(candidate), Some(current)));
        }
    }

    #[test]
    fn is_newer_treats_a_missing_component_as_zero() {
        assert!(!is_newer(Some("1.3"), Some("1.3.0")));
        assert!(!is_newer(Some("1.3.0"), Some("1.3")));
        assert!(is_newer(Some("1.4"), Some("1.3.0")));
    }

    #[test]
    fn is_newer_offers_a_release_to_someone_on_its_pre_release() {
        assert!(is_newer(Some("1.4.0"), Some("1.4.0-beta.1")));
    }

    #[test]
    fn is_newer_never_offers_a_pre_release_of_a_version_already_installed() {
        assert!(!is_newer(Some("1.4.0-beta.1"), Some("1.4.0")));
        assert!(!is_newer(Some("1.4.0-beta.1"), Some("1.4.0-beta.1")));
    }

    #[test]
    fn is_newer_still_compares_the_numbers_of_two_pre_releases() {
        assert!(is_newer(Some("1.5.0-beta.1"), Some("1.4.0-beta.1")));
    }

    #[test]
    fn is_newer_ignores_build_metadata() {
        assert!(!is_newer(Some("1.3.0+build.99"), Some("1.3.0")));
    }

    #[test]
    fn is_newer_answers_no_to_anything_unparseable() {
        for (candidate, current) in [
            (None, Some("1.3.0")),
            (Some(""), Some("1.3.0")),
            (Some("   "), Some("1.3.0")),
            (Some("not a version"), Some("1.3.0")),
            (Some("1.3.0"), None),
            (Some("1.3.0"), Some("garbage")),
        ] {
            assert!(!is_newer(candidate, current));
        }
    }

    #[test]
    fn try_parse_splits_the_number_from_the_pre_release_label() {
        let (version, prerelease) = try_parse(Some("v1.4.0-beta.2+sha.abc")).unwrap();
        assert_eq!(version, Version::new(1, 4, 0, 0));
        assert_eq!(prerelease, "beta.2");
    }

    #[test]
    fn try_parse_pads_a_missing_component_to_zero() {
        assert_eq!(try_parse(Some("1.3")).unwrap().0, Version::new(1, 3, 0, 0));
    }

    #[test]
    fn try_parse_accepts_a_single_component() {
        assert_eq!(try_parse(Some("2")).unwrap().0, Version::new(2, 0, 0, 0));
    }

    #[test]
    fn try_parse_reports_failure_without_panicking() {
        assert_eq!(try_parse(Some("nonsense")), None);
    }
}
