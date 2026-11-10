//! Keeps passwords out of the JSON files in plain text. Port of `SecretProtector.cs`.
//!
//! **Compatibility contract:** DPAPI, `CurrentUser` scope, entropy = the UTF-8
//! bytes of `"QuickByte.Credential.v1"`, base64 text. Values written by the C#
//! build decrypt here and the reverse. A value that will not decrypt (a profile
//! copied from another machine or account) reads as an empty password — the
//! user is simply asked again; it is never a reason to refuse to start.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

const ENTROPY: &[u8] = b"QuickByte.Credential.v1";

/// Encrypts `plaintext` for the current user. `None` for an empty value, or if
/// DPAPI is unavailable — the caller then simply persists no password.
pub fn protect(plaintext: &str) -> Option<String> {
    if plaintext.is_empty() {
        return None;
    }
    crate::platform::dpapi::protect(plaintext.as_bytes(), ENTROPY).map(|cipher| STANDARD.encode(cipher))
}

/// Decrypts a value written by [`protect`]; empty on any failure.
pub fn unprotect(protected_value: Option<&str>) -> String {
    let Some(value) = protected_value.filter(|v| !v.is_empty()) else {
        return String::new();
    };
    let Ok(cipher) = STANDARD.decode(value) else {
        return String::new();
    };
    crate::platform::dpapi::unprotect(&cipher, ENTROPY)
        .and_then(|plain| String::from_utf8(plain).ok())
        .unwrap_or_default()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_empty_is_none() {
        let protected = protect("hunter2").expect("DPAPI available");
        assert_ne!(protected, "hunter2");
        assert_eq!(unprotect(Some(&protected)), "hunter2");
        assert_eq!(protect(""), None);
        assert_eq!(unprotect(None), "");
        assert_eq!(unprotect(Some("not base64 !!")), "");
        assert_eq!(unprotect(Some("AAAA")), "");
    }
}
