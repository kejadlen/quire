//! Handler for the repository tree browser.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::super::db;
use super::super::templates::{Crumb, PathCommit, TreeEntry, TreeEntryKind, TreeTemplate};
use super::git::{read_bookmarks, read_tags, run_git};
use super::render;
use crate::Quire;
use crate::quire::Repo;

pub async fn tree_view(State(quire): State<Quire>, AxumPath(repo): AxumPath<String>) -> Response {
    tree_at_path(quire, repo, String::new()).await
}

pub async fn tree_view_path(
    State(quire): State<Quire>,
    AxumPath((repo, path)): AxumPath<(String, String)>,
) -> Response {
    tree_at_path(quire, repo, path).await
}

async fn tree_at_path(quire: Quire, repo: String, path: String) -> Response {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = db::resolve_repo_name(&repo);
    let git_repo = match quire.repo(&repo_name) {
        Ok(r) if r.exists() => r,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let path_clone = path.clone();
    let result = tokio::task::spawn_blocking(move || {
        let tree_data = read_tree_data(&git_repo, &path_clone)?;
        let bookmarks = read_bookmarks(&git_repo);
        let tags = read_tags(&git_repo);
        Some((tree_data, bookmarks, tags))
    })
    .await
    .unwrap_or(None);

    let (tree_data, bookmarks, tags) = match result {
        Some(v) => v,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let crumbs = {
        let mut c = vec![Crumb::with_href("tree", format!("/{}/tree", repo_display))];
        if !path.is_empty() {
            c.push(Crumb::new(
                path.split('/').last().unwrap_or(&path).to_string(),
            ));
        }
        c
    };

    let tmpl = TreeTemplate {
        repo: repo_display,
        crumbs,
        bookmarks,
        tags,
        active_section: "tree".to_string(),
        path,
        bookmark: tree_data.bookmark,
        sha_short: tree_data.sha_short,
        entries: tree_data.entries,
        total_entries: tree_data.total_entries,
        head_commit: tree_data.head_commit,
        readme_preview: tree_data.readme_preview,
    };
    render(&tmpl)
}

struct TreeData {
    bookmark: String,
    sha_short: String,
    entries: Vec<TreeEntry>,
    total_entries: usize,
    head_commit: Option<PathCommit>,
    readme_preview: Option<String>,
}

fn read_tree_data(repo: &Repo, path: &str) -> Option<TreeData> {
    let bookmark =
        run_git(repo, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_else(|| "main".to_string());

    let sha_short =
        run_git(repo, &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());

    let ls_target = if path.is_empty() {
        "HEAD".to_string()
    } else {
        format!("HEAD:{}", path)
    };

    let ls_out = run_git(repo, &["ls-tree", &ls_target])?;

    // Parse ls-tree output: "<mode> <type> <sha>\t<name>"
    let mut raw: Vec<(TreeEntryKind, String)> = Vec::new();
    for line in ls_out.lines() {
        let Some((meta, name)) = line.split_once('\t') else {
            continue;
        };
        let mut parts = meta.split_whitespace();
        let mode = parts.next().unwrap_or("");
        let obj_type = parts.next().unwrap_or("");
        let kind = if mode == "160000" || obj_type == "commit" {
            TreeEntryKind::Submodule
        } else if obj_type == "tree" {
            TreeEntryKind::Dir
        } else {
            TreeEntryKind::File
        };
        raw.push((kind, name.to_string()));
    }

    // Dirs/submodules first, then files; each group alphabetical.
    raw.sort_by(|(ak, an), (bk, bn)| {
        let ao = matches!(ak, TreeEntryKind::Dir | TreeEntryKind::Submodule) as u8;
        let bo = matches!(bk, TreeEntryKind::Dir | TreeEntryKind::Submodule) as u8;
        bo.cmp(&ao).then(an.cmp(bn))
    });

    let total_entries = raw.len();

    let mut entries: Vec<TreeEntry> = Vec::new();

    if !path.is_empty() {
        entries.push(TreeEntry {
            kind: TreeEntryKind::Up,
            name: "..".to_string(),
            last_msg: String::new(),
            age: String::new(),
        });
    }

    for (kind, name) in raw {
        let entry_path = if path.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", path, name)
        };
        let commit_info = run_git(
            repo,
            &["log", "-1", "--format=%s|%ar", "HEAD", "--", &entry_path],
        );
        let (last_msg, age) = commit_info
            .and_then(|s| {
                let mut it = s.splitn(2, '|');
                Some((it.next()?.to_string(), it.next()?.to_string()))
            })
            .unwrap_or_default();
        entries.push(TreeEntry {
            kind,
            name,
            last_msg,
            age,
        });
    }

    let head_commit = {
        let fmt = "--format=%h|%s|%ar|%an";
        let info = if path.is_empty() {
            run_git(repo, &["log", "-1", fmt, "HEAD"])
        } else {
            run_git(repo, &["log", "-1", fmt, "HEAD", "--", path])
        };
        info.and_then(|s| {
            let mut it = s.splitn(4, '|');
            Some(PathCommit {
                sha_short: it.next()?.to_string(),
                description: it.next().unwrap_or("").to_string(),
                age: it.next().unwrap_or("").to_string(),
                author: it.next().unwrap_or("").to_string(),
            })
        })
    };

    let readme_preview = {
        let readme_git_path = if path.is_empty() {
            "HEAD:README.md".to_string()
        } else {
            format!("HEAD:{}/README.md", path)
        };
        run_git(repo, &["show", &readme_git_path]).map(|content| {
            let trimmed = content.trim().to_string();
            if trimmed.len() > 400 {
                format!("{}…", &trimmed[..400])
            } else {
                trimmed
            }
        })
    };

    Some(TreeData {
        bookmark,
        sha_short,
        entries,
        total_entries,
        head_commit,
        readme_preview,
    })
}
