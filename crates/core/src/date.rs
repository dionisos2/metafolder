//! Dependency-free UTC calendar helpers, shared by the daemon (file
//! timestamps) and the CLI (log/rollback timestamp targets and display).
//!
//! The calendar conversions are Howard Hinnant's `civil_from_days` /
//! `days_from_civil` algorithms (proleptic Gregorian). Everything here is
//! UTC; no time zone handling.

use std::time::{SystemTime, UNIX_EPOCH};

const SECS_PER_DAY: i64 = 86_400;

/// Current time as Unix milliseconds. Times before the epoch clamp to 0.
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

/// Days-since-epoch → (year, month, day) in the proleptic Gregorian calendar.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) → days-since-epoch in the proleptic Gregorian calendar.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Unix milliseconds → (year, month, day, hour, minute, second), UTC.
/// Negative inputs (pre-epoch) are clamped to the epoch.
pub fn ms_to_civil(ms: i64) -> (i64, u32, u32, u64, u64, u64) {
    let secs = ms.div_euclid(1000).max(0) as u64;
    let days = (secs / SECS_PER_DAY as u64) as i64;
    let rem = secs % SECS_PER_DAY as u64;
    let (y, mo, d) = civil_from_days(days);
    (y, mo, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Formats Unix milliseconds as an ISO-8601 UTC datetime
/// (`YYYY-MM-DDTHH:MM:SSZ`). Negative inputs (pre-epoch) clamp to the epoch.
/// Sub-second precision is dropped (the format has no fractional part).
pub fn iso8601_from_ms(ms: i64) -> String {
    let (y, mo, d, h, mi, s) = ms_to_civil(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Formats a [`SystemTime`] as an ISO-8601 UTC datetime
/// (`YYYY-MM-DDTHH:MM:SSZ`). Times before the Unix epoch clamp to the epoch.
pub fn iso8601(t: SystemTime) -> String {
    iso8601_from_ms(ms_from_systemtime(t))
}

/// A [`SystemTime`] as Unix milliseconds. Times before the epoch clamp to 0.
pub fn ms_from_systemtime(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

/// Minimal ISO-8601 → Unix ms (UTC). Accepts `T` or a space as the
/// date/time separator, a trailing `Z`, an omitted time (midnight) or
/// omitted seconds/minutes, and an omitted month and/or day (both default
/// to 1, so `2017` and `2027-01` mean `2017-01-01` and `2027-01-01`).
/// Returns `None` on a malformed string.
pub fn iso_to_ms(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = s.split_once(['T', ' ']).unwrap_or((s, "00:00:00"));
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next().map_or(Ok(1), str::parse).ok()?;
    let day: i64 = d.next().map_or(Ok(1), str::parse).ok()?;
    // Bound the year so `days_from_civil` and the scaling below stay within
    // `i64` (a huge value would otherwise panic in debug / wrap in release);
    // this range covers every realistic file timestamp and date literal.
    if !(0..=999_999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    if !(1..=days_in_month(year, month as u32)).contains(&(day as u32)) {
        return None;
    }
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next().unwrap_or("0").parse().ok()?;
    let sec: i64 = t.next().unwrap_or("0").parse().ok()?;
    // Allow second 60 for a leap second; reject everything else out of range.
    if !(0..=23).contains(&hour) || !(0..=59).contains(&min) || !(0..=60).contains(&sec) {
        return None;
    }
    let days = days_from_civil(year, month as u32, day as u32);
    days.checked_mul(SECS_PER_DAY)?
        .checked_add(hour * 3600 + min * 60 + sec)?
        .checked_mul(1000)
}

/// Whether `year` is a leap year in the proleptic Gregorian calendar.
fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Number of days in `month` (1–12) of `year`; 0 for an invalid month.
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn days_from_civil_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn civil_round_trip_over_a_wide_range() {
        // Every day from 1900 to 2100 must survive a there-and-back.
        let start = days_from_civil(1900, 1, 1);
        let end = days_from_civil(2100, 1, 1);
        for z in start..end {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z, "round trip failed at day {z}");
        }
    }

    #[test]
    fn iso8601_formats_the_epoch() {
        assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn iso8601_formats_a_known_instant() {
        // 2021-01-01T00:00:00Z = 1_609_459_200 s since the epoch.
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_609_459_200);
        assert_eq!(iso8601(t), "2021-01-01T00:00:00Z");
    }

    #[test]
    fn iso8601_handles_leap_years() {
        // 2024-02-29T12:34:56Z (leap year) == 1_709_210_096 s.
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_709_210_096);
        assert_eq!(iso8601(t), "2024-02-29T12:34:56Z");
        // 2000-01-01T00:00:00Z (a centurial leap year) == 946_684_800 s.
        let t = UNIX_EPOCH + std::time::Duration::from_secs(946_684_800);
        assert_eq!(iso8601(t), "2000-01-01T00:00:00Z");
    }

    #[test]
    fn iso8601_clamps_pre_epoch_times() {
        let t = UNIX_EPOCH - std::time::Duration::from_secs(10);
        assert_eq!(iso8601(t), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn iso_to_ms_parses_full_datetime() {
        assert_eq!(iso_to_ms("2021-01-01T00:00:00Z"), Some(1_609_459_200_000));
    }

    #[test]
    fn iso_to_ms_accepts_space_separator_and_partial_time() {
        assert_eq!(iso_to_ms("1970-01-01 00:00"), Some(0));
        assert_eq!(iso_to_ms("1970-01-02"), Some(SECS_PER_DAY * 1000));
    }

    #[test]
    fn iso_to_ms_accepts_year_only_and_year_month() {
        // A bare year means the first instant of that year.
        assert_eq!(iso_to_ms("2017"), iso_to_ms("2017-01-01"));
        // A year-month means the first day of that month.
        assert_eq!(iso_to_ms("2027-01"), iso_to_ms("2027-01-01"));
        assert_eq!(iso_to_ms("2027-03"), iso_to_ms("2027-03-01"));
    }

    #[test]
    fn iso_to_ms_rejects_garbage() {
        assert_eq!(iso_to_ms("not-a-date"), None);
        assert_eq!(iso_to_ms("2021-13"), None);
        assert_eq!(iso_to_ms("2021-00"), None);
        assert_eq!(iso_to_ms("2021-01-32"), None);
        assert_eq!(iso_to_ms("2021-01-00"), None);
    }

    #[test]
    fn iso_to_ms_rejects_days_beyond_the_month() {
        // 31 is in `1..=31` but does not exist in these months.
        assert_eq!(iso_to_ms("2021-02-31"), None);
        assert_eq!(iso_to_ms("2021-02-30"), None);
        assert_eq!(iso_to_ms("2021-04-31"), None);
        assert_eq!(iso_to_ms("2021-06-31"), None);
        // Real last days are still accepted.
        assert!(iso_to_ms("2021-04-30").is_some());
        assert!(iso_to_ms("2021-01-31").is_some());
    }

    #[test]
    fn iso_to_ms_handles_february_leap_rules() {
        assert!(iso_to_ms("2024-02-29").is_some()); // leap year
        assert_eq!(iso_to_ms("2021-02-29"), None); // common year
        assert_eq!(iso_to_ms("2100-02-29"), None); // centurial, not leap
        assert!(iso_to_ms("2000-02-29").is_some()); // divisible by 400
    }

    #[test]
    fn iso_to_ms_rejects_out_of_range_time_components() {
        assert_eq!(iso_to_ms("2021-01-01T24:00:00"), None);
        assert_eq!(iso_to_ms("2021-01-01T00:60:00"), None);
        assert_eq!(iso_to_ms("2021-01-01T00:00:99999999999999"), None);
    }

    #[test]
    fn iso_to_ms_rejects_absurd_year_without_overflow() {
        // Must not panic (debug overflow-checks) or silently wrap (release):
        // an out-of-range year is simply rejected.
        assert_eq!(iso_to_ms("300000000000000-01-01"), None);
        assert_eq!(iso_to_ms("9999999999-01-01"), None);
    }

    #[test]
    fn iso8601_and_iso_to_ms_round_trip() {
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let s = iso8601(t);
        assert_eq!(iso_to_ms(&s), Some(1_700_000_000_000));
    }

    #[test]
    fn iso8601_from_ms_matches_iso8601() {
        // 2024-02-29T12:34:56Z == 1_709_210_096 s.
        assert_eq!(iso8601_from_ms(1_709_210_096_000), "2024-02-29T12:34:56Z");
    }

    #[test]
    fn iso8601_from_ms_clamps_pre_epoch() {
        assert_eq!(iso8601_from_ms(-10_000), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn iso8601_from_ms_and_iso_to_ms_round_trip() {
        // iso_to_ms only carries second precision, so use a whole-second instant.
        let ms = 1_700_000_000_000;
        assert_eq!(iso_to_ms(&iso8601_from_ms(ms)), Some(ms));
    }

    #[test]
    fn ms_from_systemtime_reads_milliseconds() {
        let t = UNIX_EPOCH + std::time::Duration::from_millis(1_709_210_096_123);
        assert_eq!(ms_from_systemtime(t), 1_709_210_096_123);
    }

    #[test]
    fn ms_from_systemtime_clamps_pre_epoch() {
        let t = UNIX_EPOCH - std::time::Duration::from_secs(10);
        assert_eq!(ms_from_systemtime(t), 0);
    }

    #[test]
    fn ms_to_civil_splits_components() {
        // 2021-01-01T01:02:03Z
        let ms = 1_609_459_200_000 + (3600 + 2 * 60 + 3) * 1000;
        assert_eq!(ms_to_civil(ms), (2021, 1, 1, 1, 2, 3));
    }
}
