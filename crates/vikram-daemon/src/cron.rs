//! When work runs.
//!
//! Three shapes cover everything the operator asks for, and they are all parsed here so the rest
//! of the daemon never sees a string it has to interpret:
//!
//! * `every 10m` — a fixed period, measured from when the job was created.
//! * `in 30m` / `at 2026-11-03T09:00` — a single instant.
//! * `*/10 * * * *` — a five field cron expression, evaluated in the job's own zone.
//!
//! Cron expressions are minutes-resolution, like every cron since V7: there is no seconds field.
//! Day-of-month and day-of-week follow the traditional rule — when both are restricted a day
//! matches if either does (`0 9 1 * mon` fires on the first of the month *and* on Mondays).
//!
//! Named IANA zones are not supported; see [`crate::time::Zone`] for why. A job's zone is `local`,
//! `UTC` or a fixed offset, and the wall clock reading it produces is what a person would read off
//! their own clock.

use std::fmt;

use crate::time::{self, Civil, Millis, Zone};

/// Why a schedule string could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    /// Nothing to parse.
    Empty,
    /// A cron expression must have exactly five fields.
    FieldCount(usize),
    /// A field that is not `*`, a number, a range, a step or a list of those.
    BadField { field: &'static str, text: String },
    /// A number outside the field's own range.
    OutOfRange {
        field: &'static str,
        value: i64,
        min: i64,
        max: i64,
    },
    /// A duration the operator could not have meant (`0m`, `10x`, `1h30`).
    BadDuration(String),
    /// A date or time that is not one.
    BadTimestamp(String),
    /// A word in front of the expression that names no schedule.
    UnknownShape(String),
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScheduleError::Empty => write!(f, "the schedule is empty"),
            ScheduleError::FieldCount(n) => write!(
                f,
                "a cron expression has five fields (minute hour day month weekday), got {n}"
            ),
            ScheduleError::BadField { field, text } => {
                write!(f, "`{text}` is not a valid {field} field")
            }
            ScheduleError::OutOfRange {
                field,
                value,
                min,
                max,
            } => write!(f, "{field} {value} is outside {min}-{max}"),
            ScheduleError::BadDuration(text) => {
                write!(f, "`{text}` is not a duration (try 30m, 2h, 1d, 1h30m)")
            }
            ScheduleError::BadTimestamp(text) => {
                write!(
                    f,
                    "`{text}` is not a date (try 2026-11-03 or 2026-11-03T09:00)"
                )
            }
            ScheduleError::UnknownShape(text) => {
                write!(
                    f,
                    "`{text}` is not a schedule (try `every 10m`, `in 30m`, or a cron expression)"
                )
            }
        }
    }
}

impl std::error::Error for ScheduleError {}

/// When a job runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// Exactly once, at this instant.
    Once { at: Millis },
    /// Every `period`, counted from `from`.
    Every { period: Millis, from: Millis },
    /// A five field cron expression, read in `zone`.
    Cron { fields: CronExpr, zone: Zone },
}

impl Schedule {
    /// Read the shapes an operator actually types.
    ///
    /// `now` anchors the relative shapes (`in 30m`, `every 10m`); a cron expression needs it not,
    /// but it is passed anyway so every shape is created the same way.
    pub fn parse(spec: &str, zone: Zone, now: Millis) -> Result<Self, ScheduleError> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(ScheduleError::Empty);
        }
        let lower = spec.to_ascii_lowercase();

        // One word aliases, so a job can say `@daily` instead of `0 0 * * *`.
        let alias = match lower.as_str() {
            "@hourly" => Some("0 * * * *"),
            "@daily" | "@midnight" => Some("0 0 * * *"),
            "@weekly" => Some("0 0 * * 0"),
            "@monthly" => Some("0 0 1 * *"),
            "@yearly" | "@annually" => Some("0 0 1 1 *"),
            _ => None,
        };
        if let Some(expression) = alias {
            return Ok(Schedule::Cron {
                fields: CronExpr::parse(expression)?,
                zone,
            });
        }

        if let Some(rest) = lower.strip_prefix("every ") {
            let period = time::parse_duration(rest)
                .ok_or_else(|| ScheduleError::BadDuration(rest.trim().to_string()))?;
            return Ok(Schedule::Every { period, from: now });
        }
        if let Some(rest) = lower.strip_prefix("in ") {
            let delay = time::parse_duration(rest)
                .ok_or_else(|| ScheduleError::BadDuration(rest.trim().to_string()))?;
            return Ok(Schedule::Once { at: now + delay });
        }
        if let Some(rest) = lower.strip_prefix("at ") {
            let rest = rest.trim();
            // An absolute instant may be written as epoch milliseconds. That is how a schedule that
            // was relative when it was created (`in 30m`) is frozen once it has been resolved, so
            // reading the job again cannot move it.
            if rest.len() >= 10 && rest.bytes().all(|byte| byte.is_ascii_digit()) {
                let at: Millis = rest
                    .parse()
                    .map_err(|_| ScheduleError::BadTimestamp(rest.to_string()))?;
                return Ok(Schedule::Once { at });
            }
            let at = time::parse_timestamp(rest, zone)
                .ok_or_else(|| ScheduleError::BadTimestamp(rest.to_string()))?;
            return Ok(Schedule::Once { at });
        }
        if let Some(rest) = lower.strip_prefix("once ") {
            // `once 30m` means the same as `in 30m`, because a bare duration after `once` can only
            // be an offset from now.
            let delay = time::parse_duration(rest)
                .ok_or_else(|| ScheduleError::BadDuration(rest.trim().to_string()))?;
            return Ok(Schedule::Once { at: now + delay });
        }

        if spec.split_whitespace().count() == 5 {
            return Ok(Schedule::Cron {
                fields: CronExpr::parse(spec)?,
                zone,
            });
        }

        Err(ScheduleError::UnknownShape(spec.to_string()))
    }

    /// The first run strictly after `after`, or `None` when there will never be one.
    pub fn next_after(&self, after: Millis) -> Option<Millis> {
        match self {
            Schedule::Once { at } => (*at > after).then_some(*at),
            Schedule::Every { period, from } => {
                if *period <= 0 {
                    return None;
                }
                if *from > after {
                    return Some(*from);
                }
                let elapsed = after - from;
                let slots = elapsed / period + 1;
                Some(from + slots * period)
            }
            Schedule::Cron { fields, zone } => fields.next_after(*zone, after),
        }
    }

    /// What this schedule means, in words, for `cron list` and `cron show`.
    pub fn describe(&self) -> String {
        match self {
            Schedule::Once { at } => format!("once, at {}", time::format_ms(Zone::Local, *at)),
            Schedule::Every { period, .. } => {
                format!("every {}", time::humanize_ms(*period))
            }
            Schedule::Cron { fields, zone } => {
                if *zone == Zone::Local {
                    format!("cron {}", fields.source)
                } else {
                    format!("cron {} ({zone})", fields.source)
                }
            }
        }
    }

    /// A stable name for the shape, for tables and for tests.
    pub fn kind(&self) -> &'static str {
        match self {
            Schedule::Once { .. } => "once",
            Schedule::Every { .. } => "every",
            Schedule::Cron { .. } => "cron",
        }
    }

    /// The zone the schedule is read in. A period has no wall clock, so it reports `local`.
    pub fn zone(&self) -> Zone {
        match self {
            Schedule::Cron { zone, .. } => *zone,
            _ => Zone::Local,
        }
    }
}

impl fmt::Display for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// A parsed five field cron expression.
#[derive(Debug, Clone)]
pub struct CronExpr {
    /// Minutes 0-59.
    minute: u64,
    /// Hours 0-23.
    hour: u64,
    /// Days of the month 1-31.
    day: u64,
    /// Months 1-12.
    month: u64,
    /// Days of the week, Sunday 0 … Saturday 6.
    weekday: u64,
    /// Which of the day fields were written as `*`, because the traditional union rule needs it.
    day_restricted: bool,
    weekday_restricted: bool,
    /// The text it was parsed from, kept for display.
    source: String,
}

/// Cron numbers its days from Sunday, `Civil::weekday` numbers them from Monday. This conversion
/// lives here because the cron expression is the only thing that speaks the former.
fn cron_weekday(civil: &Civil) -> u32 {
    (civil.weekday() + 1) % 7
}

impl PartialEq for CronExpr {
    /// Two spellings of the same schedule are the same schedule: `0 9 * * 7` and `0 9 * * 0` both
    /// mean Sunday at nine. `source` is display text and takes no part in equality.
    fn eq(&self, other: &Self) -> bool {
        self.minute == other.minute
            && self.hour == other.hour
            && self.day == other.day
            && self.month == other.month
            && self.weekday == other.weekday
            && self.day_restricted == other.day_restricted
            && self.weekday_restricted == other.weekday_restricted
    }
}

impl Eq for CronExpr {}

const MONTH_NAMES: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

impl CronExpr {
    /// Parse `minute hour day month weekday`.
    pub fn parse(expression: &str) -> Result<Self, ScheduleError> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(ScheduleError::FieldCount(fields.len()));
        }
        let (minute, _) = parse_field(fields[0], "minute", 0, 59, &[])?;
        let (hour, _) = parse_field(fields[1], "hour", 0, 23, &[])?;
        let (day, day_restricted) = parse_field(fields[2], "day of month", 1, 31, &[])?;
        let (month, _) = parse_field(fields[3], "month", 1, 12, &month_names())?;
        let (weekday, weekday_restricted) =
            parse_field(fields[4], "weekday", 0, 7, &weekday_names())?;
        // Cron accepts 7 for Sunday; it is the same day as 0.
        let weekday = if weekday & (1 << 7) != 0 {
            (weekday & !(1 << 7)) | 1
        } else {
            weekday
        };
        Ok(CronExpr {
            minute,
            hour,
            day,
            month,
            weekday,
            day_restricted,
            weekday_restricted,
            source: fields.join(" "),
        })
    }

    /// The expression as it was written.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether a civil reading matches every field.
    pub fn matches(&self, civil: &Civil) -> bool {
        let (year, month, day) = (civil.year, civil.month, civil.day);
        if self.month & (1 << month) == 0 {
            return false;
        }
        if self.hour & (1 << civil.hour) == 0 || self.minute & (1 << civil.minute) == 0 {
            return false;
        }
        let day_ok = self.day & (1 << day) != 0;
        let days_in_month = time::days_in_month(year, month);
        // A `31` in a 30 day month can never match, and must not be treated as "any day".
        let day_ok = day_ok && day <= days_in_month;
        let weekday_ok = self.weekday & (1 << cron_weekday(civil)) != 0;
        match (self.day_restricted, self.weekday_restricted) {
            // Neither restricted: every day matches.
            (false, false) => true,
            // One restricted: only that one decides.
            (true, false) => day_ok,
            (false, true) => weekday_ok,
            // Both restricted: the traditional union.
            (true, true) => day_ok || weekday_ok,
        }
    }

    /// The first instant in `zone` strictly after `after` that matches this expression.
    pub fn next_after(&self, zone: Zone, after: Millis) -> Option<Millis> {
        // Start at the next whole minute: a cron expression has no seconds, so the current minute
        // can never match again.
        let mut candidate = align_minute(after);
        let mut civil = time::civil_in(zone, candidate);
        // Five years covers every gap a valid expression can have (a leap day is the widest).
        let deadline = after + 5 * 366 * 86_400_000;
        while candidate < deadline {
            if self.month & (1 << civil.month) == 0 {
                advance_month(&mut civil);
            } else if !self.day_matches(&civil) {
                advance_day(&mut civil);
            } else if self.hour & (1 << civil.hour) == 0 {
                advance_hour(&mut civil);
            } else if self.minute & (1 << civil.minute) == 0 {
                advance_minute(&mut civil);
            } else {
                // The civil reading matches; the instant it names has to read back the same way,
                // otherwise the reading fell in a daylight saving gap and this minute does not exist.
                let instant = time::instant_of(zone, civil);
                if time::civil_in(zone, instant) == civil {
                    return Some(instant);
                }
                advance_minute(&mut civil);
            }
            candidate = time::instant_of(zone, civil);
        }
        None
    }

    fn day_matches(&self, civil: &Civil) -> bool {
        let day = civil.day;
        let in_month = day <= time::days_in_month(civil.year, civil.month);
        let day_ok = in_month && self.day & (1 << day) != 0;
        let weekday_ok = self.weekday & (1 << cron_weekday(civil)) != 0;
        match (self.day_restricted, self.weekday_restricted) {
            (false, false) => true,
            (true, false) => day_ok,
            (false, true) => weekday_ok,
            (true, true) => day_ok || weekday_ok,
        }
    }
}

fn month_names() -> Vec<(&'static str, u32)> {
    MONTH_NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| (*name, index as u32 + 1))
        .collect()
}

fn weekday_names() -> Vec<(&'static str, u32)> {
    WEEKDAY_NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| (*name, index as u32))
        .collect()
}

/// Read one field into a bitmask: which values it allows.
///
/// Returns the mask, whether the field was written as anything other than `*` (the union rule for
/// the two day fields needs to know), and the mask unchanged for callers that want it.
fn parse_field(
    text: &str,
    field: &'static str,
    min: u32,
    max: u32,
    names: &[(&'static str, u32)],
) -> Result<(u64, bool), ScheduleError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ScheduleError::BadField {
            field,
            text: String::new(),
        });
    }
    if text == "*" || text == "?" {
        let mask = ((1u64 << (max - min + 1)) - 1) << min;
        return Ok((mask, false));
    }
    let mut mask = 0u64;
    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(ScheduleError::BadField {
                field,
                text: text.to_string(),
            });
        }
        // `*/n` and `a-b/n` and `a/n` all end in a step.
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step.parse().map_err(|_| ScheduleError::BadField {
                    field,
                    text: part.to_string(),
                })?;
                if step == 0 {
                    return Err(ScheduleError::BadField {
                        field,
                        text: part.to_string(),
                    });
                }
                (range, Some(step))
            }
            None => (part, None),
        };
        let (start, end) = if range == "*" {
            (min, max)
        } else if let Some((start, end)) = range.split_once('-') {
            (
                value_of(start, field, min, max, names)?,
                value_of(end, field, min, max, names)?,
            )
        } else {
            let value = value_of(range, field, min, max, names)?;
            // A bare value with a step means "from here to the end, every step".
            match step {
                Some(_) => (value, max),
                None => (value, value),
            }
        };
        if start > end {
            return Err(ScheduleError::BadField {
                field,
                text: part.to_string(),
            });
        }
        let step = step.unwrap_or(1);
        let mut value = start;
        while value <= end {
            mask |= 1 << value;
            value += step;
        }
    }
    Ok((mask, true))
}

fn value_of(
    text: &str,
    field: &'static str,
    min: u32,
    max: u32,
    names: &[(&'static str, u32)],
) -> Result<u32, ScheduleError> {
    let text = text.trim();
    let lowered = text.to_ascii_lowercase();
    if let Some((_, value)) = names.iter().find(|(name, _)| *name == lowered) {
        return Ok(*value);
    }
    let value: i64 = text.parse().map_err(|_| ScheduleError::BadField {
        field,
        text: text.to_string(),
    })?;
    if value < min as i64 || value > max as i64 {
        return Err(ScheduleError::OutOfRange {
            field,
            value,
            min: min as i64,
            max: max as i64,
        });
    }
    Ok(value as u32)
}

/// The next whole minute after `at`.
fn align_minute(at: Millis) -> Millis {
    let minute = 60_000;
    let floor = at.div_euclid(minute) * minute;
    floor + minute
}

fn advance_minute(civil: &mut Civil) {
    let next = time::days_from_civil(
        i64::from(civil.year),
        i64::from(civil.month),
        i64::from(civil.day),
    );
    let mut minute = civil.minute + 1;
    let mut hour = civil.hour;
    let mut days = next;
    if minute > 59 {
        minute = 0;
        hour += 1;
        if hour > 23 {
            hour = 0;
            days += 1;
        }
    }
    let (year, month, day) = time::civil_from_days(days);
    *civil = Civil::new(year, month, day, hour, minute, 0);
}

fn advance_hour(civil: &mut Civil) {
    let mut hour = civil.hour + 1;
    let mut days = time::days_from_civil(
        i64::from(civil.year),
        i64::from(civil.month),
        i64::from(civil.day),
    );
    if hour > 23 {
        hour = 0;
        days += 1;
    }
    let (year, month, day) = time::civil_from_days(days);
    *civil = Civil::new(year, month, day, hour, 0, 0);
}

fn advance_day(civil: &mut Civil) {
    let days = time::days_from_civil(
        i64::from(civil.year),
        i64::from(civil.month),
        i64::from(civil.day),
    ) + 1;
    let (year, month, day) = time::civil_from_days(days);
    *civil = Civil::new(year, month, day, 0, 0, 0);
}

fn advance_month(civil: &mut Civil) {
    let mut month = civil.month + 1;
    let mut year = civil.year;
    if month > 12 {
        month = 1;
        year += 1;
    }
    *civil = Civil::new(year, month, 1, 0, 0, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-03T21:07:00Z — a Tuesday evening, so every expectation below is easy to read.
    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> Millis {
        time::instant_of(Zone::Utc, Civil::new(year, month, day, hour, minute, 0))
    }

    fn cron(spec: &str) -> CronExpr {
        CronExpr::parse(spec).expect("expression should parse")
    }

    #[test]
    fn a_step_expression_lands_on_the_next_boundary() {
        let expression = cron("*/10 * * * *");
        let start = at(2026, 11, 3, 21, 7);
        let next = expression.next_after(Zone::Utc, start).unwrap();
        assert_eq!(next, at(2026, 11, 3, 21, 10));
        // And it does not fire twice for the same minute.
        assert_eq!(
            expression.next_after(Zone::Utc, next).unwrap(),
            at(2026, 11, 3, 21, 20)
        );
    }

    #[test]
    fn a_midnight_expression_rolls_into_the_next_month() {
        let expression = cron("0 0 1 * *");
        let start = at(2026, 1, 31, 23, 59);
        assert_eq!(
            expression.next_after(Zone::Utc, start).unwrap(),
            at(2026, 2, 1, 0, 0)
        );
    }

    #[test]
    fn a_leap_day_expression_waits_for_the_leap_year() {
        let expression = cron("0 0 29 2 *");
        let start = at(2026, 3, 1, 0, 0);
        assert_eq!(
            expression.next_after(Zone::Utc, start).unwrap(),
            at(2028, 2, 29, 0, 0)
        );
    }

    #[test]
    fn a_weekday_name_selects_that_weekday() {
        let expression = cron("0 9 * * mon");
        // Tuesday 2026-11-03 → the next Monday is the 9th.
        let next = expression
            .next_after(Zone::Utc, at(2026, 11, 3, 21, 7))
            .unwrap();
        assert_eq!(next, at(2026, 11, 9, 9, 0));
    }

    #[test]
    fn sunday_may_be_written_as_seven() {
        let as_seven = cron("0 9 * * 7");
        let as_zero = cron("0 9 * * 0");
        assert_eq!(as_seven, as_zero);
        let next = as_seven
            .next_after(Zone::Utc, at(2026, 11, 3, 21, 7))
            .unwrap();
        assert_eq!(next, at(2026, 11, 8, 9, 0));
    }

    #[test]
    fn restricted_day_and_weekday_fire_on_either() {
        // The first of the month, and every Monday: the union, not the intersection.
        let expression = cron("0 9 1 * mon");
        let start = at(2026, 11, 3, 21, 7);
        let first = expression.next_after(Zone::Utc, start).unwrap();
        assert_eq!(first, at(2026, 11, 9, 9, 0), "the next Monday");
        let second = expression.next_after(Zone::Utc, first).unwrap();
        assert_eq!(second, at(2026, 11, 16, 9, 0), "the Monday after");
    }

    #[test]
    fn a_day_of_month_that_does_not_exist_this_month_is_skipped() {
        let expression = cron("0 0 31 * *");
        let start = at(2026, 11, 1, 0, 0);
        assert_eq!(
            expression.next_after(Zone::Utc, start).unwrap(),
            at(2026, 12, 31, 0, 0),
            "November has no 31st"
        );
    }

    #[test]
    fn lists_ranges_and_steps_all_combine() {
        let expression = cron("0,30 9-17 * * *");
        assert!(expression.matches(&Civil::new(2026, 11, 3, 9, 0, 0)));
        assert!(expression.matches(&Civil::new(2026, 11, 3, 17, 30, 0)));
        assert!(!expression.matches(&Civil::new(2026, 11, 3, 18, 0, 0)));
        assert!(!expression.matches(&Civil::new(2026, 11, 3, 9, 15, 0)));
    }

    #[test]
    fn every_shape_an_operator_types_parses() {
        let now = at(2026, 11, 3, 21, 7);

        let once = Schedule::parse("in 30m", Zone::Utc, now).unwrap();
        assert_eq!(
            once,
            Schedule::Once {
                at: now + 30 * 60_000
            }
        );
        assert_eq!(once.next_after(now), Some(now + 30 * 60_000));
        assert_eq!(once.next_after(now + 30 * 60_000), None, "a once is once");

        let every = Schedule::parse("every 10m", Zone::Utc, now).unwrap();
        assert_eq!(every.next_after(now), Some(now + 10 * 60_000));
        assert_eq!(every.next_after(now + 10 * 60_000), Some(now + 20 * 60_000));

        let absolute = Schedule::parse("at 2026-11-03T09:00", Zone::Utc, now).unwrap();
        assert_eq!(
            absolute,
            Schedule::Once {
                at: at(2026, 11, 3, 9, 0)
            }
        );
        assert_eq!(
            absolute.next_after(now),
            None,
            "an instant that has passed does not come back"
        );

        for (spec, kind) in [
            ("@hourly", "cron"),
            ("*/10 * * * *", "cron"),
            ("0 9 * * *", "cron"),
            ("every 2h", "every"),
            ("in 30m", "once"),
        ] {
            assert_eq!(Schedule::parse(spec, Zone::Utc, now).unwrap().kind(), kind);
        }
    }

    #[test]
    fn a_bad_schedule_says_what_is_wrong() {
        assert_eq!(
            Schedule::parse("  ", Zone::Utc, 0),
            Err(ScheduleError::Empty)
        );
        assert_eq!(
            Schedule::parse("* * *", Zone::Utc, 0),
            Err(ScheduleError::UnknownShape("* * *".into()))
        );
        assert_eq!(
            Schedule::parse("61 * * * *", Zone::Utc, 0).unwrap_err(),
            ScheduleError::OutOfRange {
                field: "minute",
                value: 61,
                min: 0,
                max: 59
            }
        );
        assert!(matches!(
            Schedule::parse("every 0m", Zone::Utc, 0),
            Err(ScheduleError::BadDuration(_))
        ));
        assert!(matches!(
            Schedule::parse("at next tuesday", Zone::Utc, 0),
            Err(ScheduleError::BadTimestamp(_))
        ));
        // Six fields is the seconds flavour of cron, which this one deliberately is not.
        assert_eq!(
            Schedule::parse("*/10 * * * * *", Zone::Utc, 0),
            Err(ScheduleError::UnknownShape("*/10 * * * * *".into()))
        );
    }

    #[test]
    fn a_fixed_offset_zone_reads_the_wall_clock_of_that_offset() {
        // 09:00 in +04:00 is 05:00 UTC.
        let expression = cron("0 9 * * *");
        let start = at(2026, 11, 3, 0, 0);
        let next = expression
            .next_after(Zone::Fixed(4 * 60), start)
            .expect("a daily expression always has a next run");
        assert_eq!(next, at(2026, 11, 3, 5, 0));
    }

    #[test]
    fn the_descriptions_are_the_ones_the_operator_reads() {
        let now = at(2026, 11, 3, 21, 7);
        assert_eq!(
            Schedule::parse("every 90m", Zone::Utc, now)
                .unwrap()
                .describe(),
            "every 1h 30m"
        );
        assert_eq!(
            Schedule::parse("0 9 * * *", Zone::Utc, now)
                .unwrap()
                .describe(),
            "cron 0 9 * * * (UTC)"
        );
        assert_eq!(
            Schedule::parse("0 9 * * *", Zone::Local, now)
                .unwrap()
                .describe(),
            "cron 0 9 * * *"
        );
    }
}
