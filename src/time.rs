//! Epoch-millisecond helpers. Sessions store UTC ISO-8601 strings or epoch
//! milliseconds; everything inside sessiongator uses epoch milliseconds.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .expect("system clock is before 1970")
}

/// Relative age like "now", "5m", "3h", "2d", "1w", "4mo", "2y".
pub fn rel_time(epoch_ms: i64, now_epoch_ms: i64) -> String {
    let secs = ((now_epoch_ms - epoch_ms) / 1000).max(0);
    let mins = secs / 60;
    if secs < 60 {
        return "now".to_string();
    }
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d");
    }
    if days < 30 {
        return format!("{}w", days / 7);
    }
    if days < 365 {
        return format!("{}mo", days / 30);
    }
    format!("{}y", days / 365)
}

/// Parse an ISO-8601 UTC timestamp ("2026-06-29T16:11:35.150Z") to epoch ms.
pub fn parse_iso_utc_ms(value: &str) -> Option<i64> {
    let value = value.strip_suffix('Z')?;
    let (date, time) = value.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut time_parts = hms.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts.next()?.parse().ok()?;
    let millis: i64 = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(3).collect();
        let value: i64 = digits.parse().ok()?;
        value * 10i64.pow(3 - digits.len() as u32)
    };
    let days = days_from_civil(year, month, day);
    Some((((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis)
}

/// Like `parse_iso_utc_ms`, but also accepts SQLite `datetime('now')`
/// values: a space instead of `T`, and no `Z`.
pub fn parse_flexible_time_ms(value: &str) -> Option<i64> {
    let value = value.replacen(' ', "T", 1);
    parse_iso_utc_ms(&value).or_else(|| parse_iso_utc_ms(&format!("{value}Z")))
}

/// Format epoch ms as "YYYY-MM-DD HH:MM" (UTC).
pub fn format_utc(epoch_ms: i64) -> String {
    let (year, month, day, hour, minute, _, _) = civil_parts(epoch_ms);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Format epoch ms as "YYYY-MM-DDTHH:MM:SS.mmmZ".
pub fn iso_utc(epoch_ms: i64) -> String {
    let (year, month, day, hour, minute, second, millis) = civil_parts(epoch_ms);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// (year, month, day) of epoch ms in UTC.
pub fn civil_date(epoch_ms: i64) -> (i64, u32, u32) {
    civil_from_days(epoch_ms.div_euclid(1000).div_euclid(86_400))
}

fn civil_parts(epoch_ms: i64) -> (i64, u32, u32, i64, i64, i64, i64) {
    let secs = epoch_ms.div_euclid(1000);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    (
        year,
        month,
        day,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        epoch_ms.rem_euclid(1000),
    )
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of `days_from_civil`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = ((mp + 2) % 12 + 1) as u32;
    (if month <= 2 { y + 1 } else { y }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_roundtrip() {
        let ms = parse_iso_utc_ms("2026-06-29T16:11:35.150Z").unwrap();
        assert_eq!(format_utc(ms), "2026-06-29 16:11");
        assert_eq!(iso_utc(ms), "2026-06-29T16:11:35.150Z");
        assert_eq!(civil_date(ms), (2026, 6, 29));
    }

    #[test]
    fn iso_without_fraction_or_zone() {
        assert_eq!(parse_iso_utc_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_flexible_time_ms("1970-01-01T00:00:01"), Some(1000));
        assert_eq!(parse_flexible_time_ms("1970-01-01 00:00:02"), Some(2000));
        assert_eq!(parse_iso_utc_ms("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso_utc_ms("not a date"), None);
    }

    #[test]
    fn epoch_day_boundaries() {
        assert_eq!(format_utc(0), "1970-01-01 00:00");
        let ms = parse_iso_utc_ms("2024-02-29T12:00:00Z").unwrap();
        assert_eq!(format_utc(ms), "2024-02-29 12:00");
    }

    #[test]
    fn relative_times() {
        let now = 1_000_000_000_000;
        assert_eq!(rel_time(now - 30_000, now), "now");
        assert_eq!(rel_time(now - 5 * 60_000, now), "5m");
        assert_eq!(rel_time(now - 3 * 3_600_000, now), "3h");
        assert_eq!(rel_time(now - 2 * 86_400_000, now), "2d");
        assert_eq!(rel_time(now - 400 * 86_400_000, now), "1y");
    }
}
