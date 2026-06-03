//! Shared git-reading helpers used by multiple handlers.

use super::super::templates::{BookmarkRow, ChangeRow, HeadInfo, TagRow};
use crate::quire::Repo;

pub(super) type GitData = (
    Option<HeadInfo>,
    Option<String>,
    Vec<BookmarkRow>,
    Vec<TagRow>,
    Vec<ChangeRow>,
);

/// Run a git command in `repo`, returning trimmed stdout or `None` on failure
/// or empty output.
pub(super) fn run_git(repo: &Repo, args: &[&str]) -> Option<String> {
    let output = repo.git(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Read summary data from a bare git repository for the repo home page.
pub(super) fn read_git_data(repo: &Repo) -> GitData {
    let head = read_head_info(repo);
    let readme_html = read_readme(repo);
    let bookmarks = read_bookmarks(repo);
    let tags = read_tags(repo);
    let recent_changes = read_recent_changes(repo);
    (head, readme_html, bookmarks, tags, recent_changes)
}

pub(super) fn read_head_info(repo: &Repo) -> Option<HeadInfo> {
    let bookmark =
        run_git(repo, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_else(|| "main".to_string());
    // %H = full sha, %s = subject, %ar = relative age
    let log = run_git(repo, &["log", "-1", "--format=%H%n%s%n%ar"])?;
    let mut lines = log.lines();
    let sha = lines.next()?.to_string();
    let description = lines.next().unwrap_or("").to_string();
    let age = lines.next().unwrap_or("").to_string();
    Some(HeadInfo {
        sha,
        description,
        age,
        bookmark,
    })
}

pub(super) fn read_readme(repo: &Repo) -> Option<String> {
    let candidates = ["HEAD:README.md", "HEAD:readme.md", "HEAD:README"];
    for candidate in &candidates {
        if let Some(raw) = run_git(repo, &["show", candidate]) {
            return Some(render_markdown(&raw));
        }
    }
    None
}

fn render_markdown(markdown: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(markdown, opts);
    let mut output = String::new();
    html::push_html(&mut output, parser);
    output
}

pub(super) fn read_bookmarks(repo: &Repo) -> Vec<BookmarkRow> {
    let out = run_git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname:short)|%(objectname:short)|%(committerdate:relative)",
            "--sort=-committerdate",
            "refs/heads/",
        ],
    )
    .unwrap_or_default();

    out.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '|');
            Some(BookmarkRow {
                name: parts.next()?.to_string(),
                sha_short: parts.next()?.to_string(),
                age: parts.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

pub(super) fn read_tags(repo: &Repo) -> Vec<TagRow> {
    let out = run_git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname:short)|%(committerdate:relative)",
            "--sort=-creatordate",
            "refs/tags/",
        ],
    )
    .unwrap_or_default();

    out.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '|');
            Some(TagRow {
                name: parts.next()?.to_string(),
                age: parts.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

pub(super) fn read_recent_changes(repo: &Repo) -> Vec<ChangeRow> {
    let out = run_git(repo, &["log", "-12", "--format=%H|%s|%ar"]).unwrap_or_default();

    out.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '|');
            Some(ChangeRow {
                sha: parts.next()?.to_string(),
                description: parts.next().unwrap_or("").to_string(),
                age: parts.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}
