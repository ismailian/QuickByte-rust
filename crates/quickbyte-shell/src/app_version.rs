//! Port of `src/QuickByte.UI/AppVersion.cs`.
//!
//! Reads the running build's version, so no window ever hardcodes a version
//! string. The numbers themselves are set in one place — `[workspace.package]
//! version` in `rust/Cargo.toml` — and flow here through `quickbyte_core::VERSION`.
//!
//! **Why not the binary's `QB_PRODUCT_VERSION`:** each binary's `build.rs` stamps
//! `QB_PRODUCT_VERSION` / `QB_FILE_VERSION` into *that binary's* compile, and a
//! library crate cannot see another crate's build-script environment. So this
//! module rebuilds the same two strings from the same inputs the build script
//! uses: the workspace version (identical for every crate) plus the
//! `QB_VERSION_SUFFIX` / `QB_BUILD_REVISION` environment variables, read here
//! with `option_env!` at this crate's compile time (rustc tracks them, so a
//! changed value rebuilds this crate). The result matches the executable's
//! VERSIONINFO resource, which is what makes it the useful string on a bug report.

use std::sync::OnceLock;

/// The product version (`1.27.0`, or `1.27.0-beta.1` with QB_VERSION_SUFFIX) —
/// the version as a person would say it, minus any `+build` metadata.
pub fn display() -> &'static str {
    static DISPLAY: OnceLock<String> = OnceLock::new();
    DISPLAY.get_or_init(|| display_from(quickbyte_core::VERSION, option_env!("QB_VERSION_SUFFIX")))
}

/// The four-part file version (`1.27.0.137`), including the build number a CI
/// run stamped in. This is the string that matches the .exe's Properties page.
pub fn file() -> &'static str {
    static FILE: OnceLock<String> = OnceLock::new();
    FILE.get_or_init(|| file_from(quickbyte_core::VERSION, option_env!("QB_BUILD_REVISION")))
}

/// The copyright line, as `build_support.rs` stamps it into every executable.
pub fn copyright() -> &'static str {
    "Copyright \u{a9} 2026 QuickByte"
}

fn display_from(version: &str, suffix: Option<&str>) -> String {
    // Build metadata after a '+' is useful in a log, noise in a title bar.
    let version = version.split('+').next().unwrap_or(version);
    match suffix.map(str::trim) {
        Some(suffix) if !suffix.is_empty() => format!("{version}-{}", suffix.split('+').next().unwrap_or(suffix)),
        _ => version.to_string(),
    }
}

fn file_from(version: &str, revision: Option<&str>) -> String {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let (major, minor, patch) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    let revision = revision.and_then(|r| r.trim().parse::<u64>().ok()).unwrap_or(0);
    format!("{major}.{minor}.{patch}.{revision}")
}

#[cfg(test)]
mod tests {
    //! Port of `tests/QuickByte.UI.Tests/AppVersionTests.cs`, where meaningful.

    use super::*;

    #[test]
    fn the_displayed_version_is_the_three_part_number_a_person_would_say() {
        let parts: Vec<&str> = display().split('-').next().unwrap().split('.').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.parse::<u32>().is_ok()), "{}", display());
    }

    #[test]
    fn the_displayed_version_is_the_workspace_version() {
        assert!(display().starts_with(quickbyte_core::VERSION));
    }

    #[test]
    fn build_metadata_never_reaches_a_title_bar() {
        assert_eq!(display_from("1.27.0+abc123", None), "1.27.0");
        assert_eq!(display_from("1.27.0", Some(" beta.1 ")), "1.27.0-beta.1");
        assert_eq!(display_from("1.27.0", Some("")), "1.27.0");
        assert!(!display().contains('+'));
    }

    #[test]
    fn the_file_version_is_the_four_part_one_from_the_exes_properties_page() {
        assert_eq!(file().split('.').count(), 4);
        assert_eq!(file_from("1.27.0", Some("137")), "1.27.0.137");
        assert_eq!(file_from("1.27.0", None), "1.27.0.0");
        assert_eq!(file_from("1.27.0-beta.1", Some("x")), "1.27.0.0");
    }

    #[test]
    fn the_file_version_agrees_with_the_displayed_one() {
        let display = display().split('-').next().unwrap();
        assert!(file().starts_with(&format!("{display}.")));
    }

    #[test]
    fn the_copyright_line_is_the_one_stamped_into_the_executables() {
        assert!(copyright().contains("QuickByte"));
        assert!(include_str!("../../../build_support.rs").contains(r#""Copyright \u{a9} 2026 QuickByte""#));
    }
}
