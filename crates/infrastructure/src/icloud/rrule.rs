//! RFC 5545 `RRULE` expansion, in the event's own wall-clock space.
//!
//! iCloud cannot be relied on to expand recurring series server-side — its
//! `<C:expand>` is documented as returning different results for identical
//! requests (Apple Developer Forums thread 94363) — so WattMail fetches the
//! master `VEVENT` and expands here, which is also what DAVx5 does.
//!
//! Everything is **civil arithmetic on wall-clock strings**: "add a month",
//! "the second Thursday". No instant is ever computed, because that would need a
//! timezone database the Rust tree deliberately does not carry. The caller
//! widens the window to absorb the resulting offset uncertainty and the frontend
//! — which has the browser's IANA data — does the exact filtering.
//!
//! Supported: `FREQ` (DAILY/WEEKLY/MONTHLY/YEARLY), `INTERVAL`, `COUNT`,
//! `UNTIL`, `BYDAY` (plain and ordinal), `BYMONTHDAY`, `BYMONTH`.
//! Not supported: `BYSETPOS`, non-Monday `WKST`, sub-daily frequencies — a rule
//! using any of those is reported [`RRule::unsupported`] so the caller can show
//! the series' own start rather than inventing wrong dates.

use super::civil;

/// Hard caps so a pathological or hostile rule can never spin. A personal
/// calendar never approaches either: 1000 occurrences is ~3 years of daily.
const MAX_OCCURRENCES: usize = 1_000;
const MAX_PERIODS: i64 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// A `BYDAY` entry: a weekday, optionally with an ordinal (`2TH`, `-1FR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByDay {
    pub ordinal: Option<i32>,
    /// 0 = Sunday … 6 = Saturday, matching [`civil::weekday`].
    pub weekday: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RRule {
    pub freq: Freq,
    pub interval: i64,
    pub count: Option<usize>,
    /// `UNTIL` as a wall-clock string, compared lexicographically.
    pub until: Option<String>,
    pub by_day: Vec<ByDay>,
    /// Days of the month; negative counts back from the end (`-1` = last).
    pub by_month_day: Vec<i32>,
    pub by_month: Vec<u32>,
    /// True when the rule uses a part this expander does not implement, so
    /// expanding it would produce confidently wrong dates.
    pub unsupported: bool,
}

impl RRule {
    /// Parse an `RRULE` property value. `None` when there is no usable `FREQ`.
    pub fn parse(value: &str) -> Option<Self> {
        let mut rule = Self {
            freq: Freq::Daily,
            interval: 1,
            count: None,
            until: None,
            by_day: Vec::new(),
            by_month_day: Vec::new(),
            by_month: Vec::new(),
            unsupported: false,
        };
        let mut seen_freq = false;

        for part in value.split(';') {
            let Some((key, raw)) = part.split_once('=') else {
                continue;
            };
            let raw = raw.trim();
            match key.trim().to_ascii_uppercase().as_str() {
                "FREQ" => {
                    seen_freq = true;
                    match raw.to_ascii_uppercase().as_str() {
                        "DAILY" => rule.freq = Freq::Daily,
                        "WEEKLY" => rule.freq = Freq::Weekly,
                        "MONTHLY" => rule.freq = Freq::Monthly,
                        "YEARLY" => rule.freq = Freq::Yearly,
                        // SECONDLY/MINUTELY/HOURLY: no personal calendar tool
                        // authors these, and expanding them here would be pure
                        // speculation.
                        _ => rule.unsupported = true,
                    }
                }
                // Clamped rather than merely floored: the interval multiplies the
                // period offset, so an absurd value from a hostile feed would
                // overflow the day arithmetic downstream.
                "INTERVAL" => rule.interval = raw.parse().unwrap_or(1).clamp(1, 100_000),
                "COUNT" => rule.count = raw.parse().ok(),
                "UNTIL" => {
                    // UNTIL is defined in UTC while DTSTART may be zoned. Without
                    // a tz database the two are compared by digits, so a series
                    // can end at most one occurrence early or late when the zone
                    // offset is large.
                    // ponytail: digit comparison; exact only once something in
                    // the pipeline can convert an instant between zones.
                    rule.until = super::ical::IcalDateTime::parse(raw, None).map(|dt| dt.wall);
                }
                "BYDAY" => {
                    rule.by_day = raw.split(',').filter_map(parse_by_day).collect();
                }
                "BYMONTHDAY" => {
                    rule.by_month_day = raw
                        .split(',')
                        .filter_map(|d| d.trim().parse().ok())
                        .collect()
                }
                "BYMONTH" => {
                    rule.by_month = raw
                        .split(',')
                        .filter_map(|m| m.trim().parse().ok())
                        .filter(|m| (1..=12).contains(m))
                        .collect()
                }
                "WKST" => {
                    // Every mainstream client, iCloud included, uses Monday. A
                    // different week start only changes WEEKLY+INTERVAL>1 results.
                    if !raw.eq_ignore_ascii_case("MO") {
                        rule.unsupported = true;
                    }
                }
                // Needs full per-period candidate generation then positional
                // selection; iCloud's own UI expresses "last Friday" as
                // BYDAY=-1FR instead, so this is genuinely unused here.
                "BYSETPOS" | "BYWEEKNO" | "BYYEARDAY" => rule.unsupported = true,
                _ => {}
            }
        }

        seen_freq.then_some(rule)
    }
}

/// Parse one `BYDAY` token (`MO`, `2TH`, `-1FR`).
fn parse_by_day(token: &str) -> Option<ByDay> {
    let token = token.trim();
    let split = token.len().checked_sub(2)?;
    let (prefix, day) = token.split_at(split);
    let weekday = match day.to_ascii_uppercase().as_str() {
        "SU" => 0,
        "MO" => 1,
        "TU" => 2,
        "WE" => 3,
        "TH" => 4,
        "FR" => 5,
        "SA" => 6,
        _ => return None,
    };
    let ordinal = match prefix.trim() {
        "" => None,
        n => Some(n.parse().ok()?),
    };
    Some(ByDay { ordinal, weekday })
}

/// Expand `dtstart` into occurrence start wall-clocks overlapping
/// `[window_start, window_end)`.
///
/// Bounds are wall-clock strings compared lexicographically — which is
/// chronological for fixed-width ISO values. The returned list is sorted,
/// deduplicated, and always includes `dtstart` itself when it falls in range.
pub fn expand(dtstart: &str, rule: &RRule, window_start: &str, window_end: &str) -> Vec<String> {
    if rule.unsupported {
        // Least-wrong degradation: the series' own start is a real date the user
        // chose, where a partially-applied rule would invent dates they didn't.
        return if in_window(dtstart, window_start, window_end) {
            vec![dtstart.to_string()]
        } else {
            Vec::new()
        };
    }

    let Some((year, month, day)) = civil::date_parts(dtstart) else {
        return Vec::new();
    };
    let suffix = civil::time_suffix(dtstart).to_string();
    let start_day = civil::days_from_civil(year, month, day);

    // With COUNT the series length is defined from its own beginning, so every
    // occurrence must be generated in order. Without it, skip straight to the
    // window — a daily event begun years ago would otherwise burn its whole
    // period budget getting there.
    let first_period = match rule.count {
        Some(_) => 0,
        None => first_period_at_or_before(rule, dtstart, window_start, start_day),
    };

    let mut out: Vec<String> = Vec::new();
    let mut emitted = 0usize;

    for period in first_period..first_period.saturating_add(MAX_PERIODS) {
        let offset = period * rule.interval;
        let mut candidates = match rule.freq {
            Freq::Daily => vec![start_day + offset],
            Freq::Weekly => weekly_candidates(start_day, offset, &rule.by_day),
            Freq::Monthly => monthly_candidates(year, month, day, offset, rule),
            Freq::Yearly => yearly_candidates(year, month, day, offset, rule),
        };
        candidates.sort_unstable();
        candidates.dedup();

        // A period whose every candidate already sits past the window ends the
        // walk — but only once COUNT can no longer be the binding limit, and
        // only when the period produced candidates at all: an empty period is
        // routine (February for a "31st monthly" rule) and must not stop it.
        let mut all_past = true;
        let mut had_candidates = false;

        for candidate_day in candidates {
            had_candidates = true;
            let wall = civil::render_date(candidate_day, &suffix);
            // Occurrences never precede DTSTART, whatever the BY* parts imply.
            if wall.as_str() < dtstart {
                all_past = false;
                continue;
            }
            if let Some(until) = &rule.until {
                if wall.as_str() > until.as_str() {
                    return finish(out);
                }
            }
            if let Some(count) = rule.count {
                if emitted >= count {
                    return finish(out);
                }
            }
            emitted += 1;
            if wall.as_str() < window_end {
                all_past = false;
            }
            if in_window(&wall, window_start, window_end) {
                out.push(wall);
                if out.len() >= MAX_OCCURRENCES {
                    return finish(out);
                }
            }
        }

        if had_candidates && all_past && rule.count.is_none() {
            break;
        }
    }

    finish(out)
}

fn finish(mut out: Vec<String>) -> Vec<String> {
    out.sort();
    out.dedup();
    out
}

/// Half-open containment: `[window_start, window_end)`.
fn in_window(wall: &str, window_start: &str, window_end: &str) -> bool {
    wall >= window_start && wall < window_end
}

/// The first period index that can still land at or after the window start.
fn first_period_at_or_before(
    rule: &RRule,
    dtstart: &str,
    window_start: &str,
    start_day: i64,
) -> i64 {
    let Some((wy, wm, wd)) = civil::date_parts(window_start) else {
        return 0;
    };
    if window_start <= dtstart {
        return 0;
    }
    let periods = match rule.freq {
        Freq::Daily => (civil::days_from_civil(wy, wm, wd) - start_day) / rule.interval,
        Freq::Weekly => (civil::days_from_civil(wy, wm, wd) - start_day) / (7 * rule.interval),
        Freq::Monthly => {
            let (sy, sm, _) = civil::date_parts(dtstart).unwrap_or((wy, wm, wd));
            ((wy * 12 + i64::from(wm)) - (sy * 12 + i64::from(sm))) / rule.interval
        }
        Freq::Yearly => {
            let (sy, _, _) = civil::date_parts(dtstart).unwrap_or((wy, wm, wd));
            (wy - sy) / rule.interval
        }
    };
    // Step back one period so an occurrence straddling the boundary is kept.
    (periods - 1).max(0)
}

/// Weekly candidates: every `BYDAY` weekday inside the period's week (weeks
/// start Monday — `WKST` other than MO is rejected at parse time).
fn weekly_candidates(start_day: i64, offset_weeks: i64, by_day: &[ByDay]) -> Vec<i64> {
    let base = start_day + offset_weeks * 7;
    if by_day.is_empty() {
        return vec![base];
    }
    // Monday of the base's week. `weekday` is 0=Sunday, so shift by 6 first.
    let base_weekday = (base + 4).rem_euclid(7); // 0 = Sunday
    let monday = base - (base_weekday + 6).rem_euclid(7);
    by_day
        .iter()
        .map(|d| monday + i64::from((d.weekday + 6) % 7))
        .collect()
}

/// Monthly candidates for the period `offset_months` after the series start.
fn monthly_candidates(
    year: i64,
    month: u32,
    day: u32,
    offset_months: i64,
    rule: &RRule,
) -> Vec<i64> {
    let total = year * 12 + i64::from(month) - 1 + offset_months;
    let (y, m) = (total.div_euclid(12), (total.rem_euclid(12) + 1) as u32);
    days_in(y, m, day, rule)
}

/// Yearly candidates: `BYMONTH` selects the months, defaulting to the series'
/// own month.
fn yearly_candidates(year: i64, month: u32, day: u32, offset_years: i64, rule: &RRule) -> Vec<i64> {
    let y = year + offset_years;
    let months: Vec<u32> = if rule.by_month.is_empty() {
        vec![month]
    } else {
        rule.by_month.clone()
    };
    months
        .into_iter()
        .flat_map(|m| days_in(y, m, day, rule))
        .collect()
}

/// The candidate days within one `(year, month)`, per the rule's BY* parts.
///
/// With no BY* part the anchor is the series' own day of month, and a month too
/// short for it yields **nothing** — RFC 5545 §3.3.10 skips an invalid date
/// rather than clamping it, so "the 31st monthly" simply has no February.
fn days_in(y: i64, m: u32, anchor_day: u32, rule: &RRule) -> Vec<i64> {
    let last = civil::days_in_month(y, m);
    if last == 0 {
        return Vec::new();
    }
    let mut days: Vec<u32> = Vec::new();

    for entry in &rule.by_day {
        match entry.ordinal {
            Some(n) => days.extend(nth_weekday(y, m, entry.weekday, n)),
            // A plain weekday under MONTHLY/YEARLY means every such weekday.
            None => days.extend(all_weekdays(y, m, entry.weekday)),
        }
    }
    for &raw in &rule.by_month_day {
        let resolved = if raw > 0 {
            raw
        } else if raw < 0 {
            last as i32 + raw + 1
        } else {
            continue;
        };
        if (1..=last as i32).contains(&resolved) {
            days.push(resolved as u32);
        }
    }
    if days.is_empty() {
        if anchor_day > last {
            return Vec::new();
        }
        days.push(anchor_day);
    }

    days.into_iter()
        .filter(|&d| d >= 1 && d <= last)
        .map(|d| civil::days_from_civil(y, m, d))
        .collect()
}

/// Day of month of the `n`-th `weekday` in `(y, m)`; negative `n` counts back.
fn nth_weekday(y: i64, m: u32, weekday: u32, n: i32) -> Option<u32> {
    if n == 0 {
        return None;
    }
    let last = civil::days_in_month(y, m);
    let first_weekday = (civil::days_from_civil(y, m, 1) + 4).rem_euclid(7) as u32;
    let first = 1 + (weekday + 7 - first_weekday) % 7;
    let occurrences = (last - first) / 7 + 1;

    let index = if n > 0 { n - 1 } else { occurrences as i32 + n };
    if index < 0 || index >= occurrences as i32 {
        return None;
    }
    Some(first + (index as u32) * 7)
}

/// Every day of month in `(y, m)` falling on `weekday`.
fn all_weekdays(y: i64, m: u32, weekday: u32) -> Vec<u32> {
    let last = civil::days_in_month(y, m);
    let first_weekday = (civil::days_from_civil(y, m, 1) + 4).rem_euclid(7) as u32;
    let first = 1 + (weekday + 7 - first_weekday) % 7;
    (0..)
        .map(|i| first + i * 7)
        .take_while(|&d| d <= last)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dtstart: &str, rrule: &str, from: &str, to: &str) -> Vec<String> {
        let rule = RRule::parse(rrule).expect("rule parses");
        expand(dtstart, &rule, from, to)
    }

    #[test]
    fn weekly_by_day_hits_every_listed_weekday() {
        // 2026-07-22 is a Wednesday.
        let got = run(
            "2026-07-22T09:00:00",
            "FREQ=WEEKLY;BYDAY=MO,WE,FR",
            "2026-07-20T00:00:00",
            "2026-08-03T00:00:00",
        );
        assert_eq!(
            got,
            [
                // Monday the 20th is in the first week but precedes DTSTART.
                "2026-07-22T09:00:00", // Wed
                "2026-07-24T09:00:00", // Fri
                "2026-07-27T09:00:00", // Mon
                "2026-07-29T09:00:00", // Wed
                "2026-07-31T09:00:00", // Fri
                                       // Monday 3 August is excluded: the window end is exclusive.
            ]
        );
    }

    #[test]
    fn count_limits_the_series_regardless_of_window_width() {
        let got = run(
            "2026-07-22T09:00:00",
            "FREQ=DAILY;COUNT=3",
            "2026-01-01T00:00:00",
            "2027-01-01T00:00:00",
        );
        assert_eq!(
            got,
            [
                "2026-07-22T09:00:00",
                "2026-07-23T09:00:00",
                "2026-07-24T09:00:00"
            ]
        );
    }

    #[test]
    fn until_stops_the_series() {
        let got = run(
            "2026-07-22T09:00:00",
            "FREQ=DAILY;UNTIL=20260725T090000Z",
            "2026-07-01T00:00:00",
            "2026-08-01T00:00:00",
        );
        assert_eq!(got.len(), 4, "22nd through 25th inclusive: {got:?}");
        assert_eq!(got.last().unwrap(), "2026-07-25T09:00:00");
    }

    #[test]
    fn monthly_on_the_31st_skips_short_months_rather_than_clamping() {
        // The highest-risk case: clamping would invent meetings on the 28th/30th.
        let got = run(
            "2026-01-31T10:00:00",
            "FREQ=MONTHLY",
            "2026-01-01T00:00:00",
            "2026-06-01T00:00:00",
        );
        assert_eq!(
            got,
            [
                "2026-01-31T10:00:00",
                "2026-03-31T10:00:00",
                "2026-05-31T10:00:00"
            ],
            "February and April have no 31st"
        );
    }

    #[test]
    fn yearly_on_leap_day_only_occurs_in_leap_years() {
        let got = run(
            "2024-02-29T10:00:00",
            "FREQ=YEARLY",
            "2024-01-01T00:00:00",
            "2029-01-01T00:00:00",
        );
        assert_eq!(got, ["2024-02-29T10:00:00", "2028-02-29T10:00:00"]);
    }

    #[test]
    fn ordinal_by_day_resolves_nth_and_last_weekday() {
        // Second Thursday of each month.
        let got = run(
            "2026-07-09T18:00:00",
            "FREQ=MONTHLY;BYDAY=2TH",
            "2026-07-01T00:00:00",
            "2026-10-01T00:00:00",
        );
        assert_eq!(
            got,
            [
                "2026-07-09T18:00:00",
                "2026-08-13T18:00:00",
                "2026-09-10T18:00:00"
            ]
        );

        // Last Friday of each month.
        let got = run(
            "2026-07-31T17:00:00",
            "FREQ=MONTHLY;BYDAY=-1FR",
            "2026-07-01T00:00:00",
            "2026-10-01T00:00:00",
        );
        assert_eq!(
            got,
            [
                "2026-07-31T17:00:00",
                "2026-08-28T17:00:00",
                "2026-09-25T17:00:00"
            ]
        );
    }

    #[test]
    fn interval_and_by_month_day_and_negative_month_day() {
        let fortnightly = run(
            "2026-07-22T09:00:00",
            "FREQ=WEEKLY;INTERVAL=2",
            "2026-07-01T00:00:00",
            "2026-09-01T00:00:00",
        );
        assert_eq!(
            fortnightly,
            [
                "2026-07-22T09:00:00",
                "2026-08-05T09:00:00",
                "2026-08-19T09:00:00"
            ]
        );

        let bills = run(
            "2026-07-01T08:00:00",
            "FREQ=MONTHLY;BYMONTHDAY=1,-1",
            "2026-07-01T00:00:00",
            "2026-09-01T00:00:00",
        );
        assert_eq!(
            bills,
            [
                "2026-07-01T08:00:00",
                "2026-07-31T08:00:00",
                "2026-08-01T08:00:00",
                "2026-08-31T08:00:00"
            ]
        );
    }

    #[test]
    fn a_long_running_daily_series_reaches_a_distant_window_cheaply() {
        // Started in 2015, viewed in 2026: without the fast-forward this would
        // exhaust the period budget before reaching the window and show nothing.
        let got = run(
            "2015-01-01T07:00:00",
            "FREQ=DAILY",
            "2026-07-20T00:00:00",
            "2026-07-23T00:00:00",
        );
        assert_eq!(
            got,
            [
                "2026-07-20T07:00:00",
                "2026-07-21T07:00:00",
                "2026-07-22T07:00:00"
            ]
        );
    }

    #[test]
    fn a_long_count_bounded_series_still_reaches_a_present_day_window() {
        // COUNT is defined from the series' own beginning, so these occurrences
        // have to be counted from 2015 to know which ones land today. Capping
        // the *count* rather than the *output* used to return nothing at all.
        let got = run(
            "2015-01-01T07:00:00",
            "FREQ=DAILY;COUNT=4500",
            "2026-07-20T00:00:00",
            "2026-07-23T00:00:00",
        );
        assert_eq!(
            got,
            [
                "2026-07-20T07:00:00",
                "2026-07-21T07:00:00",
                "2026-07-22T07:00:00"
            ]
        );
    }

    #[test]
    fn an_absurd_interval_is_clamped_rather_than_overflowing() {
        let rule = RRule::parse("FREQ=DAILY;INTERVAL=99999999999999").unwrap();
        assert_eq!(rule.interval, 100_000);
        // Still terminates, and produces nothing silly.
        let got = expand(
            "2026-07-22T09:00:00",
            &rule,
            "2026-07-01T00:00:00",
            "2026-08-01T00:00:00",
        );
        assert_eq!(got, ["2026-07-22T09:00:00"]);
    }

    #[test]
    fn all_day_series_keep_their_date_only_shape() {
        let got = run("2026-07-22", "FREQ=WEEKLY", "2026-07-01", "2026-08-12");
        assert_eq!(got, ["2026-07-22", "2026-07-29", "2026-08-05"]);
    }

    #[test]
    fn unsupported_parts_degrade_to_the_series_start_not_wrong_dates() {
        let rule = RRule::parse("FREQ=MONTHLY;BYDAY=FR;BYSETPOS=-1").unwrap();
        assert!(rule.unsupported);
        let got = expand(
            "2026-07-31T17:00:00",
            &rule,
            "2026-07-01T00:00:00",
            "2026-10-01T00:00:00",
        );
        assert_eq!(got, ["2026-07-31T17:00:00"], "master occurrence only");

        // …and nothing at all when even that start is outside the window.
        let got = expand(
            "2026-07-31T17:00:00",
            &rule,
            "2026-09-01T00:00:00",
            "2026-10-01T00:00:00",
        );
        assert!(got.is_empty());

        assert!(RRule::parse("FREQ=HOURLY;INTERVAL=2").unwrap().unsupported);
        assert!(RRule::parse("FREQ=WEEKLY;WKST=SU").unwrap().unsupported);
        assert!(!RRule::parse("FREQ=WEEKLY;WKST=MO").unwrap().unsupported);
        assert!(RRule::parse("INTERVAL=2").is_none(), "no FREQ, no rule");
    }

    #[test]
    fn by_day_tokens_parse_including_ordinals() {
        assert_eq!(
            parse_by_day("MO"),
            Some(ByDay {
                ordinal: None,
                weekday: 1
            })
        );
        assert_eq!(
            parse_by_day("2TH"),
            Some(ByDay {
                ordinal: Some(2),
                weekday: 4
            })
        );
        assert_eq!(
            parse_by_day("-1FR"),
            Some(ByDay {
                ordinal: Some(-1),
                weekday: 5
            })
        );
        assert_eq!(parse_by_day("XX"), None);
        assert_eq!(parse_by_day(""), None);
    }
}
