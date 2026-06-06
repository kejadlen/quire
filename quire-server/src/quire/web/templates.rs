//! Maud template rendering.

use maud::{DOCTYPE, Markup, PreEscaped, html};

use super::format;

fn pkg_version() -> &'static str {
    env!("QUIRE_VERSION")
}

// ── Shared types ────────────────────────────────────────────────────

/// A section nav link in the repo tab bar.
pub struct SectionLink {
    pub label: &'static str,
    pub href: String,
    pub active: bool,
}

/// Build the section nav links for a repo page.
///
/// CI is included only when `authed` — the decision belongs here, not in the
/// template.
pub fn nav_sections(repo: &str, active: &str, authed: bool) -> Vec<SectionLink> {
    let mut sections = vec![
        SectionLink {
            label: "overview",
            href: format!("/{repo}"),
            active: active == "overview",
        },
        SectionLink {
            label: "tree",
            href: format!("/{repo}/tree"),
            active: active == "tree",
        },
        SectionLink {
            label: "log",
            href: format!("/{repo}/log"),
            active: active == "log",
        },
    ];
    if authed {
        sections.push(SectionLink {
            label: "ci",
            href: format!("/{repo}/ci"),
            active: active == "ci",
        });
    }
    sections
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

// ── Layout helpers ─────────────────────────────────────────────────

const QUIRE_APP_JS: &str = r#"
function quireApp() {
  return {
    darkMode: false,
    init() {
      const stored = localStorage.getItem('quire-dark');
      if (stored !== null) {
        this.darkMode = stored === '1';
      } else {
        this.darkMode = window.matchMedia('(prefers-color-scheme: dark)').matches;
      }
      this._highlight();
    },
    toggleDark() {
      this.darkMode = !this.darkMode;
      localStorage.setItem('quire-dark', this.darkMode ? '1' : '0');
      this._highlight();
    },
    _highlight() {
      const arborium = window.arborium;
      if (!arborium) return;
      const theme = this.darkMode ? 'github-dark' : 'github-light';
      arborium.highlightAll({ theme });
    }
  }
}
"#;

fn q_mark_svg() -> Markup {
    html! {
        svg class="q-mark" width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" {
            rect x="2" y="2" width="12" height="12" rx="1.2" fill="none"
                stroke="currentColor" stroke-width="1.2" {}
            line x1="4.5" y1="6" x2="11.5" y2="6"
                stroke="currentColor" stroke-width="0.8" {}
            line x1="4.5" y1="8" x2="11.5" y2="8"
                stroke="currentColor" stroke-width="0.8" {}
            line x1="4.5" y1="10" x2="9" y2="10"
                stroke="currentColor" stroke-width="0.8" {}
            circle cx="11" cy="11" r="3" fill="none"
                stroke="currentColor" stroke-width="0.8"
                stroke-dasharray="1.2 1.2" opacity="0.35" {}
        }
    }
}

fn footer() -> Markup {
    html! {
        footer class="page-footer" {
            span { "quire v" (pkg_version()) }
            button class="dark-toggle"
                "@click"="toggleDark()"
                ":aria-label"="darkMode ? 'switch to light' : 'switch to dark'"
                title="toggle dark mode" {
                span "x-show"="!darkMode" { "◐" }
                span "x-show"="darkMode" { "◑" }
            }
        }
    }
}

fn page_nav(repo: &str, crumbs: Option<&[Crumb]>) -> Markup {
    html! {
        nav class="page-nav" {
            div class="nav-bar" {
                a class="nav-wordmark" href="/" aria-label="quire home" {
                    (q_mark_svg())
                    span class="nav-wordmark-text" { "quire" }
                }
                span class="sep" { "/" }
                a class="nav-repo" href=(format!("/{repo}")) { (repo) }
                @if let Some(crumbs) = crumbs {
                    @for crumb in crumbs {
                        span class="sep" { "/" }
                        @if let Some(href) = &crumb.href {
                            a class="nav-crumb" href=(href) { (crumb.label) }
                        } @else {
                            span class="nav-crumb" { (crumb.label) }
                        }
                    }
                }
            }
        }
    }
}

fn config_page_nav() -> Markup {
    html! {
        nav class="page-nav" {
            div class="nav-bar" {
                a class="nav-wordmark" href="/" aria-label="quire home" {
                    (q_mark_svg())
                    span class="nav-wordmark-text" { "quire" }
                }
                span class="sep" { "/" }
                span class="nav-crumb" { "config" }
            }
        }
    }
}

fn section_nav_links(sections: &[SectionLink]) -> Markup {
    html! {
        @for s in sections {
            a class=(if s.active { "section-link section-link--active" } else { "section-link" })
              href=(s.href) {
                (s.label)
            }
        }
    }
}

fn base(title: &str, nav: Markup, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" "x-data"="quireApp()" ":class"="{ dark: darkMode }" "x-init"="init()" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                link rel="stylesheet" href="/style.css";
                script defer="" data-manual=""
                    src="https://cdn.jsdelivr.net/npm/@arborium/arborium@2/dist/arborium.iife.js" {}
                script defer=""
                    src="https://cdn.jsdelivr.net/npm/alpinejs@3/dist/cdn.min.js" {}
            }
            body {
                (nav)
                (body)
                (footer())
            }
            script { (PreEscaped(QUIRE_APP_JS)) }
        }
    }
}

// ── Run list ───────────────────────────────────────────────────────

pub struct RunListTemplate {
    pub repo: String,
    pub crumbs: Option<Vec<Crumb>>,
    pub runs: Vec<RunListRow>,
    pub sections: Vec<SectionLink>,
}

impl RunListTemplate {
    pub fn render(&self) -> Markup {
        let body = html! {
            nav class="repo-section-nav" {
                (section_nav_links(&self.sections))
            }
            div class="repo-body" {
                article class="ci-run-list" {
                    @if self.runs.is_empty() {
                        p class="ci-empty" { "no runs yet" }
                    } @else {
                        @for run in &self.runs {
                            div class="ci-run-row" {
                                span class=(format!("ci-status-dot {}", run.state_class())) {}
                                a class="ci-commit-link"
                                  href=(format!("/{}/ci/{}", self.repo, run.id)) {
                                    (run.sha_short())
                                }
                                span class="ci-run-branch" { (run.branch_short()) }
                                span class="ci-run-age" {
                                    time title=(run.queued_iso()) { (run.queued_relative()) }
                                }
                                span class="ci-run-dur" { (run.duration_display()) }
                            }
                        }
                    }
                }
                aside class="repo-sidebar" {}
            }
        };
        base(
            &format!("ci · {}", self.repo),
            page_nav(&self.repo, self.crumbs.as_deref()),
            body,
        )
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

pub struct RunDetailTemplate {
    pub repo: String,
    pub crumbs: Option<Vec<Crumb>>,
    pub run: DetailRun,
    pub jobs: Vec<DetailJob>,
    pub quire_ci_log: String,
    pub sections: Vec<SectionLink>,
}

impl RunDetailTemplate {
    pub fn render(&self) -> Markup {
        let body = html! {
            nav class="repo-section-nav" {
                (section_nav_links(&self.sections))
                span class="repo-position" {
                    span class="ci-commit-link" { (self.run.sha_short()) }
                    span class="repo-meta-dot" { "·" }
                    span { (self.run.branch_short()) }
                    span class="repo-meta-dot" { "·" }
                    span class=(format!("ci-state-label {}", self.run.state_class())) {
                        span class=(format!("ci-status-dot {}", self.run.state_class())) {}
                        " " (self.run.state())
                    }
                }
            }
            div class="repo-body" {
                article class="ci-detail" {
                    div class="ci-meta" {
                        @if self.run.is_terminal() {
                            "queued "
                            time title=(self.run.queued_iso()) { (self.run.queued_relative()) }
                            span class="repo-meta-dot" { "·" }
                            span title=(format!("started {}\nfinished {}",
                                self.run.started_iso(), self.run.finished_iso())) {
                                "ran " (self.run.duration_display())
                            }
                        } @else if self.run.has_started() {
                            "queued "
                            time title=(self.run.queued_iso()) { (self.run.queued_relative()) }
                            span class="repo-meta-dot" { "·" }
                            "started "
                            time title=(self.run.started_iso()) { (self.run.started_display()) }
                        } @else {
                            "queued "
                            time title=(self.run.queued_iso()) { (self.run.queued_relative()) }
                        }
                    }
                    @if self.jobs.is_empty() {
                        div class="ci-empty" { "no jobs recorded" }
                    } @else {
                        @for job in &self.jobs {
                            div class="ci-job" {
                                div class="ci-job-header" {
                                    (job.job_id)
                                    span class="repo-meta-dot" { "·" }
                                    (job.duration_display())
                                    @if let Some(code) = job.exit_code_filter_nonzero() {
                                        span class="repo-meta-dot" { "·" }
                                        "exit " (code)
                                    }
                                    span class="repo-meta-dot" { "·" }
                                    span class=(format!("ci-state-label {}", job.state_class())) {
                                        span class=(format!("ci-status-dot {}", job.state_class())) {}
                                        " " (job.state)
                                    }
                                }
                                @for sh in &job.sh_events {
                                    div class="ci-sh" {
                                        div class="ci-sh-cmd" {
                                            (sh.cmd_display())
                                            span class="ci-sh-meta" {
                                                (sh.duration_display())
                                                @if sh.exit_code != 0 {
                                                    " · exit " (sh.exit_code)
                                                }
                                            }
                                        }
                                        @if !sh.log_content.is_empty() {
                                            pre class="ci-sh-log" { (sh.log_content) }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    @if !self.quire_ci_log.is_empty() {
                        div class="ci-job" {
                            div class="ci-job-header" { "quire-ci" }
                            pre class="ci-sh-log" { (self.quire_ci_log) }
                        }
                    }
                }
                aside class="repo-sidebar" {}
            }
        };
        base(
            &format!("ci · {} · {}", self.repo, self.run.sha_short()),
            page_nav(&self.repo, self.crumbs.as_deref()),
            body,
        )
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

// ── Repo list ─────────────────────────────────────────────────────

pub struct RepoListTemplate {
    pub repos: Vec<ListedRepo>,
}

impl RepoListTemplate {
    pub fn render(&self) -> Markup {
        let nav = html! {
            nav class="page-nav" {
                div class="nav-bar" {
                    a class="nav-wordmark" href="/" aria-label="quire home" {
                        (q_mark_svg())
                        span class="nav-wordmark-text" { "quire" }
                    }
                }
            }
        };
        let body = html! {
            div class="repo-list" {
                @if self.repos.is_empty() {
                    p class="repo-list-empty" { "no repositories" }
                } @else {
                    @for repo in &self.repos {
                        a class="repo-list-row" href=(format!("/{}", repo.name)) {
                            span class="repo-list-name" { (repo.name) }
                            @if let Some(desc) = &repo.description {
                                span class="repo-list-desc" { (desc) }
                            }
                        }
                    }
                }
            }
        };
        base("quire", nav, body)
    }
}

pub struct ListedRepo {
    pub name: String,
    pub description: Option<String>,
}

// ── Repo Home ──────────────────────────────────────────────────────

pub struct RepoHomeTemplate {
    pub repo: String,
    pub crumbs: Option<Vec<Crumb>>,
    pub head: Option<HeadInfo>,
    pub readme_html: Option<String>,
    pub recent_runs: Vec<RunListRow>,
    pub recent_changes: Vec<ChangeRow>,
    pub sections: Vec<SectionLink>,
}

impl RepoHomeTemplate {
    pub fn render(&self) -> Markup {
        let body = html! {
            nav class="repo-section-nav" {
                (section_nav_links(&self.sections))
                @if let Some(h) = &self.head {
                    span class="repo-position" {
                        span class="bookmark-glyph" { "※" }
                        span class="bookmark-name" { (h.bookmark) }
                        span class="repo-meta-sep" { "→" }
                        a class="change-id" href=(format!("/{}/log", self.repo))
                          title=(format!("commit {}", h.sha_short())) {
                            span class="change-head" { (h.change_head()) }
                            span class="change-tail" { (h.change_tail()) }
                        }
                        span class="commit-id-secondary" { (h.sha_short()) }
                        span class="repo-meta-dot" { "·" }
                        span class="repo-position-age" { (h.age) }
                        @if !self.recent_runs.is_empty() {
                            span class="repo-meta-dot" { "·" }
                            span class="ci-inline" {
                                span class=(format!("ci-status-dot {}", self.latest_ci_state_class())) {}
                                span { "ci " (self.latest_ci_state()) }
                            }
                        }
                    }
                }
            }
            div class="repo-body" {
                article class="repo-readme" {
                    @if let Some(html) = &self.readme_html {
                        div class="readme-content" { (PreEscaped(html)) }
                    } @else {
                        p class="readme-empty" { "no readme" }
                    }
                }
                aside class="repo-sidebar" {
                    div class="side-block" {
                        div class="side-block-title" { "Clone" }
                        div class="clone-url" { "https://quire.local/" (self.repo) ".git" }
                    }
                    @if !self.recent_runs.is_empty() {
                        div class="side-block" {
                            div class="side-block-title" { "CI" }
                            div class="ci-mini-list" {
                                @for run in &self.recent_runs {
                                    div class="ci-mini-row" {
                                        span class=(format!("ci-status-dot {}", run.state_class())) {}
                                        a class="ci-mini-link"
                                          href=(format!("/{}/ci/{}", self.repo, run.id)) {
                                            (run.sha_short())
                                        }
                                        span class="ci-mini-branch" { (run.branch_short()) }
                                        span class="ci-mini-age" {
                                            time title=(run.queued_iso()) { (run.queued_relative()) }
                                        }
                                        span class="ci-mini-dur" { (run.duration_display()) }
                                    }
                                }
                            }
                            a class="side-more" href=(format!("/{}/ci", self.repo)) {
                                "all runs →"
                            }
                        }
                    }
                    @if !self.recent_changes.is_empty() {
                        div class="side-block side-block--last" {
                            div class="side-block-title" { "Recent changes" }
                            div class="change-mini-list" {
                                @for ch in &self.recent_changes {
                                    div class="change-mini-row" {
                                        div class="change-mini-header" {
                                            a class="change-id"
                                              href=(format!("/{}/log", self.repo))
                                              title=(format!("commit {}", ch.sha_full())) {
                                                span class="change-head" { (ch.change_head()) }
                                                span class="change-tail" { (ch.change_tail()) }
                                            }
                                            span class="change-mini-age" { (ch.age) }
                                        }
                                        div class="change-mini-desc" {
                                            @if ch.description.is_empty() {
                                                span class="no-desc" { "(no description set)" }
                                            } @else {
                                                (ch.description)
                                            }
                                        }
                                    }
                                }
                            }
                            a class="side-more" href=(format!("/{}/log", self.repo)) {
                                "full log →"
                            }
                        }
                    }
                }
            }
        };
        base(
            &self.repo,
            page_nav(&self.repo, self.crumbs.as_deref()),
            body,
        )
    }

    fn latest_ci_state(&self) -> &str {
        self.recent_runs
            .first()
            .map(|r| r.state())
            .unwrap_or("none")
    }

    fn latest_ci_state_class(&self) -> &'static str {
        self.recent_runs
            .first()
            .map(|r| r.state_class())
            .unwrap_or("")
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

pub struct ChangeRow {
    pub sha: String,
    pub description: String,
    pub age: String,
    pub commit_url: String,
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

pub struct ConfigTemplate {
    pub crumbs: Option<Vec<Crumb>>,
    pub config: crate::GlobalConfig,
}

impl ConfigTemplate {
    pub fn render(&self) -> Markup {
        let body = html! {
            nav class="repo-section-nav" {
                span class="repo-position" { "config" }
            }
            div class="repo-body" {
                article class="config-table" {
                    div class="config-row" {
                        span class="config-key" { "port" }
                        span class="config-val" { (self.config.port) }
                    }
                    div class="config-row" {
                        span class="config-key" { "ci.executor" }
                        span class="config-val" { (self.config.ci.executor) }
                    }
                    @if let Some(sentry) = &self.config.sentry {
                        div class="config-row" {
                            span class="config-key" { "sentry.dsn" }
                            span class="config-val" { (sentry.dsn) }
                        }
                    } @else {
                        div class="config-row" {
                            span class="config-key" { "sentry" }
                            span class="config-val config-val--empty" { "disabled" }
                        }
                    }
                    @if let Some(token) = &self.config.github.mirror_token {
                        div class="config-row" {
                            span class="config-key" { "github.mirror-token" }
                            span class="config-val" { (token) }
                        }
                    } @else {
                        div class="config-row" {
                            span class="config-key" { "github.mirror-token" }
                            span class="config-val config-val--empty" { "not set" }
                        }
                    }
                    @for (key, value) in self.sorted_secrets() {
                        div class="config-row" {
                            span class="config-key" { "secrets." (key) }
                            span class="config-val" { (value) }
                        }
                    }
                }
                aside class="repo-sidebar" {}
            }
        };
        base("config", config_page_nav(), body)
    }

    pub fn sorted_secrets(&self) -> Vec<(&String, &quire_core::secret::SecretString)> {
        let mut pairs: Vec<_> = self.config.secrets.iter().collect();
        pairs.sort_by_key(|(k, _)| *k);
        pairs
    }
}

// ── Tree view ─────────────────────────────────────────────────────

pub struct TreeTemplate {
    pub repo: String,
    pub crumbs: Option<Vec<Crumb>>,
    pub sections: Vec<SectionLink>,
    pub path: String,
    pub bookmark: String,
    pub sha_short: String,
    pub entries: Vec<TreeEntry>,
    pub recent_changes: Vec<ChangeRow>,
}

impl TreeTemplate {
    pub fn render(&self) -> Markup {
        let body = html! {
            nav class="repo-section-nav" {
                (section_nav_links(&self.sections))
                span class="repo-position" {
                    span class="bookmark-glyph" { "※" }
                    span class="bookmark-name" { (self.bookmark) }
                    span class="repo-meta-sep" { "→" }
                    span class="change-id" title=(self.sha_short) {
                        span class="change-head" { (self.sha_head()) }
                        span class="change-tail" { (self.sha_tail()) }
                    }
                    @if !self.path.is_empty() {
                        span class="repo-meta-dot" { "·" }
                        span class="repo-position-path" { (self.path) }
                    }
                }
            }
            div class="tree-body" {
                div class="tree-table-col" {
                    @for entry in &self.entries {
                        @let row_class = format!("tree-row{}{}{}",
                            if entry.is_dir_like() { " tree-row--dir" } else { "" },
                            if entry.is_submodule() { " tree-row--sub" } else { "" },
                            if entry.is_up() { " tree-row--up" } else { "" },
                        );
                        div class=(row_class) {
                            span class="tree-icon" aria-hidden="true" {
                                @if entry.is_dir() {
                                    svg width="14" height="14" viewBox="0 0 14 14"
                                        fill="none" stroke="currentColor"
                                        stroke-width="1.2" stroke-linejoin="round" {
                                        path d="M1 3.5h4l1 1.2h7v7.8h-12z" {}
                                    }
                                } @else if entry.is_submodule() {
                                    svg width="14" height="14" viewBox="0 0 14 14"
                                        fill="none" stroke="currentColor"
                                        stroke-width="1.2" stroke-dasharray="1.6 1.4" {
                                        path d="M1 3.5h4l1 1.2h7v7.8h-12z" {}
                                    }
                                } @else if entry.is_up() {
                                    svg width="14" height="14" viewBox="0 0 14 14"
                                        fill="none" stroke="currentColor"
                                        stroke-width="1.4" stroke-linecap="round" {
                                        path d="M3 7l4-4 4 4M7 3v9" {}
                                    }
                                } @else {
                                    svg width="14" height="14" viewBox="0 0 14 14"
                                        fill="none" stroke="currentColor"
                                        stroke-width="1.2" stroke-linejoin="round" {
                                        path d="M3 1.5h5l3 3v8.5h-8z" {}
                                        path d="M8 1.5v3h3" {}
                                    }
                                }
                            }
                            @if entry.is_up() {
                                a class="tree-name tree-name--up" href=(self.parent_url()) { ".." }
                                span {}
                                span {}
                            } @else if entry.is_dir_like() {
                                a class="tree-name tree-name--dir"
                                  href=(self.dir_entry_url(&entry.name)) {
                                    (entry.name) "/"
                                }
                                span class="tree-msg" { (entry.last_msg) }
                                span class="tree-age" { (entry.age) }
                            } @else {
                                a class="tree-name" href=(self.dir_entry_url(&entry.name)) {
                                    (entry.name)
                                }
                                span class="tree-msg" { (entry.last_msg) }
                                span class="tree-age" { (entry.age) }
                            }
                        }
                    }
                }
                aside class="tree-sidebar" {
                    @for change in &self.recent_changes {
                        a class="tree-log-item" href=(format!("/{}/log", self.repo)) {
                            div class="tree-log-subject" { (change.description) }
                            div class="tree-log-meta" {
                                span class="tree-log-sha" {
                                    (change.change_head()) (change.change_tail())
                                }
                                span class="tree-log-age" { (change.age) }
                            }
                        }
                    }
                    a class="tree-log-more" href=(format!("/{}/log", self.repo)) { "log →" }
                }
            }
        };
        base(
            &format!("tree · {}", self.repo),
            page_nav(&self.repo, self.crumbs.as_deref()),
            body,
        )
    }

    fn parent_url(&self) -> String {
        if self.path.is_empty() {
            return format!("/{}", self.repo);
        }
        match self.path.rfind('/') {
            Some(idx) => format!("/{}/tree/{}", self.repo, &self.path[..idx]),
            None => format!("/{}/tree", self.repo),
        }
    }

    fn dir_entry_url(&self, name: &str) -> String {
        if self.path.is_empty() {
            format!("/{}/tree/{}", self.repo, name)
        } else {
            format!("/{}/tree/{}/{}", self.repo, self.path, name)
        }
    }

    fn sha_head(&self) -> &str {
        &self.sha_short[..self.sha_short.len().min(4)]
    }

    fn sha_tail(&self) -> &str {
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

// ── Commit view ───────────────────────────────────────────────────

pub struct CommitTemplate {
    pub repo: String,
    pub crumbs: Option<Vec<Crumb>>,
    pub sections: Vec<SectionLink>,
    pub sha: String,
    pub sha_short: String,
    pub sha_head: String,
    pub sha_tail: String,
    pub author: String,
    pub email: String,
    pub date_relative: String,
    pub date_iso: String,
    pub subject: String,
    pub body: String,
    pub parents: Vec<CommitParent>,
    pub diff: String,
}

impl CommitTemplate {
    pub fn render(&self) -> Markup {
        let body = html! {
            nav class="repo-section-nav" {
                (section_nav_links(&self.sections))
                span class="repo-position" {
                    span class="change-id" title=(self.sha) {
                        span class="change-head" { (self.sha_head) }
                        span class="change-tail" { (self.sha_tail) }
                    }
                }
            }
            div class="commit-body" {
                article class="commit-detail" {
                    div class="commit-meta" {
                        div class="commit-subject" { (self.subject) }
                        @if !self.body.is_empty() {
                            pre class="commit-body-text" { (self.body) }
                        }
                        div class="commit-byline" {
                            span class="commit-author" { (self.author) }
                            " <" (self.email) ">"
                            span class="repo-meta-dot" { "·" }
                            time title=(self.date_iso) { (self.date_relative) }
                        }
                        @if !self.parents.is_empty() {
                            div class="commit-parents" {
                                "parent"
                                @if self.parents.len() > 1 { "s" }
                                ": "
                                @for (i, p) in self.parents.iter().enumerate() {
                                    @if i > 0 { ", " }
                                    a class="change-id" href=(p.commit_url)
                                      title=(p.sha_full()) {
                                        span class="change-head" { (p.sha_head()) }
                                        span class="change-tail" { (p.sha_tail()) }
                                    }
                                }
                            }
                        }
                    }
                    @if !self.diff.is_empty() {
                        pre class="commit-diff" { (self.diff) }
                    }
                }
            }
        };
        base(
            &format!("{} · {}", self.sha_short, self.repo),
            page_nav(&self.repo, self.crumbs.as_deref()),
            body,
        )
    }
}

pub struct CommitParent {
    pub sha: String,
    pub commit_url: String,
}

impl CommitParent {
    pub fn sha_full(&self) -> &str {
        &self.sha
    }

    pub fn sha_head(&self) -> &str {
        &self.sha[..self.sha.len().min(4)]
    }

    pub fn sha_tail(&self) -> &str {
        let start = self.sha.len().min(4);
        &self.sha[start..self.sha.len().min(8)]
    }
}

// ── Commit log ────────────────────────────────────────────────────

pub struct LogTemplate {
    pub repo: String,
    pub crumbs: Option<Vec<Crumb>>,
    pub sections: Vec<SectionLink>,
    pub changes: Vec<ChangeRow>,
    pub bookmark: String,
    pub sha_short: String,
}

impl LogTemplate {
    pub fn render(&self) -> Markup {
        let body = html! {
            nav class="repo-section-nav" {
                (section_nav_links(&self.sections))
                span class="repo-position" {
                    span class="bookmark-glyph" { "※" }
                    span class="bookmark-name" { (self.bookmark) }
                    span class="repo-meta-sep" { "→" }
                    span class="change-id" title=(self.sha_short) {
                        span class="change-head" { (self.sha_head()) }
                        span class="change-tail" { (self.sha_tail()) }
                    }
                }
            }
            div class="log-body" {
                @if self.changes.is_empty() {
                    p class="log-empty" { "no commits yet" }
                } @else {
                    @for change in &self.changes {
                        div class="log-row" {
                            a class="log-sha"
                              href=(format!("/{}/log", self.repo))
                              title=(format!("commit {}", change.sha_full())) {
                                span class="change-head" { (change.change_head()) }
                                span class="change-tail" { (change.change_tail()) }
                            }
                            a class="log-subject" href=(format!("/{}/log", self.repo)) {
                                @if change.description.is_empty() {
                                    span class="no-desc" { "(no description set)" }
                                } @else {
                                    (change.description)
                                }
                            }
                            span class="log-age" {
                                time { (change.age) }
                            }
                        }
                    }
                }
            }
        };
        base(
            &format!("log · {}", self.repo),
            page_nav(&self.repo, self.crumbs.as_deref()),
            body,
        )
    }

    fn sha_head(&self) -> &str {
        &self.sha_short[..self.sha_short.len().min(4)]
    }

    fn sha_tail(&self) -> &str {
        let start = self.sha_short.len().min(4);
        &self.sha_short[start..]
    }
}
// ── Error ──────────────────────────────────────────────────────────

pub struct ErrorTemplate {
    pub repo: String,
    pub crumbs: Option<Vec<Crumb>>,
    pub title: String,
    pub detail: String,
}

impl ErrorTemplate {
    pub fn render(&self) -> Markup {
        base(
            &self.title,
            page_nav(&self.repo, self.crumbs.as_deref()),
            html! {
                main class="page-main" {
                    p class="error-title" { (self.title) }
                    pre class="error-detail" { (self.detail) }
                }
            },
        )
    }
}

// ── File view ─────────────────────────────────────────────────────

pub struct FileViewTemplate {
    pub repo: String,
    pub crumbs: Option<Vec<Crumb>>,
    pub sections: Vec<SectionLink>,
    pub path: String,
    pub bookmark: String,
    pub sha_short: String,
    pub sha_head: String,
    pub sha_tail: String,
    pub last_change_sha: String,
    pub last_change_head: String,
    pub last_change_tail: String,
    pub last_change_msg: String,
    pub last_change_author: String,
    pub last_change_age: String,
    pub line_count: usize,
    pub file_size: String,
    pub language: String,
    pub mode: String,
    pub encoding: String,
    pub line_ending: String,
    pub line_nums: Vec<usize>,
    pub lines: Vec<String>,
}

impl FileViewTemplate {
    pub fn render(&self) -> Markup {
        let body = html! {
            nav class="repo-section-nav" {
                (section_nav_links(&self.sections))
                span class="repo-position" {
                    span class="bookmark-glyph" { "※" }
                    span class="bookmark-name" { (self.bookmark) }
                    span class="repo-meta-sep" { "→" }
                    span class="change-id" title=(self.sha_short) {
                        span class="change-head" { (self.sha_head) }
                        span class="change-tail" { (self.sha_tail) }
                    }
                }
            }
            div class="tree-body" {
                div class="file-code-col" {
                    div class="code-surface" {
                        div class="code-gutter" {
                            @for line_num in &self.line_nums {
                                div class="code-line-num" { (line_num) }
                            }
                        }
                        pre class="code-body" {
                            code "data-lang"=(self.language) {
                                @for line in &self.lines {
                                    (PreEscaped(line))
                                }
                            }
                        }
                    }
                }
                aside class="tree-sidebar" {
                    div class="side-block" {
                        div class="side-block-title" { "File" }
                        div class="file-info-list" {
                            div class="file-info-row" {
                                span class="file-info-key" { "lines" }
                                span class="file-info-val" { (self.line_count) }
                            }
                            div class="file-info-row" {
                                span class="file-info-key" { "size" }
                                span class="file-info-val" { (self.file_size) }
                            }
                            div class="file-info-row" {
                                span class="file-info-key" { "lang" }
                                span class="file-info-val" { (self.language) }
                            }
                            div class="file-info-row" {
                                span class="file-info-key" { "mode" }
                                span class="file-info-val" { (self.mode) }
                            }
                            div class="file-info-row" {
                                span class="file-info-key" { "encoding" }
                                span class="file-info-val" {
                                    (self.encoding) ", " (self.line_ending)
                                }
                            }
                        }
                    }
                    div class="side-block" {
                        div class="side-block-title" { "Last change" }
                        div class="file-last-change-side" {
                            a class="change-id"
                              href=(format!("/{}/log", self.repo))
                              title=(format!("commit {}", self.last_change_sha)) {
                                span class="change-head" { (self.last_change_head) }
                                span class="change-tail" { (self.last_change_tail) }
                            }
                            div class="file-last-change-msg" { (self.last_change_msg) }
                            div class="file-last-change-meta" {
                                span class="file-last-change-author" { (self.last_change_author) }
                                span class="file-last-change-age" { (self.last_change_age) }
                            }
                        }
                    }
                    div class="side-block side-block--last" {
                        div class="side-block-title" { "Actions" }
                        div class="file-actions-list" {
                            a class="file-action-row"
                              href=(format!("/{}/raw/{}", self.repo, self.path)) { "raw" }
                            a class="file-action-row"
                              href=(format!("/{}/blame/{}", self.repo, self.path)) { "blame" }
                            a class="file-action-row"
                              href=(format!("/{}/log/{}", self.repo, self.path)) { "history" }
                        }
                    }
                }
            }
        };
        base(
            &format!("file · {} · {}", self.repo, self.path),
            page_nav(&self.repo, self.crumbs.as_deref()),
            body,
        )
    }
}
