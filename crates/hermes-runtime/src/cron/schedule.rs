//! Schedule strings, stored schedules and next-run computation.
//!
//! Mirrors `cron/jobs.py` of the Python reference: `parse_schedule` and `compute_next_run`.

use chrono::{
    DateTime, Duration, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc,
};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use super::expr::CronExpr;

/// How long after its time a one-shot job is still eligible.
const ONESHOT_GRACE_SECONDS: i64 = 120;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Schedule {
    pub kind: String,
    #[serde(default)]
    pub display: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minutes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,
}

impl Schedule {
    fn interval(minutes: u64) -> Self {
        Self {
            kind: "interval".into(),
            display: format!("every {minutes}m"),
            run_at: None,
            minutes: Some(minutes),
            expr: None,
        }
    }

    fn cron(expr: &str, display: &str) -> Result<Self, String> {
        CronExpr::parse(expr).map_err(|error| format!("Invalid schedule '{display}': {error}"))?;
        Ok(Self {
            kind: "cron".into(),
            display: display.to_owned(),
            run_at: None,
            minutes: None,
            expr: Some(expr.to_owned()),
        })
    }

    fn once(run_at: String, display: String) -> Self {
        Self {
            kind: "once".into(),
            display,
            run_at: Some(run_at),
            minutes: None,
            expr: None,
        }
    }
}

/// Minutes in a duration such as `30m`, `2h`, `1d` or a bare unit like `hour`.
pub fn parse_duration(text: &str) -> Result<u64, String> {
    let invalid = || {
        format!(
            "Invalid duration: '{}'. Use format like '30m', '2h', '1d', or a bare unit like 'hour' (defaults to 1).",
            text.trim().to_lowercase()
        )
    };
    let lower = text.trim().to_lowercase();
    let digits = lower.chars().take_while(char::is_ascii_digit).count();
    let (number, unit) = lower.split_at(digits);
    let unit = unit.trim_start();
    let factor = match unit {
        "m" | "min" | "mins" | "minute" | "minutes" => 1,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60,
        "d" | "day" | "days" => 1440,
        _ => return Err(invalid()),
    };
    let value = if number.is_empty() {
        1
    } else {
        number.parse::<u64>().map_err(|_| invalid())?
    };
    value.checked_mul(factor).ok_or_else(invalid)
}

fn weekday_number(token: &str) -> Option<&'static str> {
    Some(match token {
        "sunday" | "sun" => "0",
        "monday" | "mon" => "1",
        "tuesday" | "tue" | "tues" => "2",
        "wednesday" | "wed" | "weds" => "3",
        "thursday" | "thu" | "thur" | "thurs" => "4",
        "friday" | "fri" => "5",
        "saturday" | "sat" => "6",
        _ => return None,
    })
}

fn dayspec(token: &str) -> Option<&'static str> {
    Some(match token {
        "day" | "daily" | "everyday" => "*",
        "weekday" | "weekdays" => "1-5",
        "weekend" | "weekends" => "0,6",
        _ => return None,
    })
}

/// `9am`, `9:30am`, `14:00`, a bare 24-hour `7`, `noon` or `midnight` as (hour, minute).
fn parse_clock_time(text: &str) -> Option<(u32, u32)> {
    let compact: String = text
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| *c != ' ')
        .collect();
    match compact.as_str() {
        "" => return None,
        "noon" | "midday" => return Some((12, 0)),
        "midnight" => return Some((0, 0)),
        _ => {}
    }
    let (clock, meridiem) = match compact.strip_suffix("am") {
        Some(rest) => (rest, Some(false)),
        None => match compact.strip_suffix("pm") {
            Some(rest) => (rest, Some(true)),
            None => (compact.as_str(), None),
        },
    };
    let (hour, minute) = match clock.split_once(':') {
        Some((hour, minute)) if minute.len() == 2 => (hour, minute),
        Some(_) => return None,
        None => (clock, "0"),
    };
    let digits = |text: &str, max_len: usize| {
        (!text.is_empty() && text.len() <= max_len && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse::<u32>().ok())
            .flatten()
    };
    let mut hour = digits(hour, 2)?;
    let minute = digits(minute, 2)?;
    if let Some(pm) = meridiem {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour = hour % 12 + if pm { 12 } else { 0 };
    }
    (hour <= 23 && minute <= 59).then_some((hour, minute))
}

/// `<when> [at] <time>` as a five-field cron expression.
fn natural_every_to_cron(rest: &str) -> Option<String> {
    let lowered = rest.to_lowercase().replace(',', " ");
    let tokens: Vec<&str> = lowered.split_whitespace().collect();
    let first = tokens.first()?;
    let (dow, index) = match dayspec(first) {
        Some(dow) => (dow.to_owned(), 1),
        None => {
            let mut days: Vec<&str> = Vec::new();
            let mut index = tokens.len();
            for (i, token) in tokens.iter().enumerate() {
                if *token == "and" {
                    continue;
                }
                match weekday_number(token) {
                    Some(number) => {
                        if !days.contains(&number) {
                            days.push(number);
                        }
                    }
                    None => {
                        index = i;
                        break;
                    }
                }
            }
            if days.is_empty() {
                return None;
            }
            (days.join(","), index)
        }
    };
    let mut time_tokens = &tokens[index..];
    if time_tokens.first() == Some(&"at") {
        time_tokens = &time_tokens[1..];
    }
    if time_tokens.is_empty() {
        return None;
    }
    let (hour, minute) = parse_clock_time(&time_tokens.join(" "))?;
    Some(format!("{minute} {hour} * * {dow}"))
}

/// Python `isoformat()` for a wall clock with a fixed offset.
fn isoformat(wall: NaiveDateTime, offset_seconds: i32) -> String {
    let micros = wall.and_utc().timestamp_subsec_micros();
    let fraction = if micros > 0 {
        format!(".{micros:06}")
    } else {
        String::new()
    };
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let abs = offset_seconds.unsigned_abs();
    let mut offset = format!("{sign}{:02}:{:02}", abs / 3600, abs % 3600 / 60);
    if !abs.is_multiple_of(60) {
        offset.push_str(&format!(":{:02}", abs % 60));
    }
    format!("{}{fraction}{offset}", wall.format("%Y-%m-%dT%H:%M:%S"))
}

/// Python `isoformat()` of an instant in its own zone.
pub fn iso_instant(instant: DateTime<Tz>) -> String {
    isoformat(
        instant.naive_local(),
        instant.offset().fix().local_minus_utc(),
    )
}

/// Offset of `wall` in `zone` following Python's rules: `fold` 0 takes the earlier reading of a
/// repeated hour and the offset in force before a skipped hour, `fold` 1 the later one.
fn wall_offset(zone: Tz, wall: NaiveDateTime, fold: u8) -> i32 {
    let at = |wall: NaiveDateTime| match zone.from_local_datetime(&wall) {
        LocalResult::Single(dt) => Some(dt.offset().fix().local_minus_utc()),
        LocalResult::Ambiguous(first, second) => Some(
            if fold == 0 { first } else { second }
                .offset()
                .fix()
                .local_minus_utc(),
        ),
        LocalResult::None => None,
    };
    at(wall).unwrap_or_else(|| {
        // Inside a skipped hour: read the offset from just before or after the transition.
        let probe = if fold == 0 {
            wall - Duration::hours(3)
        } else {
            wall + Duration::hours(3)
        };
        at(probe).unwrap_or(0)
    })
}

fn wall_timestamp(wall: NaiveDateTime, offset: i32) -> i64 {
    wall.and_utc().timestamp() - i64::from(offset)
}

/// A timestamp from `fromisoformat`-style text. Offset-less values are `None`.
fn parse_iso(text: &str) -> Option<(NaiveDateTime, Option<i32>)> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return Some((dt.naive_local(), Some(dt.offset().local_minus_utc())));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M%:z",
    ] {
        if let Ok(dt) = DateTime::<FixedOffset>::parse_from_str(text, format) {
            return Some((dt.naive_local(), Some(dt.offset().local_minus_utc())));
        }
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(wall) = NaiveDateTime::parse_from_str(text, format) {
            return Some((wall, None));
        }
    }
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|wall| (wall, None))
}

/// The host's zone, which the reference uses for legacy offset-less stored timestamps.
fn system_zone(fallback: Tz) -> Tz {
    iana_time_zone::get_timezone()
        .ok()
        .and_then(|name| name.parse().ok())
        .unwrap_or(fallback)
}

/// UTC timestamp of a stored value. An offset-less value is the pinned zone's wall clock, or
/// else host-local time.
fn instant_in(pinned: Option<Tz>, fallback: Tz, wall: NaiveDateTime, offset: Option<i32>) -> i64 {
    let offset = offset
        .unwrap_or_else(|| wall_offset(pinned.unwrap_or_else(|| system_zone(fallback)), wall, 0));
    wall_timestamp(wall, offset)
}

/// UTC timestamp of a stored ISO value; offset-less text is host-local time as in the reference.
pub fn parse_iso_instant(text: &str, profile: Tz) -> Option<i64> {
    let (wall, offset) = parse_iso(text)?;
    Some(instant_in(None, profile, wall, offset))
}

/// Parse a schedule string. `now` carries the profile time zone.
pub fn parse_schedule(text: &str, now: DateTime<Tz>) -> Result<Schedule, String> {
    let original = text.trim();
    let lower = original.to_lowercase();
    let is_every = lower.starts_with("every ");
    let rest = if is_every {
        original[6..].trim()
    } else {
        &lower
    };
    if let Some(expr) = natural_every_to_cron(rest) {
        return Schedule::cron(&expr, original);
    }
    if is_every {
        return parse_duration(rest).map(Schedule::interval);
    }

    let parts: Vec<&str> = original.split_whitespace().collect();
    let cron_like = |part: &&str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'*' | b'-' | b',' | b'/'))
    };
    if parts.len() >= 5 && parts[..5].iter().all(cron_like) {
        return Schedule::cron(original, original)
            .map_err(|error| error.replacen("Invalid schedule", "Invalid cron expression", 1));
    }

    if original.contains('T') || looks_like_date(original) {
        let (wall, offset) = parse_iso(original)
            .ok_or_else(|| format!("Invalid timestamp '{original}': Invalid isoformat string"))?;
        let zone = now.timezone();
        let offset = offset.unwrap_or_else(|| wall_offset(zone, wall, 0));
        return Ok(Schedule::once(
            isoformat(wall, offset),
            format!("once at {}", wall.format("%Y-%m-%d %H:%M")),
        ));
    }

    if lower.starts_with("in ") {
        let duration = original[3..].trim();
        let minutes = parse_duration(duration).map_err(|_| {
            format!("Invalid duration '{duration}' after 'in '. Use e.g. 'in 30m', 'in 2h'.")
        })?;
        let minutes = i64::try_from(minutes).map_err(|_| "duration is too large".to_owned())?;
        let run_at = now
            .checked_add_signed(Duration::minutes(minutes))
            .ok_or("duration is too large")?;
        return Ok(Schedule::once(
            iso_instant(run_at),
            format!("once in {duration}"),
        ));
    }
    if let Ok(minutes) = parse_duration(original) {
        return Ok(Schedule::interval(minutes));
    }
    Err(format!(
        "Invalid schedule '{original}'. Use:\n  - Interval: '30m', 'every 30m', 'every 2h' (recurring)\n  - One-shot delay: 'in 30m', 'in 2h' (fires once)\n  - Weekly/daily: 'every monday 9am', 'weekdays at 9am' (recurring)\n  - Cron: '0 9 * * *' (cron expression)\n  - Timestamp: '2026-02-03T14:00:00' (one-shot at time)"
    ))
}

fn looks_like_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 10
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
}

/// The next run as an ISO string, or `None` when no run remains.
///
/// `job_zone` is the job's pinned zone; `now` carries the profile zone.
pub fn compute_next_run(
    schedule: &Schedule,
    last_run_at: Option<&str>,
    job_zone: Option<Tz>,
    now: DateTime<Tz>,
) -> Option<String> {
    let profile = now.timezone();
    match schedule.kind.as_str() {
        "once" => {
            if last_run_at.is_some_and(|value| !value.is_empty()) {
                return None;
            }
            let run_at = schedule
                .run_at
                .as_deref()
                .filter(|value| !value.is_empty())?;
            let (wall, offset) = parse_iso(run_at)?;
            let instant = instant_in(job_zone, profile, wall, offset);
            (now.timestamp() - instant <= ONESHOT_GRACE_SECONDS).then(|| run_at.to_owned())
        }
        "interval" => {
            let base = base_time(last_run_at, now);
            let next =
                base.checked_add_signed(Duration::minutes(i64::try_from(schedule.minutes?).ok()?))?;
            Some(iso_instant(next))
        }
        "cron" => {
            let expr = CronExpr::parse(schedule.expr.as_deref().filter(|e| !e.is_empty())?).ok()?;
            let base = base_time(last_run_at, now);
            let zone = job_zone.unwrap_or(profile);
            let base_wall = base.with_timezone(&zone).naive_local();
            let base_ts = base.timestamp();
            let mut next_wall = expr.next_after(base_wall)?;
            // Try both readings of a repeated hour and keep the first instant after the base.
            for _ in 0..2 {
                for fold in 0..2 {
                    let offset = wall_offset(zone, next_wall, fold);
                    if wall_timestamp(next_wall, offset) > base_ts {
                        return Some(isoformat(next_wall, offset));
                    }
                }
                next_wall = expr.next_after(next_wall)?;
            }
            Some(isoformat(next_wall, wall_offset(zone, next_wall, 0)))
        }
        _ => None,
    }
}

/// `last_run_at` expressed in the profile zone, or `now`.
fn base_time(last_run_at: Option<&str>, now: DateTime<Tz>) -> DateTime<Tz> {
    let profile = now.timezone();
    last_run_at
        .filter(|value| !value.is_empty())
        .and_then(|text| {
            let (wall, offset) = parse_iso(text)?;
            let instant = instant_in(None, profile, wall, offset);
            Utc.timestamp_opt(instant, wall.and_utc().timestamp_subsec_nanos())
                .single()
        })
        .map_or(now, |utc| utc.with_timezone(&profile))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Tz> {
        chrono_tz::UTC
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .unwrap()
    }

    #[test]
    fn rejects_seconds_fields_and_cron_extensions() {
        for expr in ["0 9 * * * *", "0 9 L * *", "@daily", "0 9 * * 1#2"] {
            assert!(parse_schedule(expr, now()).is_err(), "{expr}");
        }
    }

    #[test]
    fn handles_values_after_a_skipped_hour() {
        let zone: Tz = "America/Toronto".parse().unwrap();
        let now = zone.with_ymd_and_hms(2026, 3, 8, 1, 30, 0).unwrap();
        let schedule = parse_schedule("30 2 * * *", now).unwrap();
        let next = compute_next_run(&schedule, None, None, now).unwrap();
        // The reference keeps the nonexistent wall time with the pre-transition offset.
        assert_eq!(next, "2026-03-08T02:30:00-05:00");
    }
}
