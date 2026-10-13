//! Shared git-reading helpers used by multiple handlers.

use super::super::templates::{ChangeRow, CommitId, HeadInfo, MirrorLink};
use crate::quire::Repo;

pub(super) type GitData = (Option<HeadInfo>, Option<String>, Vec<ChangeRow>);

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
        (self.head_info(), self.readme(), self.recent_changes(repo))
    }

    /// Mirror remotes configured at HEAD, as sidebar links.
    ///
    /// An unreadable or unparsable config yields no links — a broken
    /// config.fnl must not take the repo page down with it — but is logged
    /// so operators see it.
    pub(super) fn mirrors(&self, head: Option<&HeadInfo>) -> Vec<MirrorLink> {
        let Some(head) = head else {
            return Vec::new();
        };
        match self.repo.repo_config(&head.id.sha) {
            Ok(config) => {
                let mut links: Vec<MirrorLink> =
                    config.mirrors.into_keys().map(MirrorLink::new).collect();
                links.sort_by(|a, b| a.label.cmp(&b.label));
                links
            }
            Err(error) => {
                tracing::warn!(
                    error = &error as &(dyn std::error::Error + 'static),
                    "reading repo config for mirrors failed",
                );
                Vec::new()
            }
        }
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
        let change_id = self.change_id(&sha);
        Some(HeadInfo {
            id: CommitId::new(sha, change_id),
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
                let change_id = self.change_id(&sha);
                Some(ChangeRow {
                    commit_url: format!("/{repo}/commits/{sha}"),
                    id: CommitId::new(sha, change_id),
                    description: parts.next().unwrap_or("").to_string(),
                    age: parts.next().unwrap_or("").to_string(),
                })
            })
            .collect()
    }

    /// Read the jj change-id header from a commit object, if present.
    pub(super) fn change_id(&self, sha: &str) -> Option<String> {
        let raw = self.run(&["cat-file", "-p", sha])?;
        for line in raw.lines() {
            if line.is_empty() {
                break; // end of headers
            }
            if let Some(id) = line.strip_prefix("change-id ") {
                return Some(id.trim().to_string());
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
}
