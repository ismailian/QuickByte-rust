//! Human-readable sizes, speeds, percentages and ETAs. Port of `ByteFormatter.cs`.
//! Always formats with a '.' decimal point (the C# code used the current culture;
//! the UI is English-only, so the invariant form is what users saw).

use crate::serde_compat::TimeSpan;

const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
pub const BYTES_PER_KILOBYTE: i64 = 1024;

/// `1536` → `"1.50 KB"`. Negative values read as zero.
pub fn format_bytes(bytes: i64) -> String {
    let mut value = bytes.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

/// `"1.50 MB/s"`.
pub fn format_speed(bytes_per_second: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_second as i64))
}

/// `"42.00%"`, clamped to 0–100.
pub fn format_percentage(percentage: f64) -> String {
    format!("{:.2}%", percentage.clamp(0.0, 100.0))
}

/// `"1h 2m 3s"`, `"2m 3s"`, `"3s"`, or `"--"` when unknown.
pub fn format_eta(eta: Option<TimeSpan>) -> String {
    let Some(t) = eta else { return "--".into() };
    if t.ticks() < 0 || t.ticks() == i64::MAX {
        return "--".into();
    }
    if t.total_hours() >= 1.0 {
        format!("{}h {}m {}s", t.total_hours() as i64, t.minutes(), t.seconds())
    } else if t.total_minutes() >= 1.0 {
        format!("{}m {}s", t.minutes(), t.seconds())
    } else {
        format!("{}s", t.seconds())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes_with_two_decimals() {
        assert_eq!(format_bytes(0), "0.00 B");
        assert_eq!(format_bytes(-5), "0.00 B");
        assert_eq!(format_bytes(1023), "1023.00 B");
        assert_eq!(format_bytes(1536), "1.50 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MB");
        assert_eq!(format_bytes(5 * 1024_i64.pow(4)), "5.00 TB");
        assert_eq!(format_bytes(2048 * 1024_i64.pow(4)), "2048.00 TB");
    }

    #[test]
    fn formats_speed_percentage_and_eta() {
        assert_eq!(format_speed(1536.9), "1.50 KB/s");
        assert_eq!(format_percentage(150.0), "100.00%");
        assert_eq!(format_percentage(-1.0), "0.00%");
        assert_eq!(format_eta(None), "--");
        assert_eq!(format_eta(Some(TimeSpan::from_seconds(5))), "5s");
        assert_eq!(format_eta(Some(TimeSpan::from_seconds(125))), "2m 5s");
        assert_eq!(format_eta(Some(TimeSpan::from_seconds(3723))), "1h 2m 3s");
        assert_eq!(format_eta(Some(TimeSpan::from_seconds(-1))), "--");
    }
}
