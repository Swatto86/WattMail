//! Civil (wall-clock) date arithmetic over the ISO strings the calendar domain
//! speaks, with no dependency and no timezone database.
//!
//! Every function here works purely on the *calendar* — "add a month", "what
//! weekday is this" — and never on instants, because the Rust side has no tz
//! data to turn a wall clock into an instant with. That is deliberate: the
//! CalDAV backend expands recurrences in the event's own wall-clock space and
//! the frontend resolves the zone (see the module docs on [`super`]).
//!
//! Because the strings are fixed-width ISO-8601, **lexicographic order is
//! chronological order** — callers compare them with `<`/`>` directly rather
//! than parsing first. A date-only value sorts before any time on that day,
//! which is also what an agenda wants (all-day events lead the day).
//!
//! The conversions are Howard Hinnant's `days_from_civil` / `civil_from_days`
//! (public domain), valid across the whole proleptic Gregorian calendar.

/// Days since 1970-01-01 for a proleptic Gregorian civil date.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 }); // March-based month
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// The civil date `days` days after 1970-01-01.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

/// The `(year, month, day)` of a wall-clock string, ignoring any time.
pub fn date_parts(wall: &str) -> Option<(i64, u32, u32)> {
    parts(wall).map(|(y, m, d, _, _)| (y, m, d))
}

/// The `THH:MM:SS` suffix of a wall-clock string, or `""` for a date-only value.
/// Re-attaching this is how a generated occurrence keeps its series' time of day.
pub fn time_suffix(wall: &str) -> &str {
    match wall.find('T') {
        Some(i) => &wall[i..],
        None => "",
    }
}

/// Render a day number as `YYYY-MM-DD` with `suffix` appended verbatim.
pub fn render_date(days: i64, suffix: &str) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}{suffix}")
}

/// Seconds from `from` to `to`, treating a date-only value as midnight.
///
/// Used to carry a series' own duration onto each generated occurrence.
pub fn diff_seconds(from: &str, to: &str) -> Option<i64> {
    let (fy, fm, fd, fs, _) = parts(from)?;
    let (ty, tm, td, ts, _) = parts(to)?;
    let days = days_from_civil(ty, tm, td) - days_from_civil(fy, fm, fd);
    Some(days * 86_400 + (ts - fs))
}

/// Day of week for a wall-clock string: 0 = Sunday … 6 = Saturday.
///
/// 1970-01-01 (day 0) was a Thursday, hence the `+ 4`.
pub fn weekday(wall: &str) -> Option<u32> {
    let (y, m, d, _, _) = parts(wall)?;
    Some((days_from_civil(y, m, d) + 4).rem_euclid(7) as u32)
}

/// Whether the value carries a time (`false` for a date-only, all-day value).
pub fn has_time(wall: &str) -> bool {
    wall.len() > 10
}

/// Add whole days, preserving the value's date-only or date-time shape.
pub fn add_days(wall: &str, days: i64) -> Option<String> {
    let (y, m, d, secs, timed) = parts(wall)?;
    Some(render(days_from_civil(y, m, d) + days, secs, timed))
}

/// Add seconds, rolling into following days as needed.
///
/// A date-only value is returned unchanged: an all-day date has no time to
/// shift, and shifting it is exactly the bug that lands events on the wrong day.
pub fn add_seconds(wall: &str, seconds: i64) -> Option<String> {
    let (y, m, d, secs, timed) = parts(wall)?;
    if !timed {
        return Some(wall.to_string());
    }
    let total = secs + seconds;
    let day_shift = total.div_euclid(86_400);
    Some(render(
        days_from_civil(y, m, d) + day_shift,
        total.rem_euclid(86_400),
        true,
    ))
}

/// Add whole months, keeping the day of month.
///
/// Returns `None` when the resulting day does not exist (31 January + 1 month).
/// RFC 5545 §3.3.10 says such an occurrence is **skipped**, not clamped back to
/// the 28th — clamping would invent a meeting on a day the user never chose.
pub fn add_months(wall: &str, months: i64) -> Option<String> {
    let (y, m, d, secs, timed) = parts(wall)?;
    let total = y * 12 + i64::from(m) - 1 + months;
    let (ny, nm) = (total.div_euclid(12), (total.rem_euclid(12) + 1) as u32);
    (d <= days_in_month(ny, nm)).then(|| render(days_from_civil(ny, nm, d), secs, timed))
}

/// Add whole years, keeping month and day. `None` for 29 February in a common
/// year — skipped, for the same reason as [`add_months`].
pub fn add_years(wall: &str, years: i64) -> Option<String> {
    add_months(wall, years.checked_mul(12)?)
}

/// Number of days in a month of the proleptic Gregorian calendar.
pub fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Split `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM:SS` into
/// `(year, month, day, seconds-of-day, carries-a-time)`.
fn parts(wall: &str) -> Option<(i64, u32, u32, i64, bool)> {
    let bytes = wall.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let y: i64 = wall.get(..4)?.parse().ok()?;
    let m: u32 = wall.get(5..7)?.parse().ok()?;
    let d: u32 = wall.get(10 - 2..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }

    let Some(time) = wall.get(10..) else {
        return Some((y, m, d, 0, false));
    };
    if time.is_empty() {
        return Some((y, m, d, 0, false));
    }
    let time = time.strip_prefix('T')?;
    let h: i64 = time.get(..2)?.parse().ok()?;
    let mi: i64 = time.get(3..5)?.parse().ok()?;
    let s: i64 = time.get(6..8).unwrap_or("00").parse().ok()?;
    if h > 23 || mi > 59 || s > 60 {
        return None;
    }
    Some((y, m, d, h * 3_600 + mi * 60 + s, true))
}

/// Render a day number plus seconds-of-day back to the ISO shape it came from.
fn render(days: i64, secs: i64, timed: bool) -> String {
    let (y, m, d) = civil_from_days(days);
    if !timed {
        return format!("{y:04}-{m:02}-{d:02}");
    }
    let (h, mi, s) = (secs / 3_600, (secs / 60) % 60, secs % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_conversions_round_trip_across_era_boundaries() {
        // Known anchors, then an exhaustive round-trip over ~1100 years so a
        // sign or era-division slip cannot hide in an untested corner.
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));

        for day in -365_243..365_243 {
            let (y, m, d) = civil_from_days(day);
            assert_eq!(days_from_civil(y, m, d), day, "round-trip at {day}");
            assert!((1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m));
        }
    }

    #[test]
    fn weekdays_are_correct() {
        // 1970-01-01 was a Thursday; 2026-07-22 is a Wednesday.
        assert_eq!(weekday("1970-01-01"), Some(4));
        assert_eq!(weekday("2026-07-22T14:00:00"), Some(3));
        assert_eq!(weekday("2026-07-26"), Some(0)); // Sunday
        assert_eq!(weekday("not-a-date"), None);
    }

    #[test]
    fn day_and_second_arithmetic_preserves_shape() {
        assert_eq!(add_days("2026-07-22", 10).unwrap(), "2026-08-01");
        assert_eq!(
            add_days("2026-07-22T14:00:00", -22).unwrap(),
            "2026-06-30T14:00:00"
        );
        // Rolling over midnight, forwards and backwards.
        assert_eq!(
            add_seconds("2026-07-22T23:30:00", 3_600).unwrap(),
            "2026-07-23T00:30:00"
        );
        assert_eq!(
            add_seconds("2026-01-01T00:30:00", -3_600).unwrap(),
            "2025-12-31T23:30:00"
        );
        // An all-day date has no time to shift — this is the guard that keeps
        // all-day events off the wrong day.
        assert_eq!(add_seconds("2026-07-22", 86_399).unwrap(), "2026-07-22");
    }

    #[test]
    fn invalid_month_and_year_landings_are_skipped_not_clamped() {
        // RFC 5545: an occurrence that lands on a non-existent date is dropped.
        assert_eq!(add_months("2026-01-31", 1), None); // no 31 February
        assert_eq!(add_months("2026-01-31", 2).unwrap(), "2026-03-31");
        assert_eq!(add_months("2026-08-31", 1), None); // no 31 September
        assert_eq!(add_years("2024-02-29", 1), None); // 2025 is not a leap year
        assert_eq!(add_years("2024-02-29", 4).unwrap(), "2028-02-29");
        // Backwards across a year boundary keeps the time component.
        assert_eq!(
            add_months("2026-03-15T09:00:00", -4).unwrap(),
            "2025-11-15T09:00:00"
        );
    }

    #[test]
    fn lexicographic_order_is_chronological_order() {
        // The property the expander and the agenda both lean on.
        let mut values = vec![
            "2026-07-22T14:00:00".to_string(),
            "2026-07-22".to_string(),
            "2026-07-21T23:59:59".to_string(),
            "2026-12-01T00:00:00".to_string(),
            "2026-07-22T09:00:00".to_string(),
        ];
        values.sort();
        assert_eq!(
            values,
            [
                "2026-07-21T23:59:59",
                "2026-07-22",
                "2026-07-22T09:00:00",
                "2026-07-22T14:00:00",
                "2026-12-01T00:00:00",
            ]
        );
    }

    #[test]
    fn malformed_values_are_rejected() {
        for bad in [
            "",
            "2026-07",
            "2026/07/22",
            "2026-13-01",
            "2026-02-30",
            "2026-07-22T25:00:00",
            "2026-07-22 14:00:00",
        ] {
            assert!(parts(bad).is_none(), "should reject {bad:?}");
            assert!(add_days(bad, 1).is_none(), "should reject {bad:?}");
        }
        assert!(has_time("2026-07-22T00:00:00"));
        assert!(!has_time("2026-07-22"));
    }
}
