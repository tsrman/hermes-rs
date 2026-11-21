//! Five-field cron expressions evaluated on a naive wall clock.
//!
//! Supports lists, ranges, steps, `*`, month and weekday names, and Sunday as 0 or 7.
//! Seconds fields, `L`, `W`, `#` and `@` aliases are rejected.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};

#[derive(Debug, PartialEq, Eq)]
pub(super) struct CronExpr {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    day_star: bool,
    weekday_star: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

impl CronExpr {
    pub(super) fn parse(expr: &str) -> Result<Self, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "expected 5 fields, found {} (seconds fields are not supported)",
                fields.len()
            ));
        }
        let minutes = field(fields[0], 0, 59, &[])?;
        let hours = field(fields[1], 0, 23, &[])?;
        let days = field(fields[2], 1, 31, &[])?;
        let months = field(fields[3], 1, 12, &MONTHS)?;
        // Sunday may be written as 7; fold it onto 0 after expanding.
        let mut weekdays = field(fields[4], 0, 7, &WEEKDAYS)?;
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays & !(1 << 7)) | 1;
        }
        Ok(Self {
            minutes,
            hours: hours as u32,
            days: days as u32,
            months: months as u16,
            weekdays: weekdays as u8,
            day_star: fields[2].starts_with('*'),
            weekday_star: fields[4].starts_with('*'),
        })
    }

    fn day_matches(&self, date: NaiveDate) -> bool {
        if self.months & (1 << date.month()) == 0 {
            return false;
        }
        let day = self.days & (1 << date.day()) != 0;
        let weekday = self.weekdays & (1 << date.weekday().num_days_from_sunday()) != 0;
        // Like cron, a restricted day of month and a restricted weekday match either.
        if self.day_star || self.weekday_star {
            day && weekday
        } else {
            day || weekday
        }
    }

    /// The first matching minute strictly after `base`.
    pub(super) fn next_after(&self, base: NaiveDateTime) -> Option<NaiveDateTime> {
        let start = base.with_second(0)?.with_nanosecond(0)? + Duration::minutes(1);
        let mut date = start.date();
        // Eight years cover a February 29 that falls on a restricted weekday.
        let last = date + Duration::days(366 * 8);
        while date <= last {
            if self.day_matches(date) {
                let from = if date == start.date() {
                    start.time()
                } else {
                    NaiveTime::MIN
                };
                for hour in from.hour()..24 {
                    if self.hours & (1 << hour) == 0 {
                        continue;
                    }
                    let first = if date == start.date() && hour == from.hour() {
                        from.minute()
                    } else {
                        0
                    };
                    for minute in first..60 {
                        if self.minutes & (1 << minute) != 0 {
                            return date.and_hms_opt(hour, minute, 0);
                        }
                    }
                }
            }
            date = date.succ_opt()?;
        }
        None
    }
}

fn value(text: &str, names: &[&str], min: u32) -> Result<u32, String> {
    let lower = text.to_ascii_lowercase();
    if let Some(index) = names.iter().position(|name| *name == lower) {
        // Month names start at 1, weekday names at 0.
        return Ok(index as u32 + if min == 1 { 1 } else { 0 });
    }
    if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
        return text.parse().map_err(|_| format!("invalid value '{text}'"));
    }
    Err(format!("invalid value '{text}'"))
}

fn field(spec: &str, min: u32, max: u32, names: &[&str]) -> Result<u64, String> {
    let mut bits = 0_u64;
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step
                    .parse()
                    .ok()
                    .filter(|step| *step > 0)
                    .ok_or_else(|| format!("invalid step in '{part}'"))?;
                (range, Some(step))
            }
            None => (part, None),
        };
        let (low, high) = if range == "*" {
            (min, max)
        } else if let Some((low, high)) = range.split_once('-') {
            (value(low, names, min)?, value(high, names, min)?)
        } else {
            let low = value(range, names, min)?;
            // `a/n` runs from `a` to the end of the field.
            (low, if step.is_some() { max } else { low })
        };
        if low < min || high > max || low > high {
            return Err(format!("value out of range in '{part}'"));
        }
        let mut current = low;
        while current <= high {
            bits |= 1 << current;
            current += step.unwrap_or(1);
        }
    }
    Ok(bits)
}
