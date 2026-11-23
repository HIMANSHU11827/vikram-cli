//! Durations and timestamps, without a date library.
//!
//! Two jobs: read a human duration (`30m`, `7d`) and print an instant an operator can check
//! against a clock (`2026-11-24T11:30:00Z`). Both are small enough to own, and neither needs the
//! calendar rules a full date crate exists for.

use std::time::{SystemTime, UNIX_EPOCH};

const SECOND: u64 = 1_000;
const MINUTE: u64 = 60 * SECOND;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// The accepted duration forms, quoted by every rejection.
pub const DURATION_FORMS: &str = "`90s`, `30m`, `2h` or `7d`";

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Read `30m` into milliseconds. A bare number is refused: the unit is the whole point, since a
/// duration with no unit is the one an operator misreads by a factor of sixty.
pub fn parse_duration(raw: &str) -> Result<u64, String> {
    let text = raw.trim();
    let (number, unit) = text.split_at(text.len().saturating_sub(1));
    let unit = unit.to_ascii_lowercase();
    let multiplier = match unit.as_str() {
        "s" => SECOND,
        "m" => MINUTE,
        "h" => HOUR,
        "d" => DAY,
        _ => {
            return Err(format!(
                "`{text}` is not a duration; expected a positive whole number followed by s, m, h or d — {DURATION_FORMS}"
            ));
        }
    };
    let value: u64 = number.parse().map_err(|_| {
        format!(
            "`{text}` is not a duration; expected a positive whole number followed by s, m, h or d — {DURATION_FORMS}"
        )
    })?;
    if value == 0 {
        return Err(format!(
            "a duration of zero is not an expiry; give a positive one — {DURATION_FORMS}"
        ));
    }
    value.checked_mul(multiplier).ok_or_else(|| {
        format!("`{text}` is too long to be an expiry; the largest useful unit here is days")
    })
}

/// `7d`, `2h 30m`, `45s` — a duration as a person says it.
pub fn humanize_ms(ms: u64) -> String {
    let days = ms / DAY;
    let hours = (ms % DAY) / HOUR;
    let minutes = (ms % HOUR) / MINUTE;
    let seconds = (ms % MINUTE) / SECOND;
    let mut parts: Vec<String> = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 && days == 0 {
        parts.push(format!("{minutes}m"));
    }
    if seconds > 0 && days == 0 && hours == 0 {
        parts.push(format!("{seconds}s"));
    }
    if !parts.is_empty() {
        parts.join(" ")
    } else if ms == 0 {
        "0s".to_owned()
    } else {
        // Anything under a second still happened; "0s" reads like nothing did.
        "<1s".to_owned()
    }
}

/// `in 30m` or `expired 2d ago`, for a list an operator reads at a glance.
pub fn remaining_label(now: u64, expires: u64) -> String {
    if expires <= now {
        format!("expired {} ago", humanize_ms(now - expires))
    } else {
        format!("in {}", humanize_ms(expires - now))
    }
}

/// The instant, in UTC, to the second: `2026-11-24T11:30:00Z`.
pub fn iso8601(ms: u64) -> String {
    let seconds = ms / SECOND;
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Days since the Unix epoch to a civil date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_units_are_accepted() {
        assert_eq!(parse_duration("90s").unwrap(), 90 * SECOND);
        assert_eq!(parse_duration("30m").unwrap(), 30 * MINUTE);
        assert_eq!(parse_duration("2h").unwrap(), 2 * HOUR);
        assert_eq!(parse_duration("7d").unwrap(), 7 * DAY);
        assert_eq!(parse_duration(" 7D ").unwrap(), 7 * DAY);
    }

    #[test]
    fn anything_else_is_refused_with_the_accepted_forms() {
        for bad in ["7", "0m", "1.5h", "7w", "-3d", "", "d", "30 m"] {
            let err = parse_duration(bad).unwrap_err();
            assert!(
                err.contains("`90s`, `30m`, `2h` or `7d`"),
                "`{bad}` was refused without naming the accepted forms: {err}"
            );
        }
    }

    #[test]
    fn an_instant_is_printed_in_utc() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        // 2026-11-24T11:30:00Z
        assert_eq!(iso8601(1_795_519_800_000), "2026-11-24T11:30:00Z");
        assert_eq!(iso8601(951_782_400_000), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn durations_read_like_a_person_wrote_them() {
        assert_eq!(humanize_ms(30 * MINUTE), "30m");
        assert_eq!(humanize_ms(7 * DAY), "7d");
        assert_eq!(humanize_ms(2 * DAY + 3 * HOUR), "2d 3h");
        assert_eq!(humanize_ms(45 * SECOND), "45s");
        assert_eq!(remaining_label(1000, 1000 + 30 * MINUTE), "in 30m");
        assert_eq!(remaining_label(1000 + 2 * DAY, 1000), "expired 2d ago");
    }
}
