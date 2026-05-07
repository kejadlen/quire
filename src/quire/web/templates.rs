//! Askama template structs and their formatting methods.

use askama::Template;

use super::format;

// ── Run list ───────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "ci/run_list.html")]
pub struct RunListTemplate {
    pub repo: String,
    pub page: String,
    pub runs: Vec<RunListRow>,
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
    pub fn state_class(&self) -> &str {
        match self.state.as_str() {
            "complete" => "c-ok",
            "failed" => "c-bad",
            _ => "c-muted",
        }
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
    pub page: String,
    pub run: DetailRun,
    pub jobs: Vec<DetailJob>,
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
    pub fn state_class(&self) -> &str {
        match self.state.as_str() {
            "complete" => "c-ok",
            "failed" => "c-bad",
            _ => "c-muted",
        }
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
    pub fn state_class(&self) -> &str {
        match self.state.as_str() {
            "complete" => "c-ok",
            "failed" => "c-bad",
            _ => "c-muted",
        }
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
    pub index: usize,
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
        if self.cmd.len() > 120 {
            &self.cmd[..120]
        } else {
            &self.cmd
        }
    }
}

// ── Error ──────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorTemplate {
    pub repo: String,
    pub page: String,
    pub title: String,
    pub detail: String,
}
