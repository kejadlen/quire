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

pub(super) struct RepoView<'a> {
    repo: &'a Repo,
}

impl<'a> RepoView<'a> {
    pub(super) fn new(repo: &'a Repo) -> Self {
        Self { repo }
    }

    /// Run a git command in the repo, returning trimmed stdout or `None` on
    /// failure or empty output.
    pub(super) fn run(&self, args: &[&str]) -> Option<String> {
        let output = self.repo.git(args).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let s = String::from_utf8(output.stdout).ok()?;
        let s = s.trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    }

    /// Read all summary data from the repo for the home page.
    pub(super) fn read_all(&self, repo: &str) -> GitData {
        (
            self.head_info(),
            self.readme(),
            self.bookmarks(),
            self.tags(),
            self.recent_changes(repo),
        )
    }

    pub(super) fn head_info(&self) -> Option<HeadInfo> {
        let bookmark = self
            .run(&["symbolic-ref", "--short", "HEAD"])
            .unwrap_or_else(|| "main".to_string());
        // %H = full sha, %s = subject, %cr = committer date relative
        let log = self.run(&["log", "-1", "--format=%H%n%s%n%cr"])?;
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

    pub(super) fn readme(&self) -> Option<String> {
        let candidates = ["HEAD:README.md", "HEAD:readme.md", "HEAD:README"];
        for candidate in &candidates {
            if let Some(raw) = self.run(&["show", candidate]) {
                return Some(Self::render_markdown(&raw));
            }
        }
        None
    }

    pub(super) fn bookmarks(&self) -> Vec<BookmarkRow> {
        let out = self
            .run(&[
                "for-each-ref",
                "--format=%(refname:short)|%(objectname:short)|%(committerdate:relative)",
                "--sort=-committerdate",
                "refs/heads/",
            ])
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

    pub(super) fn tags(&self) -> Vec<TagRow> {
        let out = self
            .run(&[
                "for-each-ref",
                "--format=%(refname:short)|%(committerdate:relative)",
                "--sort=-creatordate",
                "refs/tags/",
            ])
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

    pub(super) fn recent_changes(&self, repo: &str) -> Vec<ChangeRow> {
        self.recent_changes_for(None, repo)
    }

    pub(super) fn recent_changes_for(&self, path: Option<&str>, repo: &str) -> Vec<ChangeRow> {
        let mut args = vec!["log", "-12", "--format=%H|%s|%cr"];
        if let Some(p) = path
            && !p.is_empty()
        {
            args.push("--");
            args.push(p);
        }
        let out = self.run(&args).unwrap_or_default();

        out.lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '|');
                let sha = parts.next()?.to_string();
                Some(ChangeRow {
                    commit_url: format!("/{repo}/commits/{sha}"),
                    sha,
                    description: parts.next().unwrap_or("").to_string(),
                    age: parts.next().unwrap_or("").to_string(),
                })
            })
            .collect()
    }

    fn render_markdown(markdown: &str) -> String {
        use pulldown_cmark::{Options, Parser, html};
        let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
        let parser = Parser::new_ext(markdown, opts);
        let mut output = String::new();
        html::push_html(&mut output, parser);
        output
    }
}
