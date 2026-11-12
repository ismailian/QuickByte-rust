//! `VirusTotalReport` — what the VirusTotal check found for one download,
//! persisted on the item as `VirusTotal` in `downloads.json`.
//!
//! The small enums are written as lowercase words rather than numbers: the
//! 1.26.2 C# build already wrote `"VirusTotal": {"Verdict": "clean"}`, and a
//! profile carried over from it has to read here. A word this build does not
//! know reads as the variant marked `#[serde(other)]`, never as an error that
//! would make the whole download list unreadable.

use serde::{Deserialize, Serialize};

use super::Extra;
use crate::serde_compat::DateTimeOffset;

/// What VirusTotal made of a download, as far as it has got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VirusTotalVerdict {
    /// Being checked.
    Pending,
    /// Security vendors looked at it and none flagged it.
    Clean,
    /// At least one vendor called it suspicious, none malicious.
    Suspicious,
    /// At least one vendor called it malicious.
    Malicious,
    /// Nothing to go on: VirusTotal has never seen it, it could not be uploaded,
    /// or VirusTotal could not be asked. The report's note says which.
    #[default]
    #[serde(other)]
    Unknown,
}

impl VirusTotalVerdict {
    /// Malicious or suspicious: the verdicts that have an action in Options.
    pub fn is_negative(self) -> bool {
        matches!(self, Self::Malicious | Self::Suspicious)
    }
}

/// How far the check has got. Persisted, so a check the app closed in the
/// middle of carries on at the next launch rather than starting over or being
/// forgotten.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VirusTotalStep {
    /// The download's address is being looked up.
    Url,
    /// Waiting for the download to finish, then looking up (or uploading) the file.
    File,
    /// Finished. Also what a report from an older build reads as, so a profile
    /// carried over never sends anything it had not already sent.
    #[default]
    #[serde(other)]
    Done,
}

/// Which lookup the verdict came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VirusTotalSource {
    /// The download's address.
    Url,
    /// The finished file, by its SHA-256 or after an upload.
    #[serde(other)]
    File,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct VirusTotalReport {
    pub verdict: VirusTotalVerdict,
    pub step: VirusTotalStep,
    pub source: Option<VirusTotalSource>,
    /// Vendors that called it malicious.
    pub malicious: i32,
    /// Vendors that called it suspicious.
    pub suspicious: i32,
    /// Vendors that gave an answer at all.
    pub engines: i32,
    /// The finished file's SHA-256, once it has been hashed.
    #[serde(rename = "SHA256")]
    pub sha256: Option<String>,
    /// The report on virustotal.com.
    pub permalink: Option<String>,
    /// Why there is no verdict, or what was not done ("not uploaded: 40 MB").
    pub note: Option<String>,
    /// The user saw a malicious or suspicious verdict and chose to keep the download.
    pub kept_by_user: bool,
    pub checked_at: Option<DateTimeOffset>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl VirusTotalReport {
    /// A check just starting: the address comes first.
    pub fn started() -> Self {
        Self { verdict: VirusTotalVerdict::Pending, step: VirusTotalStep::Url, ..Default::default() }
    }

    /// A negative verdict nobody has acted on yet — the app closed while asking.
    pub fn awaits_decision(&self) -> bool {
        self.verdict.is_negative() && self.step != VirusTotalStep::Done
    }

    /// Still has lookups to make (or an answer to collect).
    pub fn is_unfinished(&self) -> bool {
        self.step != VirusTotalStep::Done
    }

    /// "Flagged as malicious by 5 of 70 security vendors" — the sentence the
    /// prompt, the notification and the download list all use.
    pub fn summary(&self) -> String {
        let of = |count: i32| {
            if self.engines > 0 {
                format!("{count} of {} security vendors", self.engines)
            } else {
                format!("{count} security vendor{}", if count == 1 { "" } else { "s" })
            }
        };
        match self.verdict {
            VirusTotalVerdict::Malicious => format!("Flagged as malicious by {}", of(self.malicious)),
            // Suspicious by suspicious votes alone, or by malicious ones too few
            // to call it malicious: then it is simply "flagged", by all of them.
            VirusTotalVerdict::Suspicious if self.malicious == 0 => format!("Flagged as suspicious by {}", of(self.suspicious)),
            VirusTotalVerdict::Suspicious => format!("Flagged by {}", of(self.malicious + self.suspicious)),
            VirusTotalVerdict::Clean => format!("No security vendor flagged it ({} checked)", self.engines),
            VirusTotalVerdict::Pending => "Being checked".into(),
            VirusTotalVerdict::Unknown => self.note.clone().unwrap_or_else(|| "No result".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_from_the_csharp_build_reads_as_finished() {
        // 1.26.2 wrote only the verdict. Reading it as finished is what keeps an
        // upgrade from sending anything to VirusTotal it had not already sent.
        let report: VirusTotalReport = serde_json::from_str(r#"{"Verdict":"clean"}"#).unwrap();
        assert_eq!(report.verdict, VirusTotalVerdict::Clean);
        assert_eq!(report.step, VirusTotalStep::Done);
        assert!(!report.is_unfinished());
    }

    #[test]
    fn words_this_build_does_not_know_read_as_the_safe_variant() {
        let report: VirusTotalReport =
            serde_json::from_str(r#"{"Verdict":"quarantined","Step":"sandbox","Source":"hash","Later":1}"#).unwrap();
        assert_eq!(report.verdict, VirusTotalVerdict::Unknown);
        assert_eq!(report.step, VirusTotalStep::Done);
        assert_eq!(report.source, Some(VirusTotalSource::File));
        assert_eq!(report.extra.get("Later"), Some(&serde_json::json!(1)));
    }

    #[test]
    fn a_report_round_trips_in_words() {
        let report = VirusTotalReport {
            verdict: VirusTotalVerdict::Malicious,
            step: VirusTotalStep::File,
            source: Some(VirusTotalSource::Url),
            malicious: 3,
            engines: 70,
            ..VirusTotalReport::started()
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(r#""Verdict":"malicious""#), "{json}");
        assert!(json.contains(r#""Step":"file""#), "{json}");
        assert_eq!(serde_json::from_str::<VirusTotalReport>(&json).unwrap(), report);
    }

    #[test]
    fn a_negative_verdict_awaits_a_decision_until_the_check_is_done() {
        let mut report = VirusTotalReport { verdict: VirusTotalVerdict::Suspicious, ..VirusTotalReport::started() };
        assert!(report.awaits_decision());
        report.step = VirusTotalStep::Done;
        assert!(!report.awaits_decision());
    }

    #[test]
    fn the_summary_counts_vendors() {
        let report = VirusTotalReport { verdict: VirusTotalVerdict::Malicious, malicious: 5, engines: 70, ..Default::default() };
        assert_eq!(report.summary(), "Flagged as malicious by 5 of 70 security vendors");
        let report = VirusTotalReport { verdict: VirusTotalVerdict::Suspicious, suspicious: 1, ..Default::default() };
        assert_eq!(report.summary(), "Flagged as suspicious by 1 security vendor");
        // Malicious votes below the threshold: flagged, but not called malicious.
        let report = VirusTotalReport { verdict: VirusTotalVerdict::Suspicious, malicious: 2, suspicious: 1, engines: 70, ..Default::default() };
        assert_eq!(report.summary(), "Flagged by 3 of 70 security vendors");
    }
}
