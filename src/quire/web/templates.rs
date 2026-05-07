//! Askama template structs and their formatting methods.

use askama::Template;

use super::format;

/// The package version, exposed to every template for the footer.
fn pkg_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A navigation breadcrumb entry.
///
/// When `href` is `Some`, the crumb renders as a clickable link.
pub struct Crumb {
    pub label: String,
    pub href: Option<String>,
}

impl Crumb {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            href: None,
        }
    }

    pub fn with_href(label: impl Into<String>, href: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            href: Some(href.into()),
        }
    }
}

// ── Run list ───────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "ci/run_list.html")]
pub struct RunListTemplate {
    pub repo: String,
    pub crumbs: Vec<Crumb>,
    pub runs: Vec<RunListRow>,
}

impl RunListTemplate {
    pub fn version(&self) -> &'static str {
        pkg_version()
    }
}

pub struct RunListRow {
    pub id: String,
    pub state: String,
    pub sha: String,
    pub ref_name: String,
    pub queued_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
}

impl RunListRow {
    pub fn state_class(&self) -> &'static str {
        format::state_class(&self.state)
    }

    pub fn sha_short(&self) -> &str {
        &self.sha[..self.sha.len().min(8)]
    }

    pub fn ref_short(&self) -> &str {
        self.ref_name.trim_start_matches("refs/heads/")
    }

    pub fn queued_relative(&self) -> String {
        format::format_timestamp_relative(self.queued_at_ms)
    }

    pub fn queued_iso(&self) -> String {
        format::format_timestamp_iso(self.queued_at_ms)
    }

    pub fn duration_display(&self) -> String {
        format::format_duration(self.started_at_ms, self.finished_at_ms)
    }
}

// ── Run detail ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "ci/run_detail.html")]
pub struct RunDetailTemplate {
    pub repo: String,
    pub crumbs: Vec<Crumb>,
    pub run: DetailRun,
    pub jobs: Vec<DetailJob>,
}

impl RunDetailTemplate {
    pub fn version(&self) -> &'static str {
        pkg_version()
    }
}

pub struct DetailRun {
    pub state: String,
    pub sha: String,
    pub ref_name: String,
    pub queued_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
}

impl DetailRun {
    pub fn state_class(&self) -> &'static str {
        format::state_class(&self.state)
    }

    pub fn sha_short(&self) -> &str {
        &self.sha[..self.sha.len().min(8)]
    }

    pub fn ref_short(&self) -> &str {
        self.ref_name.trim_start_matches("refs/heads/")
    }

    pub fn queued_relative(&self) -> String {
        format::format_timestamp_relative(self.queued_at_ms)
    }

    pub fn queued_iso(&self) -> String {
        format::format_timestamp_iso(self.queued_at_ms)
    }

    pub fn started_display(&self) -> String {
        self.started_at_ms
            .map(format::format_timestamp_relative)
            .unwrap_or_else(|| "—".to_string())
    }

    pub fn started_iso(&self) -> String {
        self.started_at_ms
            .map(format::format_timestamp_iso)
            .unwrap_or_default()
    }

    pub fn has_started(&self) -> bool {
        self.started_at_ms.is_some()
    }

    pub fn finished_display(&self) -> String {
        self.finished_at_ms
            .map(format::format_timestamp_relative)
            .unwrap_or_else(|| "—".to_string())
    }

    pub fn finished_iso(&self) -> String {
        self.finished_at_ms
            .map(format::format_timestamp_iso)
            .unwrap_or_default()
    }

    pub fn has_finished(&self) -> bool {
        self.finished_at_ms.is_some()
    }

    pub fn duration_display(&self) -> String {
        format::format_duration(self.started_at_ms, self.finished_at_ms)
    }
}

pub struct DetailJob {
    pub job_id: String,
    pub state: String,
    pub exit_code: Option<i32>,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub sh_events: Vec<DetailShEvent>,
}

impl DetailJob {
    pub fn state_class(&self) -> &'static str {
        format::state_class(&self.state)
    }

    pub fn duration_display(&self) -> String {
        format::format_duration(self.started_at_ms, self.finished_at_ms)
    }

    pub fn exit_display(&self) -> String {
        self.exit_code
            .map(|c| format!(" · exit {c}"))
            .unwrap_or_default()
    }
}

pub struct DetailShEvent {
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub exit_code: i32,
    pub cmd: String,
    pub log_content: String,
}

impl DetailShEvent {
    pub fn duration_display(&self) -> String {
        format::format_duration_exact(self.started_at_ms, self.finished_at_ms)
    }

    pub fn cmd_display(&self) -> &str {
        &self.cmd
    }
}

// ── Error ──────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorTemplate {
    pub repo: String,
    pub crumbs: Vec<Crumb>,
    pub title: String,
    pub detail: String,
}

impl ErrorTemplate {
    pub fn version(&self) -> &'static str {
        pkg_version()
    }
}
