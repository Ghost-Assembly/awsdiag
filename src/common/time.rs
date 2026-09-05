//! Time-window parsing for the `--since` / `--until` flags.
//!
//! Accepts four forms, resolved against a caller-supplied `now` so the
//! parser stays deterministic and testable:
//!
//! | Form            | Example                  | Meaning                          |
//! |-----------------|--------------------------|----------------------------------|
//! | relative        | `2h`, `30m`, `7d`, `1w`  | `now` minus that duration        |
//! | RFC 3339        | `2026-09-04T10:00:00Z`   | exactly that instant             |
//! | date only       | `2026-09-04`             | midnight UTC that day            |
//! | clock time only | `10:35`, `10:35:20`      | that time today (UTC), see below |
//!
//! A bare clock time that would land in the future resolves to the previous
//! day instead. During an incident "since 10:35" always means the 10:35 that
//! has already happened, never one 23 hours away.

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Utc};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TimeError {
    #[error("empty time value")]
    Empty,
    #[error(
        "cannot parse time {0:?}; expected a duration (2h, 30m, 7d), \
         an RFC 3339 instant (2026-09-04T10:00:00Z), a date (2026-09-04), \
         or a clock time (10:35)"
    )]
    Unparseable(String),
    #[error("duration {0:?} is missing a unit; use s, m, h, d or w (e.g. 2h)")]
    MissingUnit(String),
    #[error("duration {0:?} overflows the representable range")]
    Overflow(String),
}

/// Parse a `--since` / `--until` value into an absolute instant.
pub fn parse_instant(raw: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, TimeError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(TimeError::Empty);
    }

    if let Some(d) = parse_duration(s)? {
        return now
            .checked_sub_signed(d)
            .ok_or_else(|| TimeError::Overflow(s.to_string()));
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        // `and_hms_opt(0,0,0)` cannot fail for a valid date, but unwrapping a
        // parsed value is exactly the habit that turns a bad input into a panic.
        let naive = date
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| TimeError::Unparseable(s.to_string()))?;
        return Ok(Utc.from_utc_datetime(&naive));
    }
    if let Some(t) = parse_clock(s) {
        let today = Utc.from_utc_datetime(&now.date_naive().and_time(t));
        // A future clock time means the caller meant yesterday.
        return Ok(if today > now {
            today - Duration::days(1)
        } else {
            today
        });
    }
    Err(TimeError::Unparseable(s.to_string()))
}

/// Returns `Ok(None)` when `s` is not duration-shaped at all, leaving the
/// caller free to try the other forms. Returns `Err` only when `s` *is*
/// duration-shaped but malformed, so `12x` reports a bad unit rather than
/// falling through to a misleading "cannot parse time" further down.
fn parse_duration(s: &str) -> Result<Option<Duration>, TimeError> {
    let mut chars = s.chars();
    let digits: String = chars.by_ref().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return Ok(None);
    }
    // `take_while` consumed the delimiter, so rebuild the tail from the offset.
    let unit = &s[digits.len()..];
    if unit.is_empty() {
        return Err(TimeError::MissingUnit(s.to_string()));
    }
    let n: i64 = digits
        .parse()
        .map_err(|_| TimeError::Overflow(s.to_string()))?;
    let d = match unit {
        "s" => Duration::try_seconds(n),
        "m" => Duration::try_minutes(n),
        "h" => Duration::try_hours(n),
        "d" => Duration::try_days(n),
        "w" => Duration::try_weeks(n),
        _ => return Ok(None),
    };
    d.map(Some)
        .ok_or_else(|| TimeError::Overflow(s.to_string()))
}

fn parse_clock(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 4, 14, 0, 0).unwrap()
    }

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    #[test]
    fn relative_durations_subtract_from_now() {
        assert_eq!(
            parse_instant("90s", now()).unwrap(),
            at(2026, 9, 4, 13, 58, 30)
        );
        assert_eq!(
            parse_instant("30m", now()).unwrap(),
            at(2026, 9, 4, 13, 30, 0)
        );
        assert_eq!(
            parse_instant("2h", now()).unwrap(),
            at(2026, 9, 4, 12, 0, 0)
        );
        assert_eq!(
            parse_instant("7d", now()).unwrap(),
            at(2026, 8, 28, 14, 0, 0)
        );
        assert_eq!(
            parse_instant("1w", now()).unwrap(),
            at(2026, 8, 28, 14, 0, 0)
        );
    }

    #[test]
    fn rfc3339_instants_pass_through_and_normalise_to_utc() {
        assert_eq!(
            parse_instant("2026-09-04T10:00:00Z", now()).unwrap(),
            at(2026, 9, 4, 10, 0, 0)
        );
        // An offset instant is the same moment, re-expressed in UTC.
        assert_eq!(
            parse_instant("2026-09-04T06:00:00-04:00", now()).unwrap(),
            at(2026, 9, 4, 10, 0, 0)
        );
    }

    #[test]
    fn bare_date_means_midnight_utc() {
        assert_eq!(
            parse_instant("2026-09-04", now()).unwrap(),
            at(2026, 9, 4, 0, 0, 0)
        );
    }

    #[test]
    fn past_clock_time_resolves_to_today() {
        assert_eq!(
            parse_instant("10:35", now()).unwrap(),
            at(2026, 9, 4, 10, 35, 0)
        );
        assert_eq!(
            parse_instant("10:35:20", now()).unwrap(),
            at(2026, 9, 4, 10, 35, 20)
        );
    }

    #[test]
    fn future_clock_time_resolves_to_yesterday() {
        // 23:50 has not happened yet on the 4th, so the caller meant the 3rd.
        // Getting this backwards would silently return an empty log window.
        assert_eq!(
            parse_instant("23:50", now()).unwrap(),
            at(2026, 9, 3, 23, 50, 0)
        );
    }

    #[test]
    fn whitespace_is_tolerated() {
        assert_eq!(
            parse_instant("  2h  ", now()).unwrap(),
            at(2026, 9, 4, 12, 0, 0)
        );
    }

    #[test]
    fn bare_number_reports_the_missing_unit_not_a_generic_failure() {
        assert_eq!(
            parse_instant("12", now()),
            Err(TimeError::MissingUnit("12".into()))
        );
    }

    #[test]
    fn empty_and_garbage_are_rejected() {
        assert_eq!(parse_instant("", now()), Err(TimeError::Empty));
        assert_eq!(parse_instant("   ", now()), Err(TimeError::Empty));
        assert_eq!(
            parse_instant("yesterday", now()),
            Err(TimeError::Unparseable("yesterday".into()))
        );
        assert_eq!(
            parse_instant("2h30m", now()),
            Err(TimeError::Unparseable("2h30m".into()))
        );
    }

    #[test]
    fn absurd_durations_report_overflow_rather_than_panicking() {
        let huge = "999999999999w";
        assert!(matches!(
            parse_instant(huge, now()),
            Err(TimeError::Overflow(_))
        ));
    }
}
