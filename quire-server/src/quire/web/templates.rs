//! Maud template rendering.

use maud::{DOCTYPE, Markup, PreEscaped, html};

use super::format;

fn pkg_version() -> &'static str {
    env!("QUIRE_VERSION")
}

// ── Commit ID ─────────────────────────────────────────────────────────

/// A git commit SHA paired with an optional jj change ID.
///
/// Display methods prefer the change ID when present, falling back to the
/// git SHA. Used wherever a commit badge or reference is shown in templates.
pub struct CommitId {
    pub sha: String,
    pub change_id: Option<String>,
}

impl CommitId {
    pub fn new(sha: String, change_id: Option<String>) -> Self {
        Self { sha, change_id }
    }

    fn display(&self) -> &str {
        self.change_id.as_deref().unwrap_or(self.sha.as_str())
    }

    /// First 4 chars of the display ID (bold prefix in commit badges).
    pub fn head(&self) -> &str {
        let s = self.display();
        &s[..s.len().min(4)]
    }

    /// Chars 4–8 of the display ID (dimmed suffix in commit badges).
    pub fn tail(&self) -> &str {
        let s = self.display();
        let start = s.len().min(4);
        &s[start..s.len().min(8)]
    }

    /// First 8 chars of the git SHA, used for tooltips and secondary display.
    pub fn sha_short(&self) -> &str {
        &self.sha[..self.sha.len().min(8)]
    }

    pub fn sha_full(&self) -> &str {
        &self.sha
    }
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
            script src="/quire-app.js" {}
        }
    }
}

// ── Run list ───────────────────────────────────────────────────────

pub fn run_list(
    repo: &str,
    crumbs: Option<&[Crumb]>,
    runs: &[RunListRow],
    sections: &[SectionLink],
) -> Markup {
    let body = html! {
        nav class="repo-section-nav" {
            (section_nav_links(sections))
        }
        div class="repo-body" {
            article class="ci-run-list" {
                @if runs.is_empty() {
                    p class="ci-empty" { "no runs yet" }
                } @else {
                    @for run in runs {
                        div class="ci-run-row" {
                            span class=(format!("ci-status-dot {}", run.state_class())) {}
                            a class="ci-commit-link"
                              href=(format!("/{}/ci/{}", repo, run.id)) {
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
    base(&format!("ci · {repo}"), page_nav(repo, crumbs), body)
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

pub fn run_detail(
    repo: &str,
    crumbs: Option<&[Crumb]>,
    run: &DetailRun,
    jobs: &[DetailJob],
    quire_ci_log: &str,
    sections: &[SectionLink],
) -> Markup {
    let body = html! {
        nav class="repo-section-nav" {
            (section_nav_links(sections))
            span class="repo-position" {
                span class="ci-commit-link" { (run.sha_short()) }
                span class="repo-meta-dot" { "·" }
                span { (run.branch_short()) }
                span class="repo-meta-dot" { "·" }
                span class=(format!("ci-state-label {}", run.state_class())) {
                    span class=(format!("ci-status-dot {}", run.state_class())) {}
                    " " (run.state())
                }
            }
        }
        div class="repo-body" {
            article class="ci-detail" {
                div class="ci-meta" {
                    @if run.is_terminal() {
                        "queued "
                        time title=(run.queued_iso()) { (run.queued_relative()) }
                        span class="repo-meta-dot" { "·" }
                        span title=(format!("started {}\nfinished {}",
                            run.started_iso(), run.finished_iso())) {
                            "ran " (run.duration_display())
                        }
                    } @else if run.has_started() {
                        "queued "
                        time title=(run.queued_iso()) { (run.queued_relative()) }
                        span class="repo-meta-dot" { "·" }
                        "started "
                        time title=(run.started_iso()) { (run.started_display()) }
                    } @else {
                        "queued "
                        time title=(run.queued_iso()) { (run.queued_relative()) }
                    }
                }
                @if jobs.is_empty() {
                    div class="ci-empty" { "no jobs recorded" }
                } @else {
                    @for job in jobs {
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
                @if !quire_ci_log.is_empty() {
                    div class="ci-job" {
                        div class="ci-job-header" { "quire-ci" }
                        pre class="ci-sh-log" { (quire_ci_log) }
                    }
                }
            }
            aside class="repo-sidebar" {}
        }
    };
    base(
        &format!("ci · {} · {}", repo, run.sha_short()),
        page_nav(repo, crumbs),
        body,
    )
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

pub fn repo_list(repos: &[ListedRepo]) -> Markup {
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
            @if repos.is_empty() {
                p class="repo-list-empty" { "no repositories" }
            } @else {
                @for repo in repos {
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

pub struct ListedRepo {
    pub name: String,
    pub description: Option<String>,
}

// ── Repo Home ──────────────────────────────────────────────────────

pub fn repo_home(
    repo: &str,
    crumbs: Option<&[Crumb]>,
    head: Option<&HeadInfo>,
    readme_html: Option<&str>,
    recent_runs: &[RunListRow],
    recent_changes: &[ChangeRow],
    sections: &[SectionLink],
) -> Markup {
    let latest_ci_state = || recent_runs.first().map(|r| r.state()).unwrap_or("none");
    let latest_ci_state_class = || recent_runs.first().map(|r| r.state_class()).unwrap_or("");

    let body = html! {
        nav class="repo-section-nav" {
            (section_nav_links(sections))
            @if let Some(h) = head {
                span class="repo-position" {
                    span class="bookmark-glyph" { "※" }
                    span class="bookmark-name" { (h.bookmark) }
                    span class="repo-meta-sep" { "→" }
                    a class="change-id" href=(format!("/{}/commits/{}", repo, h.id.sha))
                      title=(format!("commit {}", h.id.sha_short())) {
                        span class="change-head" { (h.id.head()) }
                        span class="change-tail" { (h.id.tail()) }
                    }
                    span class="commit-id-secondary" { (h.id.sha_short()) }
                    span class="repo-meta-dot" { "·" }
                    span class="repo-position-age" { (h.age) }
                    @if !recent_runs.is_empty() {
                        span class="repo-meta-dot" { "·" }
                        span class="ci-inline" {
                            span class=(format!("ci-status-dot {}", latest_ci_state_class())) {}
                            span { "ci " (latest_ci_state()) }
                        }
                    }
                }
            }
        }
        div class="repo-body" {
            article class="repo-readme" {
                @if let Some(html) = readme_html {
                    div class="readme-content" { (PreEscaped(html)) }
                } @else {
                    p class="readme-empty" { "no readme" }
                }
            }
            aside class="repo-sidebar" {
                div class="side-block" {
                    div class="side-block-title" { "Clone" }
                    div class="clone-url" { "https://quire.local/" (repo) ".git" }
                }
                @if !recent_runs.is_empty() {
                    div class="side-block" {
                        div class="side-block-title" { "CI" }
                        div class="ci-mini-list" {
                            @for run in recent_runs {
                                div class="ci-mini-row" {
                                    span class=(format!("ci-status-dot {}", run.state_class())) {}
                                    a class="ci-mini-link"
                                      href=(format!("/{}/ci/{}", repo, run.id)) {
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
                        a class="side-more" href=(format!("/{}/ci", repo)) {
                            "all runs →"
                        }
                    }
                }
                @if !recent_changes.is_empty() {
                    div class="side-block side-block--last" {
                        div class="side-block-title" { "Recent changes" }
                        div class="change-mini-list" {
                            @for ch in recent_changes {
                                div class="change-mini-row" {
                                    div class="change-mini-header" {
                                        a class="change-id"
                                          href=(ch.commit_url)
                                          title=(format!("commit {}", ch.id.sha_full())) {
                                            span class="change-head" { (ch.id.head()) }
                                            span class="change-tail" { (ch.id.tail()) }
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
                        a class="side-more" href=(format!("/{}/log", repo)) {
                            "full log →"
                        }
                    }
                }
            }
        }
    };
    base(repo, page_nav(repo, crumbs), body)
}

pub struct HeadInfo {
    pub id: CommitId,
    pub description: String,
    pub age: String,
    pub bookmark: String,
}

pub struct ChangeRow {
    pub id: CommitId,
    pub description: String,
    pub age: String,
    pub commit_url: String,
}

// ── Config ─────────────────────────────────────────────────────────

pub fn config(crumbs: Option<&[Crumb]>, cfg: &crate::GlobalConfig) -> Markup {
    let mut sorted_secrets: Vec<_> = cfg.secrets.iter().collect();
    sorted_secrets.sort_by_key(|(k, _)| *k);

    let _ = crumbs; // config page uses its own nav; crumbs not rendered
    let body = html! {
        nav class="repo-section-nav" {
            span class="repo-position" { "config" }
        }
        div class="repo-body" {
            article class="config-table" {
                div class="config-row" {
                    span class="config-key" { "port" }
                    span class="config-val" { (cfg.port) }
                }
                div class="config-row" {
                    span class="config-key" { "ci.executor" }
                    span class="config-val" { (cfg.ci.executor) }
                }
                @if let Some(sentry) = &cfg.sentry {
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
                @for (key, value) in &sorted_secrets {
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

// ── Tree view ─────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub fn tree(
    repo: &str,
    crumbs: Option<&[Crumb]>,
    sections: &[SectionLink],
    path: &str,
    bookmark: &str,
    head: &CommitId,
    entries: &[TreeEntry],
    recent_changes: &[ChangeRow],
) -> Markup {
    let parent_url = || -> String {
        if path.is_empty() {
            return format!("/{repo}");
        }
        match path.rfind('/') {
            Some(idx) => format!("/{}/tree/{}", repo, &path[..idx]),
            None => format!("/{}/tree", repo),
        }
    };

    let dir_entry_url = |name: &str| -> String {
        if path.is_empty() {
            format!("/{}/tree/{}", repo, name)
        } else {
            format!("/{}/tree/{}/{}", repo, path, name)
        }
    };

    let body = html! {
        nav class="repo-section-nav" {
            (section_nav_links(sections))
            span class="repo-position" {
                span class="bookmark-glyph" { "※" }
                span class="bookmark-name" { (bookmark) }
                span class="repo-meta-sep" { "→" }
                span class="change-id" title=(head.sha_short()) {
                    span class="change-head" { (head.head()) }
                    span class="change-tail" { (head.tail()) }
                }
                @if !path.is_empty() {
                    span class="repo-meta-dot" { "·" }
                    span class="repo-position-path" { (path) }
                }
            }
        }
        div class="tree-body" {
            div class="tree-table-col" {
                @for entry in entries {
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
                            a class="tree-name tree-name--up" href=(parent_url()) { ".." }
                            span {}
                            span {}
                        } @else if entry.is_dir_like() {
                            a class="tree-name tree-name--dir"
                              href=(dir_entry_url(&entry.name)) {
                                (entry.name) "/"
                            }
                            span class="tree-msg" { (entry.last_msg) }
                            span class="tree-age" { (entry.age) }
                        } @else {
                            a class="tree-name" href=(dir_entry_url(&entry.name)) {
                                (entry.name)
                            }
                            span class="tree-msg" { (entry.last_msg) }
                            span class="tree-age" { (entry.age) }
                        }
                    }
                }
            }
            aside class="tree-sidebar" {
                @for change in recent_changes {
                    a class="tree-log-item" href=(format!("/{}/log", repo)) {
                        div class="tree-log-subject" { (change.description) }
                        div class="tree-log-meta" {
                            span class="tree-log-sha" {
                                (change.id.head()) (change.id.tail())
                            }
                            span class="tree-log-age" { (change.age) }
                        }
                    }
                }
                a class="tree-log-more" href=(format!("/{}/log", repo)) { "log →" }
            }
        }
    };
    base(&format!("tree · {repo}"), page_nav(repo, crumbs), body)
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

#[allow(clippy::too_many_arguments)]
pub fn commit(
    repo: &str,
    crumbs: Option<&[Crumb]>,
    sections: &[SectionLink],
    sha: &str,
    sha_short: &str,
    sha_head: &str,
    sha_tail: &str,
    author: &str,
    email: &str,
    date_relative: &str,
    date_iso: &str,
    subject: &str,
    body_text: &str,
    parents: &[CommitParent],
    diff: &str,
    change_id: &str,
) -> Markup {
    let change_id_head = &change_id[..change_id.len().min(12)];
    let change_id_tail = &change_id[change_id.len().min(12)..];
    let body = html! {
        nav class="repo-section-nav" {
            (section_nav_links(sections))
            span class="repo-position" {
                span class="change-id" title=(sha) {
                    span class="change-head" { (sha_head) }
                    span class="change-tail" { (sha_tail) }
                }
                span class="repo-meta-dot" { "·" }
                span { (author) }
                span class="repo-meta-dot" { "·" }
                span { time title=(date_iso) { (date_relative) } }
            }
        }
        div class="repo-body" {
            article class="commit-detail" {
                div class="commit-message" {
                    div class="commit-subject" { (subject) }
                    @if !body_text.is_empty() {
                        pre class="commit-body" { (body_text) }
                    }
                }
                div class="commit-meta-list" {
                    @if !change_id.is_empty() {
                        div class="commit-meta-row" {
                            span class="commit-meta-key" { "change" }
                            span class="commit-meta-val" {
                                span class="change-id-full" {
                                    span class="change-id-head" { (change_id_head) }
                                    span class="change-id-tail" { (change_id_tail) }
                                }
                            }
                        }
                    }
                    div class="commit-meta-row" {
                        span class="commit-meta-key" { "commit" }
                        span class="commit-meta-val commit-meta-val--secondary" { (sha) }
                    }
                    div class="commit-meta-row" {
                        span class="commit-meta-key" { "author" }
                        span class="commit-meta-val" { (author) " <" (email) ">" }
                    }
                    div class="commit-meta-row" {
                        span class="commit-meta-key" { "date" }
                        span class="commit-meta-val" { time title=(date_iso) { (date_relative) } }
                    }
                    @if !parents.is_empty() {
                        div class="commit-meta-row" {
                            span class="commit-meta-key" {
                                "parent"
                                @if parents.len() > 1 { "s" }
                            }
                            span class="commit-meta-val" {
                                @for (i, p) in parents.iter().enumerate() {
                                    @if i > 0 { " " }
                                    a class="change-id" href=(p.commit_url)
                                      title=(format!("commit {}", p.id.sha_full())) {
                                        span class="change-head" { (p.id.head()) }
                                        span class="change-tail" { (p.id.tail()) }
                                    }
                                }
                            }
                        }
                    }
                }
                @if !diff.is_empty() {
                    pre class="commit-diff" { (diff) }
                }
            }
            aside class="repo-sidebar" {}
        }
    };
    base(
        &format!("{sha_short} · {repo}"),
        page_nav(repo, crumbs),
        body,
    )
}

pub struct CommitParent {
    pub id: CommitId,
    pub commit_url: String,
}

// ── Commit log ────────────────────────────────────────────────────

pub fn log(
    repo: &str,
    crumbs: Option<&[Crumb]>,
    sections: &[SectionLink],
    changes: &[ChangeRow],
    bookmark: &str,
    head: &CommitId,
) -> Markup {
    let body = html! {
        nav class="repo-section-nav" {
            (section_nav_links(sections))
            span class="repo-position" {
                span class="bookmark-glyph" { "※" }
                span class="bookmark-name" { (bookmark) }
                span class="repo-meta-sep" { "→" }
                span class="change-id" title=(head.sha_short()) {
                    span class="change-head" { (head.head()) }
                    span class="change-tail" { (head.tail()) }
                }
            }
        }
        div class="log-body" {
            @if changes.is_empty() {
                p class="log-empty" { "no commits yet" }
            } @else {
                @for change in changes {
                    div class="log-row" {
                        a class="log-sha"
                          href=(change.commit_url)
                          title=(format!("commit {}", change.id.sha_full())) {
                            span class="change-head" { (change.id.head()) }
                            span class="change-tail" { (change.id.tail()) }
                        }
                        a class="log-subject" href=(change.commit_url) {
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
    base(&format!("log · {repo}"), page_nav(repo, crumbs), body)
}

// ── Error ──────────────────────────────────────────────────────────

pub fn error(repo: &str, crumbs: Option<&[Crumb]>, title: &str, detail: &str) -> Markup {
    base(
        title,
        page_nav(repo, crumbs),
        html! {
            main class="page-main" {
                p class="error-title" { (title) }
                pre class="error-detail" { (detail) }
            }
        },
    )
}

// ── File view ─────────────────────────────────────────────────────

pub struct FileView {
    pub repo: String,
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

pub fn file_view(data: &FileView, sections: &[SectionLink], crumbs: Option<&[Crumb]>) -> Markup {
    let body = html! {
        nav class="repo-section-nav" {
            (section_nav_links(sections))
            span class="repo-position" {
                span class="bookmark-glyph" { "※" }
                span class="bookmark-name" { (data.bookmark) }
                span class="repo-meta-sep" { "→" }
                span class="change-id" title=(data.sha_short) {
                    span class="change-head" { (data.sha_head) }
                    span class="change-tail" { (data.sha_tail) }
                }
            }
        }
        div class="tree-body" {
            div class="file-code-col" {
                div class="code-surface" {
                    div class="code-gutter" {
                        @for line_num in &data.line_nums {
                            div class="code-line-num" { (line_num) }
                        }
                    }
                    pre class="code-body" {
                        code "data-lang"=(data.language) {
                            @for line in &data.lines {
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
                            span class="file-info-val" { (data.line_count) }
                        }
                        div class="file-info-row" {
                            span class="file-info-key" { "size" }
                            span class="file-info-val" { (data.file_size) }
                        }
                        div class="file-info-row" {
                            span class="file-info-key" { "lang" }
                            span class="file-info-val" { (data.language) }
                        }
                        div class="file-info-row" {
                            span class="file-info-key" { "mode" }
                            span class="file-info-val" { (data.mode) }
                        }
                        div class="file-info-row" {
                            span class="file-info-key" { "encoding" }
                            span class="file-info-val" {
                                (data.encoding) ", " (data.line_ending)
                            }
                        }
                    }
                }
                div class="side-block" {
                    div class="side-block-title" { "Last change" }
                    div class="file-last-change-side" {
                        a class="change-id"
                          href=(format!("/{}/log", data.repo))
                          title=(format!("commit {}", data.last_change_sha)) {
                            span class="change-head" { (data.last_change_head) }
                            span class="change-tail" { (data.last_change_tail) }
                        }
                        div class="file-last-change-msg" { (data.last_change_msg) }
                        div class="file-last-change-meta" {
                            span class="file-last-change-author" { (data.last_change_author) }
                            span class="file-last-change-age" { (data.last_change_age) }
                        }
                    }
                }
                div class="side-block side-block--last" {
                    div class="side-block-title" { "Actions" }
                    div class="file-actions-list" {
                        a class="file-action-row"
                          href=(format!("/{}/raw/{}", data.repo, data.path)) { "raw" }
                        a class="file-action-row"
                          href=(format!("/{}/blame/{}", data.repo, data.path)) { "blame" }
                        a class="file-action-row"
                          href=(format!("/{}/log/{}", data.repo, data.path)) { "history" }
                    }
                }
            }
        }
    };
    base(
        &format!("file · {} · {}", data.repo, data.path),
        page_nav(&data.repo, crumbs),
        body,
    )
}
