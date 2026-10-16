//! The date a picture is shown and sorted by, and calendar arithmetic on it.
//!
//! Dates here are *wall-clock* seconds: the local date and time on the clock face, counted
//! as if it were UTC. EXIF dates are written that way (`DateTimeOriginal` carries no time
//! zone), so a file's modification time is turned into the same terms with the local time
//! zone ([`local`]), and the two compare and group by calendar month without a time zone
//! getting in the way. Front ends sort and group by [`shown`]: when the picture was taken if
//! the catalog knows (`Catalog::taken_of`, filled in by the pipeline's `date` stage), else
//! when its file was last modified.

use std::time::SystemTime;

/// The date to show and sort a file by: when it was taken (`taken`, wall-clock seconds, from
/// the catalog) if known, else its modification time in local wall-clock seconds.
pub fn shown(taken: Option<i64>, modified: SystemTime) -> i64 {
    taken.unwrap_or_else(|| local(modified))
}

/// `t` as local wall-clock seconds (see the module docs); before 1970 counts down from 0.
pub fn local(t: SystemTime) -> i64 {
    let secs = match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs().min(i64::MAX as u64 / 2) as i64,
        Err(e) => -(e.duration().as_secs().min(i64::MAX as u64 / 2) as i64),
    };
    let time = secs as libc::time_t;
    // SAFETY: `localtime_r` only writes the `tm` we pass it.
    let offset = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() { 0 } else { tm.tm_gmtoff as i64 }
    };
    secs + offset
}

/// An EXIF date (`YYYY:MM:DD HH:MM:SS`; `-` between the date's parts and a missing time are
/// taken too) as wall-clock seconds. `None` for blanks (`0000:00:00 00:00:00`, spaces) and
/// anything that isn't a real date.
pub fn parse_exif(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, time) = text.split_once([' ', 'T']).unwrap_or((text, ""));
    let mut d = date.split([':', '-']).map(|p| p.trim().parse::<i64>().ok());
    let (year, month, day) = (d.next()??, d.next()??, d.next()??);
    if d.next().is_some() || !(1..=9999).contains(&year) || !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let time = time.trim();
    let mut t = [0i64; 3];
    if !time.is_empty() {
        let parts: Vec<&str> = time.split(':').collect();
        if parts.len() > 3 {
            return None;
        }
        for (slot, part) in t.iter_mut().zip(parts) {
            *slot = part.trim().parse().ok()?;
        }
    }
    let [hour, minute, second] = t;
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// The calendar month of wall-clock seconds `t`: (year, month 1..=12).
pub fn month_of(t: i64) -> (i64, u32) {
    let (year, month, _) = civil_from_days(t.div_euclid(86_400));
    (year, month as u32)
}

/// When a month starts, in wall-clock seconds.
pub fn month_start(year: i64, month: u32) -> i64 {
    days_from_civil(year, month as i64, 1) * 86_400
}

/// `2024-05-12 14:30` for wall-clock seconds `t`.
pub fn format(t: i64) -> String {
    let (year, month, day) = civil_from_days(t.div_euclid(86_400));
    let in_day = t.rem_euclid(86_400);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", in_day / 3600, in_day / 60 % 60)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day count since 1970-01-01: (year, month 1..=12, day 1..=31).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn exif_dates_parse_as_wall_clock_seconds() {
        assert_eq!(parse_exif("1970:01:01 00:00:00"), Some(0));
        assert_eq!(parse_exif("1987:06:05 04:03:02"), Some(549_864_182));
        assert_eq!(parse_exif("2024-02-29 23:59:59"), Some(1_709_251_199));
        assert_eq!(parse_exif("2024:02:29"), Some(1_709_164_800), "no time: midnight");
        assert_eq!(parse_exif(" 1965:12:24 18:00:00\0 "), None, "a NUL isn't a time");
        assert_eq!(parse_exif("1965:12:24 18:00:00"), Some(-126_856_800), "before 1970");
        for blank in ["0000:00:00 00:00:00", "    :  :     :  :  ", "", "2023:02:29 10:00:00", "2023:13:01 10:00:00", "2023:01:01 24:00:00", "junk"] {
            assert_eq!(parse_exif(blank), None, "{blank:?}");
        }
    }

    #[test]
    fn months() {
        assert_eq!(month_of(0), (1970, 1));
        assert_eq!(month_of(-1), (1969, 12));
        assert_eq!(month_of(parse_exif("2026:10:31 23:59:59").unwrap()), (2026, 10));
        assert_eq!(month_of(parse_exif("2026:11:01 00:00:00").unwrap()), (2026, 11));
        assert_eq!(month_start(2026, 11), parse_exif("2026:11:01 00:00:00").unwrap());
        assert_eq!(month_start(1900, 3), parse_exif("1900:03:01").unwrap());
        // Every month of four centuries round-trips.
        for year in 1800..2200 {
            for month in 1..=12 {
                let start = month_start(year, month);
                assert_eq!(month_of(start), (year, month));
                assert_eq!(month_of(start - 1), if month == 1 { (year - 1, 12) } else { (year, month - 1) });
            }
        }
        assert_eq!(format(parse_exif("1987:06:05 04:03:02").unwrap()), "1987-06-05 04:03");
    }

    #[test]
    fn shown_prefers_the_date_taken() {
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(shown(Some(42), modified), 42);
        let local = shown(None, modified);
        assert!((local - 1_000_000).abs() <= 14 * 3600, "within a time zone of UTC");
        assert!(super::local(SystemTime::UNIX_EPOCH - Duration::from_secs(86_400)) < 0);
    }
}
