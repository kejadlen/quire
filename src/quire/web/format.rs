//! Formatting helpers for the web view.

use jiff::Timestamp;

/// Relative time display (e.g. "3m ago", "2h ago", ISO if older than 24h).
pub fn format_timestamp_relative(ms: i64) -> String {
    let Ok(ts) = Timestamp::from_millisecond(ms) else {
        return format!("{ms}ms");
    };
    let diff_secs = Timestamp::now().duration_since(ts).as_secs().max(0);
    match relative_label(diff_secs) {
        Some(label) => label,
        None => ts.to_string(),
    }
}

/// Format a positive seconds-ago value as "Nm ago" / "Nh ago".
///
/// Returns `None` when the diff is at least 24 hours — caller renders
/// an absolute timestamp instead.
fn relative_label(secs: i64) -> Option<String> {
    let mins = secs / 60;
    let hours = mins / 60;
    if hours >= 24 {
        return None;
    }
    if mins == 0 {
        return Some("just now".to_string());
    }
    if hours == 0 {
        return Some(format!("{mins}m ago"));
    }
    Some(format!("{hours}h ago"))
}

/// ISO timestamp for title attributes.
pub fn format_timestamp_iso(ms: i64) -> String {
    Timestamp::from_millisecond(ms)
        .map(|ts| ts.to_string())
        .unwrap_or_else(|_| format!("{ms}ms"))
}

/// Duration between optional start/end millisecond timestamps.
pub fn format_duration(start: Option<i64>, end: Option<i64>) -> String {
    match (start, end) {
        (Some(s), Some(e)) => format_ms_duration(e - s),
        _ => "—".to_string(),
    }
}

/// Duration between exact start/end millisecond timestamps.
pub fn format_duration_exact(start: i64, end: i64) -> String {
    format_ms_duration(end - start)
}

fn format_ms_duration(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{}s", ms / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_shows_ms_for_subsecond() {
        assert_eq!(format_duration(Some(0), Some(500)), "500ms");
    }

    #[test]
    fn format_duration_shows_seconds() {
        assert_eq!(format_duration(Some(0), Some(3500)), "3s");
    }

    #[test]
    fn format_duration_dash_when_missing() {
        assert_eq!(format_duration(None, None), "—");
    }

    #[test]
    fn format_duration_clamps_negative_to_zero() {
        assert_eq!(format_duration(Some(500), Some(0)), "0ms");
    }

    #[test]
    fn relative_label_just_now_under_a_minute() {
        assert_eq!(relative_label(0).as_deref(), Some("just now"));
        assert_eq!(relative_label(59).as_deref(), Some("just now"));
    }

    #[test]
    fn relative_label_minutes() {
        assert_eq!(relative_label(60).as_deref(), Some("1m ago"));
        assert_eq!(relative_label(59 * 60 + 59).as_deref(), Some("59m ago"));
    }

    #[test]
    fn relative_label_hours() {
        assert_eq!(relative_label(60 * 60).as_deref(), Some("1h ago"));
        assert_eq!(relative_label(23 * 60 * 60).as_deref(), Some("23h ago"));
    }

    #[test]
    fn relative_label_returns_none_past_a_day() {
        assert_eq!(relative_label(24 * 60 * 60), None);
        assert_eq!(relative_label(72 * 60 * 60), None);
    }
}
