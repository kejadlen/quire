//! Formatting helpers for the web view.

use jiff::Timestamp;

/// Relative time display (e.g. "3m ago", "2h ago", "4d ago", "3mo ago").
pub fn format_timestamp_relative(ms: i64) -> String {
    let Ok(ts) = Timestamp::from_millisecond(ms) else {
        return format!("{ms}ms");
    };
    let diff_secs = Timestamp::now().duration_since(ts).as_secs().max(0);
    relative_label(diff_secs)
}

/// Format a positive seconds-ago value as a human-friendly relative string.
fn relative_label(secs: i64) -> String {
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    if days >= 365 {
        let years = days / 365;
        return format!("{years}y ago");
    }
    if days >= 30 {
        let months = days / 30;
        return format!("{months}mo ago");
    }
    if days >= 7 {
        let weeks = days / 7;
        return format!("{weeks}w ago");
    }
    if days >= 1 {
        return format!("{days}d ago");
    }
    if hours >= 1 {
        return format!("{hours}h ago");
    }
    if mins >= 1 {
        return format!("{mins}m ago");
    }
    "just now".to_string()
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

/// Map a CI run/job state string to a CSS colour class.
///
/// Centralised here so `RunListRow`, `DetailRun`, and `DetailJob`
/// don't each carry their own identical match.
pub fn state_class(state: &str) -> &'static str {
    match state {
        "complete" => "c-ok",
        "failed" => "c-bad",
        _ => "c-muted",
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
        assert_eq!(relative_label(0), "just now");
        assert_eq!(relative_label(59), "just now");
    }

    #[test]
    fn relative_label_minutes() {
        assert_eq!(relative_label(60), "1m ago");
        assert_eq!(relative_label(59 * 60 + 59), "59m ago");
    }

    #[test]
    fn relative_label_hours() {
        assert_eq!(relative_label(60 * 60), "1h ago");
        assert_eq!(relative_label(23 * 60 * 60), "23h ago");
    }

    #[test]
    fn relative_label_days() {
        assert_eq!(relative_label(24 * 60 * 60), "1d ago");
        assert_eq!(relative_label(6 * 24 * 60 * 60), "6d ago");
    }

    #[test]
    fn relative_label_weeks() {
        assert_eq!(relative_label(7 * 24 * 60 * 60), "1w ago");
        assert_eq!(relative_label(29 * 24 * 60 * 60), "4w ago");
    }

    #[test]
    fn relative_label_months() {
        assert_eq!(relative_label(30 * 24 * 60 * 60), "1mo ago");
        assert_eq!(relative_label(364 * 24 * 60 * 60), "12mo ago");
    }

    #[test]
    fn relative_label_years() {
        assert_eq!(relative_label(365 * 24 * 60 * 60), "1y ago");
        assert_eq!(relative_label(3 * 365 * 24 * 60 * 60), "3y ago");
    }

    #[test]
    fn state_class_complete() {
        assert_eq!(state_class("complete"), "c-ok");
    }

    #[test]
    fn state_class_failed() {
        assert_eq!(state_class("failed"), "c-bad");
    }

    #[test]
    fn state_class_unknown_falls_through() {
        assert_eq!(state_class("pending"), "c-muted");
        assert_eq!(state_class("active"), "c-muted");
        assert_eq!(state_class(""), "c-muted");
    }
}
