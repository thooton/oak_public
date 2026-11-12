//! Dates and times as OAK shows them: the local zone's offset, the civil
//! calendar, and the ways times are written out. Times are ms since 1970.

use crate::now_ms;

pub const DAY_MS: i64 = 86_400_000;

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// How far local time is ahead of UTC, daylight saving included.
pub fn local_offset_ms() -> i64 {
    i64::from(chrono::Local::now().offset().local_minus_utc()) * 1000
}

/// The offset, in ms, of a zone `offset_s` seconds ahead of UTC, or this
/// machine's when it is None.
fn offset(offset_s: Option<i64>) -> i64 {
    offset_s.map_or_else(local_offset_ms, |s| s * 1000)
}

/// Days since 1970 to (year, month, day) (Howard Hinnant's algorithm).
fn civil_date(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if month <= 2 { 1 } else { 0 }, month, day)
}

/// (year, month, day) to days since 1970: `civil_date` the other way.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// "2026-10-27T12:39:59.525262+00:00" (or ending in Z) as milliseconds
/// since 1970.
pub fn parse_time(s: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let days = days_from_civil(y, mo, d);
    let mut ms = ((days * 24 + h) * 60 + mi) * 60_000 + sec * 1000;
    let rest = &s[19..];
    let rest = match rest.strip_prefix('.') {
        Some(frac) => {
            let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
            ms += format!("{:0<3}", &digits[..digits.len().min(3)]).parse::<i64>().unwrap_or(0);
            &frac[digits.len()..]
        }
        None => rest,
    };
    if let Some(sign) = rest.chars().next().filter(|c| *c == '+' || *c == '-') {
        let off = (rest.get(1..3)?.parse::<i64>().ok()? * 60 + rest.get(4..6)?.parse::<i64>().ok()?) * 60_000;
        ms -= if sign == '+' { off } else { -off };
    }
    Some(ms)
}

/// The local time of `ms` ("14:05"), with the weekday when it isn't today.
pub fn clock(ms: i64) -> String {
    clock_in(ms, None)
}

/// `clock` in a zone `offset_s` seconds ahead of UTC (this machine's when
/// it is None): for times shown in another zone.
pub fn clock_in(ms: i64, offset_s: Option<i64>) -> String {
    let off = offset(offset_s);
    let (at, now) = (ms + off, now_ms() + off);
    let day = at.div_euclid(DAY_MS);
    let min = at.rem_euclid(DAY_MS) / 60_000;
    let hm = format!("{:02}:{:02}", min / 60, min % 60);
    if day == now.div_euclid(DAY_MS) {
        hm
    } else {
        // 1970-01-01 was a Thursday.
        format!("{} {hm}", ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][day.rem_euclid(7) as usize])
    }
}

/// The local date of `ms` as mm/dd/yy.
pub fn mdy(ms: i64) -> String {
    mdy_in(ms, None)
}

/// `mdy` in a zone `offset_s` seconds ahead of UTC, as for `clock_in`.
pub fn mdy_in(ms: i64, offset_s: Option<i64>) -> String {
    let (year, month, day) = civil_date((ms + offset(offset_s)).div_euclid(DAY_MS));
    format!("{month:02}/{day:02}/{:02}", year % 100)
}

/// Today's date in UTC, as 2026-10-31.
pub fn today_utc() -> String {
    let (y, m, d) = civil_date(now_ms().div_euclid(DAY_MS));
    format!("{y}-{m:02}-{d:02}")
}

/// "now", "5m ago", "3h ago", "2d ago".
pub fn ago(ms: i64) -> String {
    let s = (now_ms() - ms).max(0) / 1000;
    match s {
        0..60 => "now".into(),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// "1h 29m", "6d 2h", "12m": how long until `at`.
pub fn until(at: i64) -> String {
    let m = ((at - now_ms()).max(0) + 59_999) / 60_000;
    match m {
        0..60 => format!("{m}m"),
        60..1440 => format!("{}h {}m", m / 60, m % 60),
        _ => format!("{}d {}h", m / 1440, m % 1440 / 60),
    }
}

/// When `ms` was, seen from `now`, in words: "just now", "5 minutes ago",
/// "yesterday", "3 days ago", then the date ("Oct 3"; "Oct 3, 2025" in
/// another year). `off`: how far local time is ahead of UTC.
pub fn time_ago(ms: i64, now: i64, off: i64) -> String {
    let s = (now - ms).max(0) / 1000;
    let ago = |n: i64, unit: &str| format!("{n} {unit}{} ago", crate::plural(n));
    if s < 60 {
        return "just now".into();
    }
    if s < 3600 {
        return ago(s / 60, "minute");
    }
    if s < 86_400 {
        return ago(s / 3600, "hour");
    }
    let (day, today) = ((ms + off).div_euclid(DAY_MS), (now + off).div_euclid(DAY_MS));
    match today - day {
        ..=1 => "yesterday".into(),
        n @ 2..=6 => format!("{n} days ago"),
        _ => {
            let ((y, m, d), (this_year, _, _)) = (civil_date(day), civil_date(today));
            let md = format!("{} {d}", MONTHS[(m - 1) as usize]);
            if y == this_year { md } else { format!("{md}, {y}") }
        }
    }
}

/// The full date and time of `ms` in a zone `off` ms ahead of UTC:
/// "Oct 28, 2026, 8:34 AM".
pub fn full_time(ms: i64, off: i64) -> String {
    let (y, m, d) = civil_date((ms + off).div_euclid(DAY_MS));
    format!("{} {d}, {y}, {}", MONTHS[(m - 1) as usize], clock12(ms, off))
}

/// The time of day of `ms` in a zone `off` ms ahead of UTC: "8:34 AM".
pub fn clock12(ms: i64, off: i64) -> String {
    let min = (ms + off).rem_euclid(DAY_MS) / 60_000;
    let (h, mm) = (min / 60, min % 60);
    let h12 = if h % 12 == 0 { 12 } else { h % 12 };
    format!("{h12}:{mm:02} {}", if h < 12 { "AM" } else { "PM" })
}

/// The day and time of `ms`, seen from `now`, as a text conversation heads
/// a stretch of messages: "Today, 8:34 AM", "Yesterday, 8:34 AM", "Oct 3,
/// 8:34 AM", or "Oct 3, 2025, 8:34 AM" in another year.
pub fn day_clock(ms: i64, now: i64, off: i64) -> String {
    let (day, today) = ((ms + off).div_euclid(DAY_MS), (now + off).div_euclid(DAY_MS));
    let ((y, m, d), (this_year, _, _)) = (civil_date(day), civil_date(today));
    let date = match today - day {
        0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        _ if y == this_year => format!("{} {d}", MONTHS[(m - 1) as usize]),
        _ => format!("{} {d}, {y}", MONTHS[(m - 1) as usize]),
    };
    format!("{date}, {}", clock12(ms, off))
}

/// The local month and day of `ms`: "Oct 24".
pub fn month_day(ms: i64) -> String {
    let (_, m, d) = civil_date((ms + local_offset_ms()).div_euclid(DAY_MS));
    format!("{} {d}", MONTHS[(m - 1) as usize])
}

/// The local hour and minute of `ms`: "14:00".
pub fn hour_min(ms: i64) -> String {
    let m = (ms + local_offset_ms()).rem_euclid(DAY_MS) / 60_000;
    format!("{:02}:{:02}", m / 60, m % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_times_in_words_and_in_full() {
        let (min, hour, day) = (60_000, 3_600_000, 86_400_000);
        // 2026-10-31 12:00 UTC, read in UTC.
        let now = 1_793_448_000_000;
        assert_eq!(time_ago(now - 5_000, now, 0), "just now");
        assert_eq!(time_ago(now + 5_000, now, 0), "just now");
        assert_eq!(time_ago(now - min, now, 0), "1 minute ago");
        assert_eq!(time_ago(now - 59 * min, now, 0), "59 minutes ago");
        assert_eq!(time_ago(now - 3 * hour, now, 0), "3 hours ago");
        assert_eq!(time_ago(now - 25 * hour, now, 0), "yesterday");
        assert_eq!(time_ago(now - 3 * day, now, 0), "3 days ago");
        assert_eq!(time_ago(now - 6 * day, now, 0), "6 days ago");
        assert_eq!(time_ago(now - 7 * day, now, 0), "Oct 24");
        assert_eq!(time_ago(now - 400 * day, now, 0), "Sep 26, 2025");
        // Days go by the local calendar: 36 hours back, 23:00 two days
        // ago in UTC, is yesterday 11 hours ahead.
        assert_eq!(time_ago(now - 36 * hour, now, 0), "yesterday");
        assert_eq!(time_ago(now - 37 * hour, now, 0), "2 days ago");
        assert_eq!(time_ago(now - 37 * hour, now, 11 * hour), "yesterday");
        assert_eq!(full_time(now, 0), "Oct 31, 2026, 12:00 PM");
        assert_eq!(full_time(now - 3 * day - 3 * hour - 26 * min, -hour), "Oct 28, 2026, 7:34 AM");
        assert_eq!(full_time(now - 12 * hour, 0), "Oct 31, 2026, 12:00 AM");
        assert_eq!(full_time(951_825_600_000, 0), "Feb 29, 2000, 12:00 PM");
        assert_eq!(day_clock(now - 3 * hour - 26 * min, now, 0), "Today, 8:34 AM");
        assert_eq!(day_clock(now - 13 * hour, now, 0), "Yesterday, 11:00 PM");
        assert_eq!(day_clock(now - 7 * day, now, 0), "Oct 24, 12:00 PM");
        assert_eq!(day_clock(now - 400 * day, now, 0), "Sep 26, 2025, 12:00 PM");
    }
}
