use std::{
    cmp::Reverse,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tool {
    Claude,
    Opencode,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Opencode => "opencode",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Claude => "◆",
            Self::Opencode => "◇",
        }
    }
}

/// One AI coding session, normalized across sources. Carries only fields every
/// source can provide; anything tool-specific goes into `extras` as
/// display-ready key/value pairs.
#[derive(Clone, Debug)]
pub struct Session {
    pub tool: Tool,
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub message_count: u32,
    pub model: Option<String>,
    pub source_ref: String,
    pub extras: Vec<(String, String)>,
}

impl Session {
    /// Stable cross-source key ("<tool>:<native id>").
    pub fn key(&self) -> String {
        format!("{}:{}", self.tool.name(), self.id)
    }

    /// Lowercased blob used for name/path/model matching.
    pub fn search_blob(&self) -> String {
        format!(
            "{} {} {} {}",
            self.cwd,
            shorten_home(&self.cwd),
            self.title,
            self.model.as_deref().unwrap_or("")
        )
        .to_lowercase()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Updated,
    Created,
    Messages,
    Path,
}

impl SortMode {
    pub fn next(self) -> Self {
        match self {
            Self::Updated => Self::Created,
            Self::Created => Self::Messages,
            Self::Messages => Self::Path,
            Self::Path => Self::Updated,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Updated => "updated",
            Self::Created => "created",
            Self::Messages => "msgs",
            Self::Path => "path",
        }
    }
}

pub fn sort_sessions(sessions: &mut [Session], mode: SortMode) {
    match mode {
        SortMode::Updated => sessions.sort_by_key(|session| Reverse(session.updated_ms)),
        SortMode::Created => sessions.sort_by_key(|session| Reverse(session.created_ms)),
        SortMode::Messages => sessions.sort_by_key(|session| Reverse(session.message_count)),
        SortMode::Path => sessions.sort_by(|a, b| {
            (a.cwd.to_lowercase(), a.updated_ms).cmp(&(b.cwd.to_lowercase(), b.updated_ms))
        }),
    }
}

/// Collapse all whitespace runs (incl. newlines/tabs) to single spaces so
/// titles are always safe for one-line display and tab-separated output.
pub fn clean_title(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn shorten_home(path: &str) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return path.to_string();
    };
    if path == home {
        "~".to_string()
    } else if let Some(rest) = path.strip_prefix(&format!("{home}/")) {
        format!("~/{rest}")
    } else {
        path.to_string()
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
    let (hms, frac) = match time.split_once('.') {
        Some((hms, frac)) => (hms, frac),
        None => (time, ""),
    };
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

/// Format epoch ms as "YYYY-MM-DD HH:MM" (UTC).
pub fn format_utc(epoch_ms: i64) -> String {
    let secs = epoch_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
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
        assert_eq!(ms % 1000, 150);
    }

    #[test]
    fn iso_without_fraction() {
        let ms = parse_iso_utc_ms("1970-01-01T00:00:00Z").unwrap();
        assert_eq!(ms, 0);
        assert_eq!(parse_iso_utc_ms("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso_utc_ms("not a date"), None);
    }

    #[test]
    fn epoch_day_boundaries() {
        assert_eq!(format_utc(0), "1970-01-01 00:00");
        // leap year day
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

    #[test]
    fn sorting() {
        let mk = |id: &str, updated: i64, created: i64, msgs: u32, cwd: &str| Session {
            tool: Tool::Claude,
            id: id.to_string(),
            title: String::new(),
            cwd: cwd.to_string(),
            created_ms: created,
            updated_ms: updated,
            message_count: msgs,
            model: None,
            source_ref: String::new(),
            extras: Vec::new(),
        };
        let mut sessions = vec![mk("a", 10, 1, 5, "/z"), mk("b", 1, 10, 50, "/a")];
        sort_sessions(&mut sessions, SortMode::Updated);
        assert_eq!(sessions[0].id, "a");
        sort_sessions(&mut sessions, SortMode::Created);
        assert_eq!(sessions[0].id, "b");
        sort_sessions(&mut sessions, SortMode::Messages);
        assert_eq!(sessions[0].id, "b");
        sort_sessions(&mut sessions, SortMode::Path);
        assert_eq!(sessions[0].id, "b");
    }
}
