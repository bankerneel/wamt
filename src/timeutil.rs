//! Time helpers: RFC 3339 parsing, local calendar days, the machine's UTC offset.
//!
//! Everything inside wamt is epoch milliseconds (UTC). "Days" are local
//! calendar days numbered from 1970-01-01, using a fixed UTC offset in minutes.

use std::time::{SystemTime, UNIX_EPOCH};

pub const MS_PER_MIN: i64 = 60_000;
pub const MS_PER_HOUR: i64 = 3_600_000;
pub const MS_PER_DAY: i64 = 86_400_000;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Days since 1970-01-01 for a civil date (proleptic Gregorian).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((if m <= 2 { y + 1 } else { y }) as i32, m, d)
}

fn digits(b: &[u8], from: usize, to: usize) -> Option<i64> {
    if to > b.len() {
        return None;
    }
    let mut n = 0i64;
    for &c in &b[from..to] {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n * 10 + (c - b'0') as i64;
    }
    Some(n)
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.fraction](Z|+HH:MM|-HH:MM)` into epoch ms.
/// A missing zone is read as UTC.
pub fn parse_ts_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d) = (digits(b, 0, 4)?, digits(b, 5, 7)?, digits(b, 8, 10)?);
    let (h, mi, se) = (digits(b, 11, 13)?, digits(b, 14, 16)?, digits(b, 17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut i = 19;
    let mut ms = 0i64;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let start = i;
        let mut n = 0;
        while i < b.len() && b[i].is_ascii_digit() {
            if n < 3 {
                ms = ms * 10 + (b[i] - b'0') as i64;
                n += 1;
            }
            i += 1;
        }
        if i == start {
            return None;
        }
        while n < 3 {
            ms *= 10;
            n += 1;
        }
    }
    let off_min = if i < b.len() {
        match b[i] {
            b'Z' | b'z' => 0,
            b'+' | b'-' => {
                let o = digits(b, i + 1, i + 3)? * 60 + digits(b, i + 4, i + 6)?;
                if b[i] == b'-' {
                    -o
                } else {
                    o
                }
            }
            _ => return None,
        }
    } else {
        0
    };
    let days = days_from_civil(y, mo, d);
    Some(days * MS_PER_DAY + h * MS_PER_HOUR + mi * MS_PER_MIN + se * 1000 + ms - off_min * MS_PER_MIN)
}

/// Local calendar day number of an instant.
pub fn day_of(ts_ms: i64, off_min: i32) -> i32 {
    (ts_ms + off_min as i64 * MS_PER_MIN).div_euclid(MS_PER_DAY) as i32
}

/// The instant local day `day` begins.
pub fn day_start_ms(day: i32, off_min: i32) -> i64 {
    day as i64 * MS_PER_DAY - off_min as i64 * MS_PER_MIN
}

/// Local hour (0..24) of an instant.
pub fn hour_of(ts_ms: i64, off_min: i32) -> usize {
    ((ts_ms + off_min as i64 * MS_PER_MIN).rem_euclid(MS_PER_DAY) / MS_PER_HOUR) as usize
}

/// `2026-11-17` for a local day number.
pub fn day_label(day: i32) -> String {
    let (y, m, d) = civil_from_days(day as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `2026-11-17T15:10:00.000Z` for an instant (the format T3 stores).
pub fn iso_ms(ms: i64) -> String {
    let day = ms.div_euclid(MS_PER_DAY);
    let rem = ms.rem_euclid(MS_PER_DAY);
    let (y, m, d) = civil_from_days(day);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / MS_PER_HOUR,
        rem % MS_PER_HOUR / MS_PER_MIN,
        rem % MS_PER_MIN / 1000,
        rem % 1000
    )
}

/// The machine's current UTC offset in minutes. `WAMT_UTC_OFFSET_MIN`
/// overrides it (used in the test container).
pub fn local_offset_min() -> i32 {
    if let Ok(v) = std::env::var("WAMT_UTC_OFFSET_MIN") {
        if let Ok(n) = v.trim().parse::<i32>() {
            return n;
        }
    }
    // SAFETY: localtime_r writes into the tm we own; called once at startup.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 0;
        }
        (tm.tm_gmtoff / 60) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_utc_and_fraction() {
        // 2026-11-17T15:10:00Z is the reset T3 recorded as 1794928200000.
        assert_eq!(parse_ts_ms("2026-11-17T15:10:00.000Z"), Some(1_794_928_200_000));
        assert_eq!(parse_ts_ms("2026-11-17T15:10:00Z"), Some(1_794_928_200_000));
        assert_eq!(parse_ts_ms("2026-11-17T15:30:20.808Z"), Some(1_794_928_200_000 + 1_220_808));
        assert_eq!(parse_ts_ms("2026-11-17T15:10:00.123456Z"), Some(1_794_928_200_123));
    }

    #[test]
    fn parses_offsets_and_rejects_junk() {
        assert_eq!(parse_ts_ms("2026-11-17T20:40:00+05:30"), Some(1_794_928_200_000));
        assert_eq!(parse_ts_ms("2026-11-17T10:10:00-05:00"), Some(1_794_928_200_000));
        assert_eq!(parse_ts_ms("not a time"), None);
        assert_eq!(parse_ts_ms("2026-13-17T15:10:00Z"), None);
        assert_eq!(parse_ts_ms(""), None);
    }

    #[test]
    fn ist_day_boundaries() {
        let off = 330;
        let midnight_ist = parse_ts_ms("2026-11-16T18:30:00Z").unwrap();
        assert_eq!(day_of(midnight_ist, off), day_of(midnight_ist + 1000, off));
        assert_eq!(day_of(midnight_ist - 1, off) + 1, day_of(midnight_ist, off));
        assert_eq!(day_start_ms(day_of(midnight_ist, off), off), midnight_ist);
        assert_eq!(day_label(day_of(midnight_ist, off)), "2026-11-17");
        assert_eq!(hour_of(midnight_ist, off), 0);
        assert_eq!(hour_of(midnight_ist + 23 * MS_PER_HOUR + 59 * MS_PER_MIN, off), 23);
    }

    #[test]
    fn iso_round_trip() {
        let t = 1_794_928_200_123;
        assert_eq!(iso_ms(t), "2026-11-17T15:10:00.123Z");
        assert_eq!(parse_ts_ms(&iso_ms(t)), Some(t));
    }

    #[test]
    fn civil_round_trip() {
        for d in [-1000i64, 0, 1, 20_000, 20_774, 40_000] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y as i64, m as i64, dd as i64), d);
        }
    }
}
