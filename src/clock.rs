//! Wall-clock timestamps for records (`2026-10-17T19:30:02Z`). Elapsed budgets use the
//! monotonic clock instead (`DESIGN` § 9a).

use std::time::{SystemTime, UNIX_EPOCH};

/// The current UTC time in RFC 3339 form, to the second.
#[must_use]
pub fn now_rfc3339() -> String {
    rfc3339_of(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs()),
    )
}

/// The UTC time `secs` after the Unix epoch, in the form [`now_rfc3339`] writes.
#[must_use]
pub fn rfc3339_of(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// The Unix time of a timestamp in the form [`now_rfc3339`] writes (`2026-10-17T19:30:02Z`);
/// `None` for any other text.
#[must_use]
pub fn unix_of(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let shape = bytes.len() == 20
        && bytes.iter().enumerate().all(|(at, byte)| match at {
            4 | 7 => *byte == b'-',
            10 => *byte == b'T',
            13 | 16 => *byte == b':',
            19 => *byte == b'Z',
            _ => byte.is_ascii_digit(),
        });
    if !shape {
        return None;
    }
    let field = |from: usize, to: usize| text.get(from..to)?.parse::<i64>().ok();
    let (year, month, day) = (field(0, 4)?, field(5, 7)?, field(8, 10)?);
    let (hour, minute, second) = (field(11, 13)?, field(14, 16)?, field(17, 19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    if second > 60 {
        return None;
    }
    u64::try_from(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
        .ok()
}

/// A proleptic Gregorian date to days since 1970-01-01 (Howard Hinnant's algorithm).
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::{civil_from_days, now_rfc3339, unix_of};

    #[test]
    fn a_written_timestamp_reads_back() {
        assert_eq!(unix_of("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_of("2000-02-29T00:00:01Z"), Some(11_016 * 86_400 + 1));
        assert_eq!(
            unix_of("2026-10-17T19:30:02Z"),
            Some(20_743 * 86_400 + 70_202)
        );
        let now = now_rfc3339();
        assert!(
            unix_of(&now).is_some_and(|secs| secs / 86_400 > 20_000),
            "{now}"
        );
        for bad in [
            "now",
            "",
            "2026-10-17 19:30:02Z",
            "2026-13-01T00:00:00Z",
            "+026-10-17T19:30:02Z",
        ] {
            assert_eq!(unix_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_743), (2026, 10, 17));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }
}
