//! Port of `src/QuickByte.Core/Helpers/AppUserAgent.cs`.
//!
//! The `User-Agent` every HTTP request the engine makes presents: the one the
//! user typed into Options, when they typed a usable one, else QuickByte's own.
//!
//! It exists because the header is not cosmetic. Plenty of servers refuse a
//! client they do not recognise — a `403`, a redirect to a landing page, or an
//! HTML error body served with a `200` — and none of those is a failure
//! QuickByte can diagnose, so the only fix is to let the user say what to claim
//! to be.
//!
//! Live process-wide state, for the reason [`crate::net::app_proxy::AppProxy`]
//! is: the engine's HTTP client is shared and built once, so a User-Agent that
//! needed a restart to apply is one the user would reasonably assume is broken.
//! Unlike the proxy, though, it is never written onto the client's default
//! headers — see below.
//!
//! **Why per-request, and not on the client.** The download client is shared by
//! up to [`crate::models::DownloadSettings::MAX_CONNECTIONS`] connections per
//! download across every running download. Re-stamping its defaults when the
//! setting changes races every send on it. So [`current`] is an immutable string
//! swapped atomically, and [`crate::net::request_decorator`] writes it onto each
//! request as it is built. A request header also takes precedence over the
//! client's default, which is what makes the two mechanisms coexist.
//!
//! **Why the value is not validated into oblivion.** Only the rules that have to
//! hold are enforced (see [`normalize`]): a header value cannot carry a control
//! character, and a megabyte-long one is a mistake rather than a preference.
//! Beyond that the string is sent exactly as typed — the point of the setting is
//! to impersonate whatever a server will accept.

use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;

/// What QuickByte calls itself when the user has not chosen otherwise. The
/// engine's shared client also carries this as its default `User-Agent`, as a
/// backstop for any request path that does not go through the decorator.
pub const DEFAULT: &str = "QuickByte/1.0 (+https://github.com/ismailian/quickbyte)";

/// Longest custom value accepted. Generous next to the ~120 characters a real
/// browser sends, and small enough that a pasted accident cannot push a request
/// header into the size a server will refuse the whole request over.
pub const MAX_LENGTH: usize = 512;

fn slot() -> &'static RwLock<Option<Arc<str>>> {
    static CURRENT: OnceLock<RwLock<Option<Arc<str>>>> = OnceLock::new();
    CURRENT.get_or_init(|| RwLock::new(None))
}

/// The User-Agent to present right now. Never empty: a custom value that does
/// not survive [`normalize`] leaves [`DEFAULT`] in place rather than sending
/// nothing.
pub fn current() -> Arc<str> {
    slot().read().clone().unwrap_or_else(|| Arc::from(DEFAULT))
}

/// True when [`current`] is the user's value, not ours.
pub fn is_custom() -> bool {
    slot().read().as_deref().is_some_and(|v| v != DEFAULT)
}

/// Points [`current`] at `custom`, or back at [`DEFAULT`] when it is empty or
/// unusable. Called by the composition root at start-up and on every settings
/// change — including the change that clears the field, which is why this has
/// to handle `None` by restoring the default rather than by doing nothing.
pub fn configure(custom: Option<&str>) {
    *slot().write() = normalize(custom).map(Arc::from);
}

/// `value` trimmed, or `None` when it cannot be sent as a header value: empty,
/// longer than [`MAX_LENGTH`], or carrying a control character. That last one is
/// the load-bearing check — a stray CR or LF in a header value is request
/// splitting, and this value is persisted in `settings.json`, which a user may
/// well have edited by hand.
pub fn normalize(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_LENGTH {
        return None;
    }
    if trimmed.chars().any(char::is_control) {
        return None;
    }
    Some(trimmed.to_string())
}

/// Whether `value` is one Options can accept. Blank counts: it is how the user
/// asks for QuickByte's own.
pub fn is_acceptable(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.trim().is_empty()) || normalize(value).is_some()
}

/// Serialises the tests (here and elsewhere) that write the process-wide value.
#[cfg(test)]
pub(crate) static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// Restores the default when a test that configured the value ends.
    struct Restore(#[allow(dead_code)] parking_lot::MutexGuard<'static, ()>);
    impl Drop for Restore {
        fn drop(&mut self) {
            configure(None);
        }
    }
    fn exclusive() -> Restore {
        Restore(TEST_LOCK.lock())
    }

    #[test]
    fn a_browser_user_agent_is_kept_exactly_as_typed() {
        let chrome = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
        assert_eq!(normalize(Some(chrome)).as_deref(), Some(chrome));
    }

    #[test]
    fn surrounding_space_is_trimmed() {
        assert_eq!(normalize(Some("  curl/8.4.0\t ")).as_deref(), Some("curl/8.4.0"));
    }

    #[test]
    fn a_trailing_newline_is_trimmed_rather_than_refused() {
        // A pasted line is the ordinary way this field gets filled in. Only a
        // break *inside* the value is a refusal.
        assert_eq!(normalize(Some("curl/8.4.0\r\n")).as_deref(), Some("curl/8.4.0"));
    }

    #[test]
    fn nothing_typed_is_nothing_chosen() {
        for blank in [None, Some(""), Some("   "), Some("\t")] {
            assert_eq!(normalize(blank), None);
            // And Options accepts it: blank is how the user asks for QuickByte's own.
            assert!(is_acceptable(blank));
        }
    }

    #[test]
    fn a_control_character_is_refused_outright() {
        for injected in ["Mozilla/5.0\r\nX-Injected: yes", "Mozilla/5.0\nX-Injected: yes", "Mozilla/\0 5.0", "Mozilla/5.0 \u{1b}[31m"] {
            assert_eq!(normalize(Some(injected)), None, "{injected:?}");
            assert!(!is_acceptable(Some(injected)));
        }
    }

    #[test]
    fn a_control_character_inside_an_otherwise_fine_value_is_still_refused() {
        assert_eq!(normalize(Some("QuickByte\r\n/1.0")), None);
    }

    #[test]
    fn the_length_cap_is_inclusive() {
        assert!(normalize(Some(&"a".repeat(MAX_LENGTH))).is_some());
        assert!(normalize(Some(&"a".repeat(MAX_LENGTH + 1))).is_none());
    }

    #[test]
    fn the_cap_is_measured_after_trimming() {
        let padded = format!("  {}  ", "a".repeat(MAX_LENGTH));
        assert!(normalize(Some(&padded)).is_some());
    }

    #[test]
    fn nothing_configured_is_quickbytes_own() {
        let _guard = exclusive();
        assert_eq!(&*current(), DEFAULT);
        assert!(!is_custom());
    }

    #[test]
    fn configure_takes_the_users_value() {
        let _guard = exclusive();
        configure(Some("  Wget/1.21.4  "));
        assert_eq!(&*current(), "Wget/1.21.4");
        assert!(is_custom());
    }

    #[test]
    fn clearing_the_setting_puts_the_default_back() {
        // A configure that only wrote a non-empty value left the old one in
        // place for the rest of the process.
        let _guard = exclusive();
        configure(Some("Wget/1.21.4"));
        configure(None);
        assert_eq!(&*current(), DEFAULT);
        assert!(!is_custom());
    }

    #[test]
    fn an_unusable_value_leaves_the_default_rather_than_sending_nothing() {
        let _guard = exclusive();
        configure(Some("Mozilla/5.0\r\nX-Injected: yes"));
        assert_eq!(&*current(), DEFAULT);
    }

    #[test]
    fn the_default_is_what_the_shared_client_carries() {
        assert_eq!(DEFAULT, crate::net::http_client::USER_AGENT);
    }
}
