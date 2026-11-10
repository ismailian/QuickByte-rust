//! The wire shapes System.Text.Json gave the C# build, reproduced exactly.
//!
//! `settings.json`, `downloads.json` and `queues.json` written by QuickByte 1.26
//! must load in this build, and files this build writes must still load in the
//! C# one (a user can roll back). Two .NET types have no serde equivalent and are
//! the whole reason this module exists:
//!
//! * `TimeSpan` — the constant ("c") format, `[-][d.]hh:mm:ss[.fffffff]`.
//! * `DateTimeOffset` — ISO 8601 with up to seven fractional digits and an
//!   explicit offset; the offset is part of the value and is written back as read.
//!
//! Everything else (`Guid`, enums as numbers, PascalCase names) is handled by
//! attributes on the models.

use std::fmt;
use std::ops::{Add, Sub};
use std::str::FromStr;

use chrono::{DateTime, FixedOffset, Local, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// .NET ticks: 100 ns.
pub const TICKS_PER_SECOND: i64 = 10_000_000;
pub const TICKS_PER_MILLISECOND: i64 = 10_000;

/// A .NET `TimeSpan`: a signed count of 100 ns ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimeSpan(pub i64);

impl TimeSpan {
    pub const ZERO: TimeSpan = TimeSpan(0);

    pub const fn from_ticks(ticks: i64) -> Self {
        TimeSpan(ticks)
    }
    pub const fn from_seconds(seconds: i64) -> Self {
        TimeSpan(seconds * TICKS_PER_SECOND)
    }
    pub const fn from_minutes(minutes: i64) -> Self {
        TimeSpan(minutes * 60 * TICKS_PER_SECOND)
    }
    pub const fn from_hours(hours: i64) -> Self {
        TimeSpan(hours * 3600 * TICKS_PER_SECOND)
    }
    pub fn from_secs_f64(seconds: f64) -> Self {
        TimeSpan((seconds * TICKS_PER_SECOND as f64).round() as i64)
    }
    pub fn from_duration(d: std::time::Duration) -> Self {
        TimeSpan((d.as_nanos() / 100).min(i64::MAX as u128) as i64)
    }
    pub const fn ticks(self) -> i64 {
        self.0
    }
    pub fn total_seconds(self) -> f64 {
        self.0 as f64 / TICKS_PER_SECOND as f64
    }
    pub fn total_minutes(self) -> f64 {
        self.total_seconds() / 60.0
    }
    pub fn total_hours(self) -> f64 {
        self.total_seconds() / 3600.0
    }
    pub fn hours(self) -> i64 {
        (self.0 / (3600 * TICKS_PER_SECOND)) % 24
    }
    pub fn minutes(self) -> i64 {
        (self.0 / (60 * TICKS_PER_SECOND)) % 60
    }
    pub fn seconds(self) -> i64 {
        (self.0 / TICKS_PER_SECOND) % 60
    }
    pub fn days(self) -> i64 {
        self.0 / (86_400 * TICKS_PER_SECOND)
    }
    /// A `chrono::TimeDelta` for date arithmetic.
    pub fn to_delta(self) -> chrono::TimeDelta {
        chrono::TimeDelta::microseconds(self.0 / 10) + chrono::TimeDelta::nanoseconds((self.0 % 10) * 100)
    }
    pub fn from_delta(d: chrono::TimeDelta) -> Self {
        TimeSpan(d.num_microseconds().map(|us| us * 10).unwrap_or(i64::MAX))
    }
    pub fn to_std(self) -> std::time::Duration {
        std::time::Duration::from_nanos((self.0.max(0) as u64).saturating_mul(100))
    }
}

impl Add for TimeSpan {
    type Output = TimeSpan;
    fn add(self, rhs: Self) -> Self {
        TimeSpan(self.0.saturating_add(rhs.0))
    }
}
impl Sub for TimeSpan {
    type Output = TimeSpan;
    fn sub(self, rhs: Self) -> Self {
        TimeSpan(self.0.saturating_sub(rhs.0))
    }
}

impl fmt::Display for TimeSpan {
    /// The "c" format: `[-][d.]hh:mm:ss[.fffffff]`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let negative = self.0 < 0;
        let ticks = self.0.unsigned_abs();
        let per_sec = TICKS_PER_SECOND as u64;
        let total_secs = ticks / per_sec;
        let frac = ticks % per_sec;
        let days = total_secs / 86_400;
        let h = (total_secs / 3600) % 24;
        let m = (total_secs / 60) % 60;
        let s = total_secs % 60;
        if negative {
            f.write_str("-")?;
        }
        if days > 0 {
            write!(f, "{days}.")?;
        }
        write!(f, "{h:02}:{m:02}:{s:02}")?;
        if frac > 0 {
            write!(f, ".{frac:07}")?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("not a TimeSpan: {0:?}")]
pub struct TimeSpanParseError(String);

impl FromStr for TimeSpan {
    type Err = TimeSpanParseError;

    /// Accepts the "c" format plus the forms `TimeSpan.Parse` takes for the same
    /// values: `d.hh:mm:ss`, `hh:mm`, `hh:mm:ss.f…`, and a bare day count.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let err = || TimeSpanParseError(input.to_string());
        let s = input.trim();
        let (negative, s) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        if s.is_empty() {
            return Err(err());
        }
        let parse_u = |p: &str| p.parse::<u64>().map_err(|_| err());

        // Split off "d." when the part before the first ':' contains a '.'.
        let (days, clock) = match s.find(':') {
            None => return Ok(TimeSpan(sign(negative) * (parse_u(s)? as i64) * 86_400 * TICKS_PER_SECOND)),
            Some(colon) => match s[..colon].find('.') {
                Some(dot) => (parse_u(&s[..dot])?, &s[dot + 1..]),
                None => (0, s),
            },
        };
        let parts: Vec<&str> = clock.split(':').collect();
        let (h, m, sec_part) = match parts.as_slice() {
            [h, m] => (parse_u(h)?, parse_u(m)?, "0"),
            [h, m, s] => (parse_u(h)?, parse_u(m)?, *s),
            _ => return Err(err()),
        };
        let (secs, frac_ticks) = match sec_part.split_once('.') {
            Some((whole, frac)) => {
                if frac.is_empty() || frac.len() > 7 || !frac.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(err());
                }
                let padded = format!("{frac:0<7}");
                (parse_u(whole)?, parse_u(&padded)?)
            }
            None => (parse_u(sec_part)?, 0),
        };
        if h > 23 || m > 59 || secs > 59 {
            return Err(err());
        }
        let total = ((days * 86_400 + h * 3600 + m * 60 + secs) * TICKS_PER_SECOND as u64 + frac_ticks) as i64;
        Ok(TimeSpan(sign(negative) * total))
    }
}

fn sign(negative: bool) -> i64 {
    if negative { -1 } else { 1 }
}

impl Serialize for TimeSpan {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TimeSpan {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A .NET `DateTimeOffset`: an instant plus the offset it was recorded in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DateTimeOffset(pub DateTime<FixedOffset>);

impl DateTimeOffset {
    /// `DateTimeOffset.Now`: local time with the local offset.
    pub fn now() -> Self {
        DateTimeOffset(Local::now().fixed_offset())
    }
    /// `DateTimeOffset.UtcNow`.
    pub fn utc_now() -> Self {
        DateTimeOffset(Utc::now().fixed_offset())
    }
    /// `.LocalDateTime`: the same instant in this machine's zone, without an offset.
    pub fn local_naive(&self) -> chrono::NaiveDateTime {
        self.0.with_timezone(&Local).naive_local()
    }
    pub fn to_utc(&self) -> DateTime<Utc> {
        self.0.with_timezone(&Utc)
    }
    pub fn from_utc(dt: DateTime<Utc>) -> Self {
        DateTimeOffset(dt.fixed_offset())
    }
}

impl fmt::Display for DateTimeOffset {
    /// `yyyy-MM-ddTHH:mm:ss.fffffff+hh:mm`, trailing fractional zeros trimmed the
    /// way System.Text.Json trims them.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dt = &self.0;
        write!(f, "{}", dt.format("%Y-%m-%dT%H:%M:%S"))?;
        let ticks = dt.timestamp_subsec_nanos() / 100;
        if ticks > 0 {
            let frac = format!("{ticks:07}");
            write!(f, ".{}", frac.trim_end_matches('0'))?;
        }
        write!(f, "{}", dt.format("%:z"))
    }
}

impl FromStr for DateTimeOffset {
    type Err = chrono::ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        // RFC 3339 covers every form System.Text.Json writes, "Z" included.
        DateTime::parse_from_rfc3339(s).map(DateTimeOffset).or_else(|e| {
            // A value with no offset at all is local time, as DateTimeOffset.Parse reads it.
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .and_then(|naive| naive.and_local_timezone(Local).single())
                .map(|local| DateTimeOffset(local.fixed_offset()))
                .ok_or(e)
        })
    }
}

impl Serialize for DateTimeOffset {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DateTimeOffset {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A .NET `DateTime` as the pipe messages carry it (`QueueSummary.NextRun`):
/// local wall-clock time with no offset, `yyyy-MM-ddTHH:mm:ss[.fffffff]`.
pub mod naive_local {
    use chrono::NaiveDateTime;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<NaiveDateTime>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(dt) => serializer.collect_str(&dt.format("%Y-%m-%dT%H:%M:%S")),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<NaiveDateTime>, D::Error> {
        let raw = Option::<String>::deserialize(deserializer)?;
        match raw {
            None => Ok(None),
            Some(s) => {
                let trimmed = s.trim_end_matches('Z');
                let trimmed = trimmed.split('+').next().unwrap_or(trimmed);
                NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.f")
                    .map(Some)
                    .map_err(serde::de::Error::custom)
            }
        }
    }
}

/// Writes a `double` the way System.Text.Json does: a whole number has no
/// fractional part (`100`, not `100.0`). Read either way.
pub fn dotnet_double<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.0e15 {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64(*value)
    }
}

/// Strips a UTF-8 byte-order mark: .NET tolerated one on read, serde_json does not.
pub fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

/// `serde_json::to_vec_pretty` — two-space indentation, as `WriteIndented = true` produced.
pub fn to_pretty_json<T: Serialize>(value: &T) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec_pretty(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timespan_round_trips_the_constant_format() {
        for text in ["02:00:00", "00:00:00", "07:30:15", "1.02:03:04", "00:00:01.5000000", "-00:05:00", "3.00:00:00.0000001"] {
            let span: TimeSpan = text.parse().unwrap();
            assert_eq!(span.to_string(), text, "{text}");
        }
        assert_eq!("02:00:00".parse::<TimeSpan>().unwrap(), TimeSpan::from_hours(2));
        assert_eq!("22:00".parse::<TimeSpan>().unwrap(), TimeSpan::from_hours(22));
        assert!("25:00:00".parse::<TimeSpan>().is_err());
        assert!("abc".parse::<TimeSpan>().is_err());
    }

    #[test]
    fn timespan_fraction_is_written_with_seven_digits() {
        assert_eq!(TimeSpan::from_secs_f64(1.25).to_string(), "00:00:01.2500000");
    }

    #[test]
    fn datetimeoffset_round_trips_what_system_text_json_wrote() {
        for text in [
            "2026-11-09T10:08:25.1128086+00:00",
            "2026-11-07T09:54:45.9376369-08:00",
            "2026-07-30T23:50:02+00:00",
        ] {
            let parsed: DateTimeOffset = text.parse().unwrap();
            assert_eq!(parsed.to_string(), text);
        }
        let z: DateTimeOffset = "2026-08-21T09:00:00Z".parse().unwrap();
        assert_eq!(z.to_string(), "2026-08-21T09:00:00+00:00");
    }

    #[test]
    fn bom_is_stripped() {
        assert_eq!(strip_bom(b"\xEF\xBB\xBF{}"), b"{}");
        assert_eq!(strip_bom(b"{}"), b"{}");
    }
}
