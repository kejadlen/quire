//! Askama template structs and their formatting methods.

use askama::Template;

use super::format;

/// The build version, exposed to every template for the footer.
fn pkg_version() -> &'static str {
    env!("QUIRE_VERSION")
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
    pub bookmarks: Vec<BookmarkRow>,
    pub tags: Vec<TagRow>,
    pub active_section: String,
}

impl RunListTemplate {
    pub fn version(&self) -> &'static str {
        pkg_version()
    }
}

pub struct RunListRow {
    pub id: String,
    pub outcome: Option<String>,
    pub sha: String,
    pub ref_name: String,
    pub created_at: i64,
    pub dispatched_at: Option<i64>,
    pub resolved_at: Option<i64>,
}

impl RunListRow {
    pub fn state(&self) -> &str {
        format::derive_run_state(self.outcome.as_deref(), self.dispatched_at)
    }

    pub fn state_class(&self) -> &'static str {
        format::state_class(self.state())
    }

    pub fn sha_short(&self) -> &str {
        &self.sha[..self.sha.len().min(8)]
    }

    pub fn branch_short(&self) -> &str {
        self.ref_name.trim_start_matches("refs/heads/")
    }

    pub fn queued_relative(&self) -> String {
        format::format_timestamp_relative(self.created_at)
    }

    pub fn queued_iso(&self) -> String {
        format::format_timestamp_iso(self.created_at)
    }

    pub fn duration_display(&self) -> String {
        format::format_duration(self.dispatched_at, self.resolved_at)
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
    pub quire_ci_log: String,
    pub bookmarks: Vec<BookmarkRow>,
    pub tags: Vec<TagRow>,
    pub active_section: String,
}

impl RunDetailTemplate {
    pub fn version(&self) -> &'static str {
        pkg_version()
    }
}

pub struct DetailRun {
    pub outcome: Option<String>,
    pub sha: String,
    pub ref_name: String,
    pub created_at: i64,
    pub dispatched_at: Option<i64>,
    pub resolved_at: Option<i64>,
}

impl DetailRun {
    pub fn state(&self) -> &str {
        format::derive_run_state(self.outcome.as_deref(), self.dispatched_at)
    }

    pub fn state_class(&self) -> &'static str {
        format::state_class(self.state())
    }

    pub fn sha_short(&self) -> &str {
        &self.sha[..self.sha.len().min(8)]
    }

    pub fn branch_short(&self) -> &str {
        self.ref_name.trim_start_matches("refs/heads/")
    }

    pub fn queued_relative(&self) -> String {
        format::format_timestamp_relative(self.created_at)
    }

    pub fn queued_iso(&self) -> String {
        format::format_timestamp_iso(self.created_at)
    }

    pub fn started_display(&self) -> String {
        self.dispatched_at
            .map(format::format_timestamp_relative)
            .unwrap_or_else(|| "—".to_string())
    }

    pub fn started_iso(&self) -> String {
        self.dispatched_at
            .map(format::format_timestamp_iso)
            .unwrap_or_default()
    }

    pub fn has_started(&self) -> bool {
        self.dispatched_at.is_some()
    }

    pub fn finished_display(&self) -> String {
        self.resolved_at
            .map(format::format_timestamp_relative)
            .unwrap_or_else(|| "—".to_string())
    }

    pub fn finished_iso(&self) -> String {
        self.resolved_at
            .map(format::format_timestamp_iso)
            .unwrap_or_default()
    }

    pub fn has_finished(&self) -> bool {
        self.resolved_at.is_some()
    }

    pub fn is_resolved(&self) -> bool {
        self.outcome.is_some()
    }

    pub fn is_terminal(&self) -> bool {
        self.is_resolved()
    }

    pub fn duration_display(&self) -> String {
        format::format_duration(self.dispatched_at, self.resolved_at)
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

    pub fn exit_code_filter_nonzero(&self) -> Option<i32> {
        self.exit_code.filter(|&c| c != 0)
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
        format::format_duration(Some(self.started_at_ms), Some(self.finished_at_ms))
    }

    pub fn cmd_display(&self) -> &str {
        &self.cmd
    }
}

// ── Repo Home ──────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "repo_home.html")]
pub struct RepoHomeTemplate {
    pub repo: String,
    pub crumbs: Vec<Crumb>,
    pub head: Option<HeadInfo>,
    pub readme_html: Option<String>,
    pub bookmarks: Vec<BookmarkRow>,
    pub tags: Vec<TagRow>,
    pub recent_runs: Vec<RunListRow>,
    pub recent_changes: Vec<ChangeRow>,
    pub active_section: String,
}

impl RepoHomeTemplate {
    pub fn version(&self) -> &'static str {
        pkg_version()
    }

    pub fn latest_ci_state(&self) -> &str {
        self.recent_runs
            .first()
            .map(|r| r.state())
            .unwrap_or("none")
    }

    pub fn latest_ci_state_class(&self) -> &'static str {
        self.recent_runs
            .first()
            .map(|r| r.state_class())
            .unwrap_or("")
    }

    pub fn bookmarks_preview(&self) -> &[BookmarkRow] {
        &self.bookmarks[..self.bookmarks.len().min(5)]
    }

    pub fn extra_bookmarks(&self) -> usize {
        self.bookmarks.len().saturating_sub(5)
    }

    pub fn tags_preview(&self) -> &[TagRow] {
        &self.tags[..self.tags.len().min(5)]
    }

    pub fn extra_tags(&self) -> usize {
        self.tags.len().saturating_sub(5)
    }
}

pub struct HeadInfo {
    pub sha: String,
    pub description: String,
    pub age: String,
    pub bookmark: String,
}

impl HeadInfo {
    pub fn change_head(&self) -> &str {
        let end = self.sha.len().min(4);
        &self.sha[..end]
    }

    pub fn change_tail(&self) -> &str {
        let start = self.sha.len().min(4);
        let end = self.sha.len().min(8);
        &self.sha[start..end]
    }

    pub fn sha_short(&self) -> &str {
        &self.sha[..self.sha.len().min(8)]
    }
}

pub struct BookmarkRow {
    pub name: String,
    pub sha_short: String,
    pub age: String,
}

pub struct TagRow {
    pub name: String,
    pub age: String,
}

pub struct ChangeRow {
    pub sha: String,
    pub description: String,
    pub age: String,
}

impl ChangeRow {
    pub fn change_head(&self) -> &str {
        let end = self.sha.len().min(4);
        &self.sha[..end]
    }

    pub fn change_tail(&self) -> &str {
        let start = self.sha.len().min(4);
        let end = self.sha.len().min(8);
        &self.sha[start..end]
    }

    pub fn sha_full(&self) -> &str {
        &self.sha
    }
}

// ── Config ─────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "config.html")]
pub struct ConfigTemplate {
    pub crumbs: Vec<Crumb>,
    pub config: crate::GlobalConfig,
}

impl ConfigTemplate {
    pub fn version(&self) -> &'static str {
        pkg_version()
    }

    pub fn sorted_secrets(&self) -> Vec<(&String, &quire_core::secret::SecretString)> {
        let mut pairs: Vec<_> = self.config.secrets.iter().collect();
        pairs.sort_by_key(|(k, _)| *k);
        pairs
    }
}

// ── Tree view ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "tree.html")]
pub struct TreeTemplate {
    pub repo: String,
    pub crumbs: Vec<Crumb>,
    pub bookmarks: Vec<BookmarkRow>,
    pub tags: Vec<TagRow>,
    pub active_section: String,
    /// Current directory path relative to repo root ("" = root).
    pub path: String,
    /// Active bookmark name (e.g. "main").
    pub bookmark: String,
    /// Short commit hash for HEAD.
    pub sha_short: String,
    pub entries: Vec<TreeEntry>,
    pub total_entries: usize,
    pub head_commit: Option<PathCommit>,
    pub readme_preview: Option<String>,
}

impl TreeTemplate {
    pub fn version(&self) -> &'static str {
        pkg_version()
    }

    /// Returns (label, href) pairs for the path breadcrumb.
    /// Root: just the repo name with no link (it's the current location).
    /// Sub-path: repo → each segment, with links on all but the last.
    pub fn path_parts(&self) -> Vec<(String, Option<String>)> {
        if self.path.is_empty() {
            return vec![(self.repo.clone(), None)];
        }
        let mut parts = vec![(self.repo.clone(), Some(format!("/{}/tree", self.repo)))];
        let segments: Vec<&str> = self.path.split('/').collect();
        for (i, seg) in segments.iter().enumerate() {
            let subpath = segments[..=i].join("/");
            let href = if i < segments.len() - 1 {
                Some(format!("/{}/tree/{}", self.repo, subpath))
            } else {
                None
            };
            parts.push((seg.to_string(), href));
        }
        parts
    }

    pub fn parent_url(&self) -> String {
        if self.path.is_empty() {
            return format!("/{}", self.repo);
        }
        match self.path.rfind('/') {
            Some(idx) => format!("/{}/tree/{}", self.repo, &self.path[..idx]),
            None => format!("/{}/tree", self.repo),
        }
    }

    pub fn dir_entry_url(&self, name: &str) -> String {
        if self.path.is_empty() {
            format!("/{}/tree/{}", self.repo, name)
        } else {
            format!("/{}/tree/{}/{}", self.repo, self.path, name)
        }
    }

    pub fn dir_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.is_dir())
            .count()
    }

    pub fn submodule_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.is_submodule())
            .count()
    }

    pub fn file_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.is_file())
            .count()
    }

    pub fn sha_head(&self) -> &str {
        &self.sha_short[..self.sha_short.len().min(4)]
    }

    pub fn sha_tail(&self) -> &str {
        let start = self.sha_short.len().min(4);
        &self.sha_short[start..]
    }
}

pub struct TreeEntry {
    pub kind: TreeEntryKind,
    pub name: String,
    pub last_msg: String,
    pub age: String,
}

pub enum TreeEntryKind {
    Up,
    Dir,
    File,
    Submodule,
}

impl TreeEntry {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, TreeEntryKind::Dir)
    }

    pub fn is_file(&self) -> bool {
        matches!(self.kind, TreeEntryKind::File)
    }

    pub fn is_submodule(&self) -> bool {
        matches!(self.kind, TreeEntryKind::Submodule)
    }

    pub fn is_up(&self) -> bool {
        matches!(self.kind, TreeEntryKind::Up)
    }

    pub fn is_dir_like(&self) -> bool {
        matches!(self.kind, TreeEntryKind::Dir | TreeEntryKind::Submodule)
    }
}

pub struct PathCommit {
    pub sha_short: String,
    pub description: String,
    pub age: String,
    pub author: String,
}

impl PathCommit {
    pub fn sha_head(&self) -> &str {
        &self.sha_short[..self.sha_short.len().min(4)]
    }

    pub fn sha_tail(&self) -> &str {
        let start = self.sha_short.len().min(4);
        &self.sha_short[start..]
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
