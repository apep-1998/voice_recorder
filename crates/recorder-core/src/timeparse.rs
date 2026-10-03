//! Parsing human time-range expressions into absolute UTC windows.
//!
//! Supported forms (case-insensitive), resolved against a "now" and a local
//! timezone so results are deterministic and testable:
//!
//! - durations: `2h`, `90m`, `1h30m`, `45s`, `3d` — used with `--last`
//! - "N ago": `8h ago`, `30m ago`
//! - clock times today/yesterday: `yesterday 15:50`, `today 09:00`, `15:50`
//! - dates: `2026-10-03`, `2026-10-03 15:50`, `2026-10-03T15:50:00`
//! - RFC3339 with offset: `2026-10-03T15:50:00+02:00`
//!
//! A full range is two endpoints; [`parse_range`] also accepts `--last
//! <duration>` (now-duration .. now) and `<from> to <to>`.

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;

use crate::timeline::UtcNs;

/// A resolved, half-open UTC time window `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    pub start_ns: UtcNs,
    pub end_ns: UtcNs,
}

impl TimeRange {
    pub fn new(start_ns: UtcNs, end_ns: UtcNs) -> Self {
        Self { start_ns, end_ns }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TimeParseError {
    #[error("could not parse time expression {0:?}")]
    Bad(String),
    #[error("could not parse duration {0:?}")]
    BadDuration(String),
    #[error("ambiguous or non-existent local time {0:?} (DST transition)")]
    AmbiguousLocal(String),
    #[error("range end {end:?} is not after start {start:?}")]
    EmptyRange { start: String, end: String },
}

/// The clock + timezone a parse is resolved against. Injecting these keeps
/// parsing a pure function.
#[derive(Debug, Clone, Copy)]
pub struct ParseContext {
    pub now: DateTime<Utc>,
    pub tz: Tz,
}

impl ParseContext {
    pub fn new(now: DateTime<Utc>, tz: Tz) -> Self {
        Self { now, tz }
    }

    /// Context from the real clock and the system local timezone (falls back
    /// to UTC if the timezone cannot be determined).
    pub fn system() -> Self {
        Self {
            now: Utc::now(),
            tz: local_timezone(),
        }
    }
}

/// Best-effort system timezone: read `$TZ`, then `/etc/timezone`, then the
/// `/etc/localtime` symlink target; fall back to UTC.
pub fn local_timezone() -> Tz {
    if let Ok(tz) = std::env::var("TZ") {
        if let Ok(parsed) = tz.parse::<Tz>() {
            return parsed;
        }
    }
    if let Ok(name) = std::fs::read_to_string("/etc/timezone") {
        if let Ok(parsed) = name.trim().parse::<Tz>() {
            return parsed;
        }
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let path = target.to_string_lossy();
        if let Some(idx) = path.find("zoneinfo/") {
            if let Ok(parsed) = path[idx + "zoneinfo/".len()..].parse::<Tz>() {
                return parsed;
            }
        }
    }
    Tz::UTC
}

/// Parse a `--last <duration>` window: `[now - duration, now)`.
pub fn parse_last(input: &str, ctx: ParseContext) -> Result<TimeRange, TimeParseError> {
    let dur = parse_duration(input)?;
    let start = ctx.now - dur;
    Ok(TimeRange::new(to_ns(start), to_ns(ctx.now)))
}

/// Parse a full range from optional `from`/`to` endpoints. Missing `to`
/// means "until now"; missing `from` is an error (use `parse_last`).
pub fn parse_range(
    from: &str,
    to: Option<&str>,
    ctx: ParseContext,
) -> Result<TimeRange, TimeParseError> {
    let start = parse_instant(from, ctx)?;
    let end = match to {
        Some(t) => parse_instant(t, ctx)?,
        None => ctx.now,
    };
    if end <= start {
        return Err(TimeParseError::EmptyRange {
            start: from.to_owned(),
            end: to.unwrap_or("now").to_owned(),
        });
    }
    Ok(TimeRange::new(to_ns(start), to_ns(end)))
}

/// Parse a single instant expression into UTC.
///
/// # Panics
/// Panics only on an internally-constructed time that is always valid
/// (midnight); malformed input returns an error instead.
pub fn parse_instant(input: &str, ctx: ParseContext) -> Result<DateTime<Utc>, TimeParseError> {
    let s = input.trim();
    let lower = s.to_lowercase();

    if lower == "now" {
        return Ok(ctx.now);
    }

    // "<duration> ago"
    if let Some(rest) = lower.strip_suffix("ago") {
        let dur = parse_duration(rest.trim())?;
        return Ok(ctx.now - dur);
    }

    // RFC3339 (has its own offset).
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    // "yesterday HH:MM[:SS]" / "today HH:MM[:SS]"
    if let Some(rest) = lower.strip_prefix("yesterday") {
        let time = parse_clock(rest.trim())?;
        let date = ctx.now.with_timezone(&ctx.tz).date_naive() - ChronoDuration::days(1);
        return resolve_local(date, time, ctx, s);
    }
    if let Some(rest) = lower.strip_prefix("today") {
        let time = parse_clock(rest.trim())?;
        let date = ctx.now.with_timezone(&ctx.tz).date_naive();
        return resolve_local(date, time, ctx, s);
    }

    // "YYYY-MM-DD[ T]HH:MM[:SS]" or bare "YYYY-MM-DD"
    if let Some(dt) = parse_local_datetime(s) {
        return resolve_local(dt.date(), dt.time(), ctx, s);
    }
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let midnight = date.and_hms_opt(0, 0, 0).expect("valid midnight");
        return resolve_local(date, midnight.time(), ctx, s);
    }

    // Bare "HH:MM[:SS]" means today.
    if let Ok(time) = parse_clock(s) {
        let date = ctx.now.with_timezone(&ctx.tz).date_naive();
        return resolve_local(date, time, ctx, s);
    }

    Err(TimeParseError::Bad(input.to_owned()))
}

/// Parse a duration like `2h`, `1h30m`, `90m`, `45s`, `3d`.
pub fn parse_duration(input: &str) -> Result<ChronoDuration, TimeParseError> {
    let s = input.trim().to_lowercase();
    if s.is_empty() {
        return Err(TimeParseError::BadDuration(input.to_owned()));
    }
    // Reuse humantime for the compound/unit forms it understands.
    if let Ok(std) = humantime::parse_duration(&s) {
        return ChronoDuration::from_std(std)
            .map_err(|_| TimeParseError::BadDuration(input.to_owned()));
    }
    // humantime wants "1h 30m"; accept "1h30m" by inserting spaces.
    let spaced = insert_spaces(&s);
    if let Ok(std) = humantime::parse_duration(&spaced) {
        return ChronoDuration::from_std(std)
            .map_err(|_| TimeParseError::BadDuration(input.to_owned()));
    }
    Err(TimeParseError::BadDuration(input.to_owned()))
}

fn insert_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let mut prev_was_digit = false;
    for c in s.chars() {
        if c.is_ascii_alphabetic() && prev_was_digit {
            // keep letter attached to the number
        }
        if c.is_ascii_digit() && !prev_was_digit && !out.is_empty() {
            out.push(' ');
        }
        out.push(c);
        prev_was_digit = c.is_ascii_digit();
    }
    out
}

fn parse_clock(s: &str) -> Result<chrono::NaiveTime, TimeParseError> {
    for fmt in ["%H:%M:%S", "%H:%M"] {
        if let Ok(t) = chrono::NaiveTime::parse_from_str(s, fmt) {
            return Ok(t);
        }
    }
    Err(TimeParseError::Bad(s.to_owned()))
}

fn parse_local_datetime(s: &str) -> Option<NaiveDateTime> {
    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt);
        }
    }
    None
}

fn resolve_local(
    date: NaiveDate,
    time: chrono::NaiveTime,
    ctx: ParseContext,
    original: &str,
) -> Result<DateTime<Utc>, TimeParseError> {
    let naive = NaiveDateTime::new(date, time);
    match ctx.tz.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => Ok(dt.with_timezone(&Utc)),
        // During a fall-back DST overlap, pick the earlier instant.
        chrono::LocalResult::Ambiguous(earlier, _later) => Ok(earlier.with_timezone(&Utc)),
        chrono::LocalResult::None => Err(TimeParseError::AmbiguousLocal(original.to_owned())),
    }
}

fn to_ns(dt: DateTime<Utc>) -> UtcNs {
    u64::try_from(dt.timestamp_nanos_opt().unwrap_or(0)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed "now": 2026-10-03 17:00:00 UTC = 19:00 Berlin (CEST, +02:00).
    fn ctx() -> ParseContext {
        let now = Utc.with_ymd_and_hms(2026, 10, 3, 17, 0, 0).unwrap();
        ParseContext::new(now, chrono_tz::Europe::Berlin)
    }

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> UtcNs {
        to_ns(Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap())
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("2h").unwrap(), ChronoDuration::hours(2));
        assert_eq!(parse_duration("90m").unwrap(), ChronoDuration::minutes(90));
        assert_eq!(parse_duration("45s").unwrap(), ChronoDuration::seconds(45));
        assert_eq!(parse_duration("3d").unwrap(), ChronoDuration::days(3));
        assert_eq!(
            parse_duration("1h30m").unwrap(),
            ChronoDuration::minutes(90)
        );
        assert_eq!(
            parse_duration("1h 30m").unwrap(),
            ChronoDuration::minutes(90)
        );
        assert!(parse_duration("").is_err());
        assert!(parse_duration("banana").is_err());
    }

    #[test]
    fn last_window_ends_at_now() {
        let r = parse_last("2h", ctx()).unwrap();
        assert_eq!(r.start_ns, at(2026, 10, 3, 15, 0, 0));
        assert_eq!(r.end_ns, at(2026, 10, 3, 17, 0, 0));
    }

    #[test]
    fn ago_is_relative_to_now() {
        assert_eq!(
            parse_instant("8h ago", ctx()).unwrap(),
            Utc.with_ymd_and_hms(2026, 10, 3, 9, 0, 0).unwrap()
        );
        assert_eq!(
            parse_instant("30m ago", ctx()).unwrap(),
            Utc.with_ymd_and_hms(2026, 10, 3, 16, 30, 0).unwrap()
        );
    }

    #[test]
    fn from_ago_to_ago_range() {
        let r = parse_range("8h ago", Some("4h ago"), ctx()).unwrap();
        assert_eq!(r.start_ns, at(2026, 10, 3, 9, 0, 0));
        assert_eq!(r.end_ns, at(2026, 10, 3, 13, 0, 0));
    }

    #[test]
    fn yesterday_clock_uses_local_tz() {
        // Yesterday 15:50 Berlin (CEST +2) = 13:50 UTC on Oct 2.
        let r = parse_range("yesterday 15:50", Some("yesterday 16:20"), ctx()).unwrap();
        assert_eq!(r.start_ns, at(2026, 10, 2, 13, 50, 0));
        assert_eq!(r.end_ns, at(2026, 10, 2, 14, 20, 0));
    }

    #[test]
    fn today_and_bare_clock_match() {
        let today = parse_instant("today 09:00", ctx()).unwrap();
        let bare = parse_instant("09:00", ctx()).unwrap();
        assert_eq!(today, bare);
        // 09:00 Berlin = 07:00 UTC.
        assert_eq!(to_ns(today), at(2026, 10, 3, 7, 0, 0));
    }

    #[test]
    fn iso_date_and_datetime_local() {
        // Bare date = local midnight.
        assert_eq!(
            to_ns(parse_instant("2026-10-03", ctx()).unwrap()),
            at(2026, 10, 2, 22, 0, 0) // 00:00 Berlin = 22:00 UTC prev day
        );
        assert_eq!(
            to_ns(parse_instant("2026-10-03 15:50", ctx()).unwrap()),
            at(2026, 10, 3, 13, 50, 0)
        );
        assert_eq!(
            to_ns(parse_instant("2026-10-03T15:50:30", ctx()).unwrap()),
            at(2026, 10, 3, 13, 50, 30)
        );
    }

    #[test]
    fn rfc3339_with_offset() {
        assert_eq!(
            to_ns(parse_instant("2026-10-03T15:50:00+02:00", ctx()).unwrap()),
            at(2026, 10, 3, 13, 50, 0)
        );
        assert_eq!(
            to_ns(parse_instant("2026-10-03T13:50:00Z", ctx()).unwrap()),
            at(2026, 10, 3, 13, 50, 0)
        );
    }

    #[test]
    fn empty_and_backwards_ranges_rejected() {
        assert!(matches!(
            parse_range("4h ago", Some("8h ago"), ctx()),
            Err(TimeParseError::EmptyRange { .. })
        ));
        assert!(matches!(
            parse_range("today 10:00", Some("today 10:00"), ctx()),
            Err(TimeParseError::EmptyRange { .. })
        ));
    }

    #[test]
    fn missing_to_means_now() {
        let r = parse_range("2h ago", None, ctx()).unwrap();
        assert_eq!(r.end_ns, at(2026, 10, 3, 17, 0, 0));
    }

    #[test]
    fn spring_forward_gap_is_rejected() {
        // Berlin 2026-03-29 02:30 does not exist (clocks jump 02:00->03:00).
        let spring = ParseContext::new(
            Utc.with_ymd_and_hms(2026, 3, 29, 12, 0, 0).unwrap(),
            chrono_tz::Europe::Berlin,
        );
        assert!(matches!(
            parse_instant("2026-03-29 02:30", spring),
            Err(TimeParseError::AmbiguousLocal(_))
        ));
    }

    #[test]
    fn fall_back_overlap_picks_earlier() {
        // Berlin 2026-10-25 02:30 occurs twice; we take the earlier (CEST,
        // +2 -> 00:30 UTC) rather than the later (CET, +1 -> 01:30 UTC).
        let fall = ParseContext::new(
            Utc.with_ymd_and_hms(2026, 10, 25, 12, 0, 0).unwrap(),
            chrono_tz::Europe::Berlin,
        );
        assert_eq!(
            to_ns(parse_instant("2026-10-25 02:30", fall).unwrap()),
            at(2026, 10, 25, 0, 30, 0)
        );
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(parse_instant("tomorrow maybe", ctx()).is_err());
        assert!(parse_instant("", ctx()).is_err());
    }
}
