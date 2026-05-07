//! Formatting helpers for the web view.

/// Relative time display (e.g. "3m ago", "2h ago", ISO if older than 24h).
pub fn format_timestamp_relative(ms: i64) -> String {
    use jiff::Timestamp;
    match Timestamp::from_millisecond(ms) {
        Ok(ts) => {
            let now = Timestamp::now();
            let span = now.since(ts).unwrap_or_else(|_| jiff::Span::new());
            let hours = span.get_hours().abs();
            let minutes = span.get_minutes().abs();
            if hours < 1 {
                if minutes < 1 {
                    "just now".to_string()
                } else {
                    format!("{minutes}m ago")
                }
            } else if hours < 24 {
                format!("{hours}h ago")
            } else {
                ts.to_string()
            }
        }
        Err(_) => format!("{ms}ms"),
    }
}

/// ISO timestamp for title attributes.
pub fn format_timestamp_iso(ms: i64) -> String {
    use jiff::Timestamp;
    Timestamp::from_millisecond(ms)
        .map(|ts| ts.to_string())
        .unwrap_or_else(|_| format!("{ms}ms"))
}

/// Duration between optional start/end millisecond timestamps.
pub fn format_duration(start: Option<i64>, end: Option<i64>) -> String {
    match (start, end) {
        (Some(s), Some(e)) => {
            let ms = e - s;
            if ms < 1000 {
                format!("{ms}ms")
            } else {
                format!("{}s", ms / 1000)
            }
        }
        _ => "—".to_string(),
    }
}

/// Duration between exact start/end millisecond timestamps.
pub fn format_duration_exact(start: i64, end: i64) -> String {
    let ms = end - start;
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
}
