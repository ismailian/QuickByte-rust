//! Port of `src/QuickByte.Core/Services/Ftp/FtpReply.cs`.
//!
//! One reply from an FTP control connection: the three-digit code and the text
//! that came with it, already reassembled if the server sent it as a multi-line
//! block.
//!
//! The first digit is the whole protocol's error model — 1xx provisional, 2xx
//! done, 3xx more input wanted, 4xx try again later, 5xx refused — which is why
//! [`FtpReply::is_positive`] and friends read the code by range rather than
//! matching the specific numbers each command happens to return.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FtpReply {
    pub code: u16,
    pub text: String,
}

impl FtpReply {
    pub fn new(code: u16, text: impl Into<String>) -> Self {
        Self { code, text: text.into() }
    }

    pub fn is_positive(&self) -> bool {
        (200..400).contains(&self.code)
    }

    /// 1xx: the server has started and will report again when it's done.
    pub fn is_preliminary(&self) -> bool {
        (100..200).contains(&self.code)
    }

    /// 530 (not logged in) and 532 (account needed for storing files).
    pub fn is_authentication_failure(&self) -> bool {
        matches!(self.code, 530 | 532)
    }

    /// Collapses a reply to one line so it can go in an error message.
    pub fn summarize(&self) -> String {
        self.text.replace(['\n', '\r'], " ").trim().to_string()
    }
}

impl std::fmt::Display for FtpReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_positive_covers_2xx_and_3xx() {
        for (code, expected) in [(200, true), (226, true), (350, true), (399, true), (150, false), (400, false), (550, false)] {
            assert_eq!(FtpReply::new(code, "text").is_positive(), expected, "{code}");
        }
    }

    #[test]
    fn is_preliminary_is_how_a_transfer_is_known_to_have_started() {
        for (code, expected) in [(125, true), (150, true), (200, false), (99, false)] {
            assert_eq!(FtpReply::new(code, "text").is_preliminary(), expected, "{code}");
        }
    }

    #[test]
    fn is_authentication_failure_names_the_two_login_refusals() {
        for (code, expected) in [(530, true), (532, true), (531, false), (550, false)] {
            assert_eq!(FtpReply::new(code, "text").is_authentication_failure(), expected, "{code}");
        }
    }

    #[test]
    fn a_reply_prints_as_its_text() {
        assert_eq!(FtpReply::new(213, "213 1048576").to_string(), "213 1048576");
    }
}
