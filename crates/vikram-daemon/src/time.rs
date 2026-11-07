//! Calendar and clock arithmetic for the scheduler.
//!
//! No date crate is available in this workspace, so the day arithmetic is Howard Hinnant's
//! `days_from_civil` / `civil_from_days` pair: exact for any proleptic Gregorian date and needing
//! no lookup tables. Zones are resolved through the C library rather than reimplementing tzdata, so
//! `TZ` means here exactly what it means in a shell.
//!
//! Every conversion below takes a number, never text, so none of them can panic on operator input;
//! the parsing entry points return `Option` and reject what they cannot read.
//!
//! Named zones (`America/New_York`) are read straight out of the system tz database, the compiled
//! `TZif` files under `/usr/share/zoneinfo` (or `$TZDIR`). That file format carries the transitions
//! themselves, so a 09:00 job stays at 09:00 across both daylight-saving changes instead of being
//! pinned to whichever offset was in force when it was created. A name with no file on this machine
//! is refused rather than approximated.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// A point in time, in milliseconds since the Unix epoch.
///
/// Milliseconds because that is what the rest of vikram carries in its `_ms` fields; a scheduler
/// that switched units at a boundary would eventually add seconds to milliseconds.
pub type Millis = i64;

const MS_PER_SEC: Millis = 1_000;
const MS_PER_MIN: Millis = 60_000;
const MS_PER_HOUR: Millis = 3_600_000;
const SECS_PER_DAY: i64 = 86_400;

unsafe extern "C" {
    fn tzset();
}

static TZ_INIT: Once = Once::new();

/// A wall clock reading with no zone attached.
///
/// The fields are public because callers do arithmetic on them (`year + 1`, `day + 7`) and then
/// hand the result back through [`days_from_civil`]; hiding them behind getters would only add
/// ceremony. Nothing here range-checks: a `month` of 13 is normalised by the day arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Civil {
    pub const fn new(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> Self {
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        }
    }

    /// Day of the week, Monday = 0 through Sunday = 6.
    ///
    /// Note this is NOT the cron numbering, which is Sunday = 0. The conversion belongs in the
    /// cron expression, which is the only place that cares, rather than in the calendar.
    pub fn weekday(self) -> u32 {
        let days = days_from_civil(
            i64::from(self.year),
            i64::from(self.month),
            i64::from(self.day),
        );
        // 1970-01-01 was a Thursday, which is 3 in this numbering.
        (days + 3).rem_euclid(7) as u32
    }
}

impl fmt::Display for Civil {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// A UTC offset, either named or numeric.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Zone {
    /// The machine's own zone, read from the C library and therefore from `TZ`.
    Local,
    /// UTC.
    Utc,
    /// A fixed offset in minutes east of UTC; `Fixed(330)` is `+05:30`.
    Fixed(i32),
    /// A named zone from the system tz database; `Named("America/New_York")` keeps its own history
    /// of daylight-saving changes.
    Named(String),
}

impl Zone {
    /// Read a zone from operator text: `utc`/`z`/`gmt`, `local`/`system`, an empty string, a
    /// numeric offset as `+05:30`, `-08:00`, `+0000`, `+05`, or a name the system tz database
    /// holds (`America/New_York`, `Europe/Berlin`, `Asia/Kolkata`).
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty()
            || text.eq_ignore_ascii_case("local")
            || text.eq_ignore_ascii_case("system")
        {
            return Some(Zone::Local);
        }
        if text.eq_ignore_ascii_case("utc")
            || text.eq_ignore_ascii_case("z")
            || text.eq_ignore_ascii_case("gmt")
        {
            return Some(Zone::Utc);
        }
        let bytes = text.as_bytes();
        let sign = match bytes.first()? {
            b'+' => 1,
            b'-' => -1,
            // Not an offset, so it has to be a name the machine actually knows.
            _ => return zone_file(text).map(|_| Zone::Named(text.to_string())),
        };
        let rest = &text[1..];
        let (hours, minutes) = match rest.split_once(':') {
            Some((hours, minutes)) => {
                if minutes.len() != 2 {
                    return None;
                }
                (hours, minutes)
            }
            None if rest.len() == 4 => rest.split_at(2),
            None => (rest, "0"),
        };
        let hours: i32 = decimal(hours)?.try_into().ok()?;
        let minutes: i32 = decimal(minutes)?.try_into().ok()?;
        if hours > 23 || minutes > 59 {
            return None;
        }
        Some(Zone::Fixed(sign * (hours * 60 + minutes)))
    }

    /// The offset in seconds east of UTC that this zone was at a given instant.
    fn offset_seconds(&self, at_ms: Millis) -> i64 {
        match self {
            Zone::Utc => 0,
            Zone::Fixed(minutes) => i64::from(*minutes) * 60,
            Zone::Local => local_offset_seconds(at_ms.div_euclid(MS_PER_SEC)),
            Zone::Named(name) => zone_file(name)
                .map(|file| i64::from(file.offset_at(at_ms.div_euclid(MS_PER_SEC))))
                .unwrap_or(0),
        }
    }
}

impl fmt::Display for Zone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Zone::Utc => f.write_str("UTC"),
            Zone::Local => f.write_str("local"),
            Zone::Named(name) => f.write_str(name),
            Zone::Fixed(minutes) => {
                let sign = if *minutes < 0 { '-' } else { '+' };
                let minutes = minutes.abs();
                write!(f, "{sign}{:02}:{:02}", minutes / 60, minutes % 60)
            }
        }
    }
}

/// One compiled zone: the offsets the tz database says were in force, oldest first.
struct ZoneFile {
    /// Instants (epoch seconds) at which the offset changed.
    transitions: Vec<i64>,
    /// Offset in seconds east of UTC after the transition at the same index.
    offsets: Vec<i32>,
    /// The offset before the first transition.
    first: i32,
}

impl ZoneFile {
    fn offset_at(&self, secs: i64) -> i32 {
        match self.transitions.partition_point(|&at| at <= secs) {
            0 => self.first,
            used => self.offsets[used - 1],
        }
    }
}

/// Read a zone out of the system tz database, once per name per process.
fn zone_file(name: &str) -> Option<Arc<ZoneFile>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<ZoneFile>>>>> = OnceLock::new();
    if !is_zone_name(name) {
        return None;
    }
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().expect("zone cache").get(name) {
        return hit.clone();
    }
    let root = std::env::var("TZDIR").unwrap_or_else(|_| "/usr/share/zoneinfo".to_string());
    let loaded = std::fs::read(Path::new(&root).join(name))
        .ok()
        .and_then(|bytes| parse_zone_file(&bytes))
        .map(Arc::new);
    cache
        .lock()
        .expect("zone cache")
        .insert(name.to_string(), loaded.clone());
    loaded
}

/// A zone name may only reach inside the database, never above it.
fn is_zone_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("..")
        && !name.starts_with('/')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'+'))
}

/// Bytes that follow the 44-byte header of a `TZif` block.
fn block_end(counts: [usize; 6], width: usize) -> usize {
    let [isut, isstd, leap, time, kind, chars] = counts;
    time * width + time + kind * 6 + chars + leap * (width + 4) + isstd + isut
}

fn counts(bytes: &[u8], header: usize) -> Option<[usize; 6]> {
    let raw = bytes.get(header + 20..header + 44)?;
    if bytes.get(header..header + 4)? != b"TZif" {
        return None;
    }
    let mut out = [0usize; 6];
    for (slot, chunk) in out.iter_mut().zip(raw.as_chunks::<4>().0) {
        *slot = u32::from_be_bytes(*chunk) as usize;
    }
    Some(out)
}

/// Parse a `TZif` file: the transitions and the offset each one switches to.
fn parse_zone_file(bytes: &[u8]) -> Option<ZoneFile> {
    let mut header = 0;
    let mut width = 4;
    if bytes.get(4)? != &0 {
        // A version-2 file repeats itself: a 32-bit block for readers that predate it, then the
        // real one with 64-bit times. Only the second is worth reading.
        let next = 44 + block_end(counts(bytes, 0)?, 4);
        if bytes.get(next..next + 4)? == b"TZif" {
            header = next;
            width = 8;
        }
    }
    let shape = counts(bytes, header)?;
    let body = header + 44;
    if bytes.len() < body + block_end(shape, width) {
        return None;
    }
    let [_, _, _, time_count, kind_count, _] = shape;
    let transitions_raw = bytes.get(body..body + time_count * width)?;
    let indices = bytes.get(body + time_count * width..body + time_count * width + time_count)?;
    let kinds_raw = bytes.get(
        body + time_count * width + time_count
            ..body + time_count * width + time_count + kind_count * 6,
    )?;

    let mut transitions = Vec::with_capacity(time_count);
    for chunk in transitions_raw.chunks_exact(width) {
        transitions.push(if width == 8 {
            i64::from_be_bytes(chunk.try_into().ok()?)
        } else {
            i64::from(i32::from_be_bytes(chunk.try_into().ok()?))
        });
    }
    let mut kinds = Vec::with_capacity(kind_count);
    for chunk in kinds_raw.as_chunks::<6>().0 {
        kinds.push((
            i32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            chunk[4] != 0,
        ));
    }
    // Before the first transition the tz database's own rule applies: the first zone that is not
    // daylight time, or the first zone if they all are.
    let first = kinds
        .iter()
        .find(|(_, daylight)| !daylight)
        .or_else(|| kinds.first())
        .map(|(offset, _)| *offset)?;
    let mut offsets = Vec::with_capacity(time_count);
    for index in indices {
        offsets.push(
            kinds
                .get(usize::from(*index))
                .map(|(offset, _)| *offset)
                .unwrap_or(first),
        );
    }
    Some(ZoneFile {
        transitions,
        offsets,
        first,
    })
}

/// Load the C library's idea of the local zone once, so a `TZ` change made after start is not
/// silently picked up halfway through a schedule.
fn ensure_tz() {
    TZ_INIT.call_once(|| unsafe { tzset() });
}

#[cfg(unix)]
fn local_offset_seconds(at_secs: i64) -> i64 {
    ensure_tz();
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let secs: libc::time_t = at_secs as libc::time_t;
    // SAFETY: `tm` is a valid, zeroed `libc::tm` and `secs` is a plain integer.
    let ok = unsafe { libc::localtime_r(&secs, &mut tm) };
    if ok.is_null() {
        return 0;
    }
    tm.tm_gmtoff as i64
}

#[cfg(not(unix))]
fn local_offset_seconds(_at_secs: i64) -> i64 {
    // Without the C library there is no zone database to consult. UTC is wrong for a machine that
    // is not on UTC, but a made-up offset would be wrong AND invisible; this one is documented.
    0
}

/// Now, in milliseconds since the epoch.
pub fn now_ms() -> Millis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as Millis)
        .unwrap_or(0)
}

/// Days since 1970-01-01 for a proleptic Gregorian date. Out-of-range months and days are
/// normalised rather than rejected, so `2026-13-01` is 2027-01-01.
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The proleptic Gregorian date of a day count since 1970-01-01.
pub fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = month_index + if month_index < 10 { 3 } else { -9 };
    (
        (year + i64::from(month <= 2)) as i32,
        month as u32,
        day as u32,
    )
}

/// Whether `year` has a 29th of February.
pub fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// How many days a month has. Zero for a month that does not exist, so a caller comparing against
/// it can never conclude that a 31st is real in a 30 day month.
pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// The wall clock in UTC at an instant.
pub fn civil_utc(at_ms: Millis) -> Civil {
    let secs = at_ms.div_euclid(MS_PER_SEC);
    let days = secs.div_euclid(SECS_PER_DAY);
    let of_day = secs.rem_euclid(SECS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    Civil {
        year,
        month,
        day,
        hour: (of_day / 3_600) as u32,
        minute: ((of_day % 3_600) / 60) as u32,
        second: (of_day % 60) as u32,
    }
}

/// The wall clock in `zone` at an instant.
pub fn civil_in(zone: Zone, at_ms: Millis) -> Civil {
    match zone {
        Zone::Utc => civil_utc(at_ms),
        _ => civil_utc(at_ms + zone.offset_seconds(at_ms) * MS_PER_SEC),
    }
}

/// The instant a wall clock reading names in `zone`.
///
/// A reading a zone skips (the hour that daylight saving removes) is pushed past the gap, and a
/// reading it repeats resolves to the first occurrence: both keep a daily job running once.
pub fn instant_of(zone: Zone, civil: Civil) -> Millis {
    let days = days_from_civil(
        i64::from(civil.year),
        i64::from(civil.month),
        i64::from(civil.day),
    );
    let local = days * SECS_PER_DAY
        + i64::from(civil.hour) * 3_600
        + i64::from(civil.minute) * 60
        + i64::from(civil.second);
    let local_ms = local * MS_PER_SEC;
    if matches!(zone, Zone::Utc) {
        return local_ms;
    }
    // The offset that applies depends on the instant, which is what we are solving for, so resolve
    // it twice: once with the offset of the reading taken as UTC, once with the offset at the
    // result. A reading inside a spring-forward gap lands after the gap.
    let first = zone.offset_seconds(local_ms);
    let guess = local_ms - first * MS_PER_SEC;
    let second = zone.offset_seconds(guess);
    local_ms - second * MS_PER_SEC
}

/// `YYYY-MM-DD HH:MM` in `zone`.
pub fn format_ms(zone: Zone, at_ms: Millis) -> String {
    let civil = civil_in(zone, at_ms);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        civil.year, civil.month, civil.day, civil.hour, civil.minute
    )
}

/// A span of time the way an operator would say it: `42s`, `1h 30m`, `2d 4h`.
pub fn humanize_ms(ms: Millis) -> String {
    let ms = ms.abs();
    if ms < MS_PER_MIN {
        return format!("{}s", ms / MS_PER_SEC);
    }
    if ms < MS_PER_HOUR {
        let minutes = ms / MS_PER_MIN;
        let seconds = (ms % MS_PER_MIN) / MS_PER_SEC;
        return if seconds == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m {seconds}s")
        };
    }
    if ms < 86_400 * MS_PER_SEC {
        let hours = ms / MS_PER_HOUR;
        let minutes = (ms % MS_PER_HOUR) / MS_PER_MIN;
        return if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {minutes}m")
        };
    }
    let days = ms / (86_400 * MS_PER_SEC);
    let hours = (ms % (86_400 * MS_PER_SEC)) / MS_PER_HOUR;
    if hours == 0 {
        format!("{days}d")
    } else {
        format!("{days}d {hours}h")
    }
}

/// Read a duration: `30s`, `10m`, `2h`, `1d`, `1w`, compounds as `1h30m`.
///
/// A bare number is minutes, because that is how a person types "remind me in 30". Zero is
/// rejected: a zero interval would ask for a job that is always due.
pub fn parse_duration(text: &str) -> Option<Millis> {
    let text = text.trim().to_ascii_lowercase();
    if text.is_empty() {
        return None;
    }
    if let Some(number) = decimal(&text) {
        let minutes = i64::from(number);
        return if minutes == 0 {
            None
        } else {
            Some(minutes * MS_PER_MIN)
        };
    }
    let mut total = 0i64;
    let mut number = String::new();
    let mut unit: Option<char> = None;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            // `1h30m` is two numbers and two units; a unit that has already been paid for starts
            // a fresh number rather than ending the parse.
            number.push(ch);
            unit = None;
            continue;
        }
        let value: i64 = number.parse().ok()?;
        let factor = match ch {
            's' => MS_PER_SEC,
            'm' => MS_PER_MIN,
            'h' => MS_PER_HOUR,
            'd' => 86_400 * MS_PER_SEC,
            'w' => 7 * 86_400 * MS_PER_SEC,
            _ => return None,
        };
        total += value * factor;
        number.clear();
        unit = Some(ch);
    }
    if !number.is_empty() || unit.is_none() || total == 0 {
        return None;
    }
    Some(total)
}

/// Read a wall clock time: `2026-11-03T09:00`, `2026-11-03 09:00:30`, `2026-11-03`.
///
/// A trailing `Z` is tolerated and does not change the zone: the caller already said which zone
/// the reading is in, and honouring a stray `Z` would move the job.
pub fn parse_timestamp(text: &str, zone: Zone) -> Option<Millis> {
    let text = text.trim().trim_end_matches(['Z', 'z']);
    let (date, clock) = match text.split_once(['T', 't', ' ']) {
        Some((date, clock)) => (date, clock.trim()),
        None => (text, "00:00"),
    };
    let mut date_parts = date.split('-');
    let year: i32 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    if day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let mut clock_parts = clock.split(':');
    let hour: u32 = clock_parts.next()?.parse().ok()?;
    let minute: u32 = clock_parts.next().unwrap_or("0").parse().ok()?;
    let second: u32 = clock_parts.next().unwrap_or("0").parse().ok()?;
    if clock_parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(instant_of(
        zone,
        Civil::new(year, month, day, hour, minute, second),
    ))
}

/// Digits to a number, rejecting anything that is not digits and anything too long to be a time.
fn decimal(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_is_the_nineteenth_of_no_interest() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_utc(0), Civil::new(1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn the_calendar_round_trips_over_centuries() {
        for days in [-1_000_000i64, -1, 0, 1, 10_000, 20_000, 365_242, 400_000] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(
                days_from_civil(i64::from(year), i64::from(month), i64::from(day)),
                days,
                "{year}-{month}-{day}"
            );
        }
        assert_eq!(
            civil_from_days(days_from_civil(1_000_000, 7, 4)),
            (1_000_000, 7, 4)
        );
    }

    #[test]
    fn weekdays_are_not_guessed() {
        assert_eq!(Civil::new(1970, 1, 1, 0, 0, 0).weekday(), 3, "a Thursday");
        assert_eq!(Civil::new(2026, 11, 3, 0, 0, 0).weekday(), 1, "a Tuesday");
        assert_eq!(Civil::new(2026, 11, 8, 0, 0, 0).weekday(), 6, "a Sunday");
    }

    #[test]
    fn an_instant_renders_in_the_zone_asked_for() {
        let instant = 1_785_000_000_000; // 2026-07-25T17:20:00Z
        assert_eq!(format_ms(Zone::Utc, instant), "2026-07-25 17:20");
        assert_eq!(format_ms(Zone::Fixed(240), instant), "2026-07-25 21:20");
        assert_eq!(
            instant_of(Zone::Fixed(240), civil_in(Zone::Fixed(240), instant)),
            instant
        );
    }

    #[test]
    fn zone_text_round_trips() {
        assert_eq!(Zone::parse(""), Some(Zone::Local));
        assert_eq!(Zone::parse("local"), Some(Zone::Local));
        assert_eq!(Zone::parse("UTC"), Some(Zone::Utc));
        assert_eq!(Zone::parse("+05:30"), Some(Zone::Fixed(330)));
        assert_eq!(Zone::parse("-0800"), Some(Zone::Fixed(-480)));
        assert_eq!(Zone::parse("+05"), Some(Zone::Fixed(300)));
        assert_eq!(Zone::parse("+25:00"), None);
        assert_eq!(Zone::Fixed(330).to_string(), "+05:30");
        assert_eq!(Zone::Utc.to_string(), "UTC");
    }

    #[test]
    fn a_zone_that_is_only_a_name_is_read_out_of_the_tz_database() {
        // A name the machine does not know is refused rather than turned into an offset, and a name
        // may never climb out of the database directory.
        assert_eq!(Zone::parse("Nowhere/Nothing"), None);
        assert_eq!(Zone::parse("../../etc/passwd"), None);
        assert_eq!(Zone::parse("/etc/passwd"), None);
        let Some(berlin) = Zone::parse("Europe/Berlin") else {
            // A machine without tzdata cannot answer this; skipping is honest, guessing is not.
            return;
        };
        assert_eq!(berlin.to_string(), "Europe/Berlin");
        // Berlin is +01:00 in January and +02:00 in July, which is the whole point of the name.
        let winter = instant_of(berlin.clone(), Civil::new(2026, 1, 15, 12, 0, 0));
        let summer = instant_of(berlin.clone(), Civil::new(2026, 7, 15, 12, 0, 0));
        assert_eq!(format_ms(Zone::Utc, winter), "2026-01-15 11:00");
        assert_eq!(format_ms(Zone::Utc, summer), "2026-07-15 10:00");
        assert_eq!(civil_in(berlin, summer), Civil::new(2026, 7, 15, 12, 0, 0));
    }

    #[test]
    fn the_local_zone_reads_its_own_clock() {
        let instant = now_ms();
        let civil = civil_in(Zone::Local, instant);
        let back = instant_of(Zone::Local, civil);
        assert!(
            (back - instant).abs() < MS_PER_MIN,
            "a local reading must resolve to the instant it came from, got {} vs {}",
            format_ms(Zone::Local, back),
            format_ms(Zone::Local, instant)
        );
    }

    #[test]
    fn durations_are_read_the_way_they_are_typed() {
        assert_eq!(parse_duration("30s"), Some(30_000));
        assert_eq!(parse_duration("10m"), Some(600_000));
        assert_eq!(parse_duration("2h"), Some(7_200_000));
        assert_eq!(parse_duration("1d"), Some(86_400_000));
        assert_eq!(parse_duration("1w"), Some(604_800_000));
        assert_eq!(parse_duration("1h30m"), Some(5_400_000));
        assert_eq!(
            parse_duration("30"),
            Some(1_800_000),
            "a bare number is minutes"
        );
        assert_eq!(parse_duration("0m"), None, "a zero interval is never due");
        assert_eq!(parse_duration("10x"), None);
        assert_eq!(parse_duration("1h30"), None);
        assert_eq!(parse_duration(""), None);
    }

    #[test]
    fn timestamps_are_read_in_the_zone_they_are_given() {
        let utc = parse_timestamp("2026-11-03T09:00", Zone::Utc).unwrap();
        assert_eq!(format_ms(Zone::Utc, utc), "2026-11-03 09:00");
        // 09:00 in +02:00 is 07:00 UTC, so it is two hours EARLIER than the same reading in UTC.
        let plus_two = parse_timestamp("2026-11-03 09:00", Zone::Fixed(120)).unwrap();
        assert_eq!(utc - plus_two, 2 * MS_PER_HOUR);
        assert_eq!(
            format_ms(Zone::Utc, parse_timestamp("2026-11-03", Zone::Utc).unwrap()),
            "2026-11-03 00:00"
        );
        assert_eq!(parse_timestamp("2026-13-01", Zone::Utc), None);
        assert_eq!(parse_timestamp("2026-02-30", Zone::Utc), None);
        assert_eq!(parse_timestamp("next tuesday", Zone::Utc), None);
        assert_eq!(parse_timestamp("2026-11-03T25:00", Zone::Utc), None);
    }

    #[test]
    fn spans_read_as_an_operator_would_say_them() {
        assert_eq!(humanize_ms(42_000), "42s");
        assert_eq!(humanize_ms(5_400_000), "1h 30m");
        assert_eq!(humanize_ms(7_200_000), "2h");
        assert_eq!(humanize_ms(93_600_000), "1d 2h");
    }
}
