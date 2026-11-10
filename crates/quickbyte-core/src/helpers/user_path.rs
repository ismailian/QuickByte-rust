//! Port of `UserPath.cs`: turns a folder under the user's own profile into the
//! short `~\…` form for display, and back again for use.
//!
//! It exists for one field: Add Download's "Save to". That box is nearly always
//! showing a folder inside the current user's profile, and
//! `C:\Users\somebody\Downloads\QuickByte` is both wider than the box and almost
//! entirely made of characters that tell the reader nothing — the interesting
//! part is the tail, and the tail is what gets clipped. `~\Downloads\QuickByte`
//! fits, and says the same thing.
//!
//! A path outside the profile is left exactly as it is. That is the point of the
//! abbreviation rather than a limitation of it: `D:\Media` or `\\nas\share` is
//! somewhere the user deliberately chose, and the whole path is the information.
//!
//! [`expand`] is not optional — the shortened text is shown in an editable box
//! whose value becomes `DownloadRequest::save_folder`, and a literal `~` reaching
//! `create_dir_all` creates a folder named `~` next to the executable. Every read
//! of that box goes through here.

use std::sync::OnceLock;

const TILDE: char = '~';
const SEPARATOR: char = '\\';
const ALT_SEPARATOR: char = '/';

/// The profile folder, resolved once. It can come back empty on a stripped-down
/// or service account, and an empty prefix would match every path — so an empty
/// answer disables the abbreviation entirely rather than turning every path into `~`.
pub fn profile() -> &'static str {
    static PROFILE: OnceLock<String> = OnceLock::new();
    PROFILE.get_or_init(|| trim_ending_directory_separator(&std::env::var("USERPROFILE").unwrap_or_default()).to_string())
}

/// `path` with the user's profile folder replaced by `~`, or unchanged when it is
/// somewhere else.
pub fn shorten(path: Option<&str>) -> String {
    shorten_with(profile(), path)
}

/// The real path behind a possibly-shortened one. Anything that does not start
/// with `~` at a folder boundary is returned untouched — including a folder whose
/// name genuinely begins with a tilde.
pub fn expand(text: Option<&str>) -> String {
    expand_with(profile(), text)
}

/// [`shorten`] against a given profile folder.
pub fn shorten_with(profile: &str, path: Option<&str>) -> String {
    let Some(path) = path else { return String::new() };
    if path.trim().is_empty() || profile.is_empty() {
        return path.to_string();
    }
    let trimmed = trim_ending_directory_separator(path.trim());

    if eq_ignore_case(trimmed, profile) {
        return TILDE.to_string();
    }

    // The separator is part of the comparison, or C:\Users\bobby matches a path
    // under C:\Users\bob and is abbreviated into a folder that is not the one it names.
    let under_profile = trimmed
        .get(..profile.len())
        .is_some_and(|head| eq_ignore_case(head, profile) && trimmed[profile.len()..].starts_with(is_separator));
    if under_profile {
        return format!("{TILDE}{}", &trimmed[profile.len()..]);
    }
    trimmed.to_string()
}

/// [`expand`] against a given profile folder.
pub fn expand_with(profile: &str, text: Option<&str>) -> String {
    let Some(text) = text else { return String::new() };
    if text.trim().is_empty() {
        return text.to_string();
    }
    let trimmed = text.trim();
    if !trimmed.starts_with(TILDE) || profile.is_empty() {
        return trimmed.to_string();
    }
    let rest = &trimmed[TILDE.len_utf8()..];
    if rest.is_empty() {
        return profile.to_string();
    }
    if !rest.starts_with(is_separator) {
        return trimmed.to_string();
    }
    // Separators normalised on the way out: people type "~/Downloads", and the
    // expanded path is persisted on the DownloadItem and shown in the details
    // window. A path with both separators in it works, and looks like a bug.
    format!("{profile}{}", rest.replace(ALT_SEPARATOR, &SEPARATOR.to_string()))
}

fn is_separator(c: char) -> bool {
    c == SEPARATOR || c == ALT_SEPARATOR
}

/// `OrdinalIgnoreCase`: case-folded per character, so a non-ASCII user name
/// still matches however it is cased.
fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.chars().count() == b.chars().count()
        && a.chars().zip(b.chars()).all(|(x, y)| x == y || x.to_uppercase().eq(y.to_uppercase()))
}

/// `Path.TrimEndingDirectorySeparator`: drops one trailing separator, unless it
/// is part of the root (`E:\`, `\`).
pub(crate) fn trim_ending_directory_separator(path: &str) -> &str {
    if !path.ends_with(is_separator) || path.len() <= root_length(path) {
        return path;
    }
    &path[..path.len() - 1]
}

fn root_length(path: &str) -> usize {
    let b = path.as_bytes();
    let sep = |c: u8| c == b'\\' || c == b'/';
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return if b.len() >= 3 && sep(b[2]) { 3 } else { 2 };
    }
    if b.len() >= 2 && sep(b[0]) && sep(b[1]) {
        // \\server\share
        let mut separators = 0;
        for (i, &c) in b.iter().enumerate().skip(2) {
            if sep(c) {
                separators += 1;
                if separators == 2 {
                    return i;
                }
            }
        }
        return b.len();
    }
    usize::from(b.first().is_some_and(|&c| sep(c)))
}

#[cfg(test)]
mod tests {
    //! Two things here are not cosmetic. The prefix match has to respect folder
    //! boundaries, or a path under `C:\Users\bobby` is displayed as though it
    //! were under `C:\Users\bob`'s profile — a wrong answer about where a file is
    //! going. And the round trip has to hold: the shortened text sits in an
    //! editable box whose value becomes the download's save folder, so a `~`
    //! that reaches `create_dir_all` unexpanded creates a folder literally named
    //! `~` beside the executable.

    use super::*;

    const PROFILE: &str = r"C:\Users\somebody";

    fn combine(parts: &[&str]) -> String {
        parts.join(r"\")
    }

    #[test]
    fn a_folder_in_the_profile_is_shown_short() {
        let full = combine(&[PROFILE, "Downloads", "QuickByte"]);
        assert_eq!(shorten_with(PROFILE, Some(&full)), r"~\Downloads\QuickByte");
    }

    #[test]
    fn the_profile_itself_is_just_the_tilde() {
        assert_eq!(shorten_with(PROFILE, Some(PROFILE)), "~");
    }

    #[test]
    fn a_trailing_separator_does_not_defeat_the_match() {
        assert_eq!(shorten_with(PROFILE, Some(&format!(r"{PROFILE}\Documents\"))), r"~\Documents");
    }

    #[test]
    fn the_match_is_case_insensitive_like_the_file_system() {
        let full = combine(&[&PROFILE.to_uppercase(), "Music"]);
        assert_eq!(shorten_with(PROFILE, Some(&full)), r"~\Music");
    }

    #[test]
    fn another_users_profile_is_left_alone() {
        // Without the separator in the comparison, C:\Users\bobby starts with
        // C:\Users\bob and is abbreviated into a folder that belongs to somebody else.
        let full = combine(&[&format!("{PROFILE}2"), "Downloads"]);
        assert_eq!(shorten_with(PROFILE, Some(&full)), full);
    }

    #[test]
    fn a_folder_the_user_deliberately_chose_is_shown_in_full() {
        // Somewhere off the profile is somewhere the user picked on purpose, and
        // the whole path is the information.
        for path in [r"D:\Media\ISOs", r"E:\", r"\\nas\share\downloads", r"C:\Temp"] {
            assert_eq!(shorten_with(PROFILE, Some(path)), trim_ending_directory_separator(path));
        }
        assert_eq!(shorten_with(PROFILE, Some(r"E:\")), r"E:\");
    }

    #[test]
    fn expand_puts_the_profile_back() {
        assert_eq!(expand_with(PROFILE, Some(r"~\Downloads\QuickByte")), combine(&[PROFILE, "Downloads", "QuickByte"]));
        assert_eq!(expand_with(PROFILE, Some("~")), PROFILE);
    }

    #[test]
    fn expand_accepts_the_forward_slash_a_user_may_type() {
        assert_eq!(expand_with(PROFILE, Some("~/Documents")), combine(&[PROFILE, "Documents"]));
    }

    #[test]
    fn expand_leaves_a_real_folder_that_starts_with_a_tilde_alone() {
        // "~work" is a legal folder name, and it is not an abbreviation. Only a
        // tilde at a folder boundary means the profile.
        assert_eq!(expand_with(PROFILE, Some(r"D:\~work")), r"D:\~work");
        assert_eq!(expand_with(PROFILE, Some("~work")), "~work");
    }

    #[test]
    fn nothing_in_nothing_out() {
        // The box can be empty while the user is retyping it; neither direction
        // may fail on the way to the validation that reports it.
        for text in [Some(""), Some("   "), None] {
            assert_eq!(shorten(text), text.unwrap_or_default());
            assert_eq!(expand(text), text.unwrap_or_default());
        }
    }

    #[test]
    fn shorten_and_expand_round_trip() {
        for relative in ["Downloads", "Documents", r"Downloads\QuickByte"] {
            let full = combine(&[PROFILE, relative]);
            assert_eq!(expand_with(PROFILE, Some(&shorten_with(PROFILE, Some(&full)))), full);
        }
    }

    #[test]
    fn the_real_profile_round_trips_too() {
        let profile = profile();
        if profile.is_empty() {
            return;
        }
        let full = combine(&[profile, "Downloads", "QuickByte"]);
        assert_eq!(shorten(Some(&full)), r"~\Downloads\QuickByte");
        assert_eq!(expand(Some(&shorten(Some(&full)))), full);
    }

    #[test]
    fn an_empty_profile_disables_the_abbreviation() {
        assert_eq!(shorten_with("", Some(r"C:\Users\x")), r"C:\Users\x");
        assert_eq!(expand_with("", Some(r"~\x")), r"~\x");
    }
}
