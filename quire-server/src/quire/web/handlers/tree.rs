//! Handler for the repository tree browser and file (blob) view.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::super::auth::Auth;
use super::super::db;
use super::super::templates::{
    Crumb, FileViewTemplate, TreeEntry, TreeEntryKind, TreeTemplate, nav_sections,
};
use super::git::RepoView;
use super::render;
use crate::Quire;

pub async fn tree_view(
    State(quire): State<Quire>,
    auth: Auth,
    AxumPath(repo): AxumPath<String>,
) -> Response {
    tree_or_file_at_path(quire, repo, String::new(), auth.is_authenticated()).await
}

pub async fn tree_view_path(
    State(quire): State<Quire>,
    auth: Auth,
    AxumPath((repo, path)): AxumPath<(String, String)>,
) -> Response {
    tree_or_file_at_path(quire, repo, path, auth.is_authenticated()).await
}

async fn tree_or_file_at_path(quire: Quire, repo: String, path: String, authed: bool) -> Response {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = db::resolve_repo_name(&repo);
    let git_repo = match quire.repo(&repo_name) {
        Ok(r) if r.exists() => r,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let path_clone = path.clone();
    let result = tokio::task::spawn_blocking(move || {
        let reader = RepoView::new(&git_repo);

        // Try ls-tree first — if it succeeds, this is a directory.
        if let Some(tree_data) = read_tree_data(&reader, &path_clone) {
            let bookmarks = reader.bookmarks();
            let tags = reader.tags();
            let recent_changes = reader.recent_changes_for(Some(&path_clone));
            Some(Ok((tree_data, bookmarks, tags, recent_changes)))
        } else {
            // ls-tree failed — try reading as a file blob.
            read_file_data(&reader, &path_clone).map(Err)
        }
    })
    .await
    .unwrap_or(None);

    match result {
        Some(Ok((tree_data, bookmarks, tags, recent_changes))) => {
            let crumbs = build_tree_crumbs(&repo_display, &path);
            let tmpl = TreeTemplate {
                sections: nav_sections(&repo_display, "tree", authed),
                repo: repo_display,
                crumbs,
                bookmarks,
                tags,
                path,
                bookmark: tree_data.bookmark,
                sha_short: tree_data.sha_short,
                entries: tree_data.entries,
                recent_changes,
            };
            render(&tmpl)
        }
        Some(Err(file_data)) => {
            let crumbs = build_file_crumbs(&repo_display, &path);
            let line_nums: Vec<usize> = (1..=file_data.line_count).collect();
            let tmpl = FileViewTemplate {
                sections: nav_sections(&repo_display, "tree", authed),
                repo: repo_display.clone(),
                crumbs,
                path,
                bookmark: file_data.bookmark,
                sha_short: file_data.sha_short.clone(),
                sha_head: file_data.sha_short[..file_data.sha_short.len().min(4)].to_string(),
                sha_tail: file_data.sha_short[file_data.sha_short.len().min(4)..].to_string(),
                last_change_sha: file_data.last_change_sha,
                last_change_head: file_data.last_change_head,
                last_change_tail: file_data.last_change_tail,
                last_change_msg: file_data.last_change_msg,
                last_change_author: file_data.last_change_author,
                last_change_age: file_data.last_change_age,
                line_count: file_data.line_count,
                file_size: file_data.file_size,
                language: file_data.language,
                mode: file_data.mode,
                encoding: file_data.encoding,
                line_ending: file_data.line_ending,
                line_nums,
                lines: file_data.lines,
            };
            render(&tmpl)
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ── Tree (directory) view ──────────────────────────────────────

struct TreeData {
    bookmark: String,
    sha_short: String,
    entries: Vec<TreeEntry>,
}

fn build_tree_crumbs(repo: &str, path: &str) -> Vec<Crumb> {
    let mut c = vec![Crumb::with_href("tree", format!("/{repo}/tree"))];
    if !path.is_empty() {
        c.push(Crumb::new(
            path.split('/').next_back().unwrap_or(path).to_string(),
        ));
    }
    c
}

fn read_tree_data(reader: &RepoView<'_>, path: &str) -> Option<TreeData> {
    let bookmark = reader
        .run(&["symbolic-ref", "--short", "HEAD"])
        .unwrap_or_else(|| "main".to_string());

    let sha_short = reader
        .run(&["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|| "unknown".to_string());

    let ls_target = if path.is_empty() {
        "HEAD".to_string()
    } else {
        format!("HEAD:{path}")
    };

    let ls_out = reader.run(&["ls-tree", &ls_target])?;

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

    raw.sort_by(|(ak, an), (bk, bn)| {
        let ao = matches!(ak, TreeEntryKind::Dir | TreeEntryKind::Submodule) as u8;
        let bo = matches!(bk, TreeEntryKind::Dir | TreeEntryKind::Submodule) as u8;
        bo.cmp(&ao).then(an.cmp(bn))
    });

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
            format!("{path}/{name}")
        };
        let commit_info = reader.run(&["log", "-1", "--format=%s|%cr", "HEAD", "--", &entry_path]);
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

    Some(TreeData {
        bookmark,
        sha_short,
        entries,
    })
}

// ── File (blob) view ───────────────────────────────────────────

struct FileData {
    bookmark: String,
    sha_short: String,
    last_change_sha: String,
    last_change_head: String,
    last_change_tail: String,
    last_change_msg: String,
    last_change_author: String,
    last_change_age: String,
    line_count: usize,
    file_size: String,
    language: String,
    mode: String,
    encoding: String,
    line_ending: String,
    lines: Vec<String>,
}

fn build_file_crumbs(repo: &str, path: &str) -> Vec<Crumb> {
    let mut crumbs = vec![Crumb::with_href("tree", format!("/{repo}/tree"))];
    if path.is_empty() {
        return crumbs;
    }
    let segments: Vec<&str> = path.split('/').collect();
    for (i, seg) in segments.iter().enumerate() {
        let href = format!("/{}/tree/{}", repo, segments[..=i].join("/"));
        crumbs.push(Crumb::with_href(*seg, href));
    }
    crumbs
}

fn read_file_data(reader: &RepoView<'_>, path: &str) -> Option<FileData> {
    let bookmark = reader
        .run(&["symbolic-ref", "--short", "HEAD"])
        .unwrap_or_else(|| "main".to_string());

    let sha_short = reader
        .run(&["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|| "unknown".to_string());

    let blob = reader.run(&["show", &format!("HEAD:{path}")])?;

    let mode = reader
        .run(&["ls-tree", "--format=%(objectmode)", "HEAD", path])
        .unwrap_or_else(|| "100644".to_string());

    let log = reader.run(&["log", "-1", "--format=%H|%s|%an|%cr", "HEAD", "--", path])?;
    let mut log_parts = log.splitn(4, '|');
    let last_change_sha = log_parts.next()?.to_string();
    let last_change_head = last_change_sha[..last_change_sha.len().min(4)].to_string();
    let last_change_tail =
        last_change_sha[last_change_sha.len().min(4)..last_change_sha.len().min(8)].to_string();
    let last_change_msg = log_parts.next().unwrap_or("").to_string();
    let last_change_author = log_parts.next().unwrap_or("").to_string();
    let last_change_age = log_parts.next().unwrap_or("").to_string();

    let file_size = reader
        .run(&["cat-file", "-s", &format!("HEAD:{path}")])
        .unwrap_or_default();
    let file_size = format_file_size(file_size.parse().unwrap_or(0));

    let language = detect_language(path);

    let raw = reader.run(&["cat-file", "-p", &format!("HEAD:{path}")])?;
    let encoding = if raw.is_ascii() { "ascii" } else { "utf-8" };
    let line_ending = if raw.contains("\r\n") { "crlf" } else { "lf" };

    let lines: Vec<String> = blob.lines().map(|l| html_escape(l) + "\n").collect();
    let line_count = lines.len();

    Some(FileData {
        bookmark,
        sha_short,
        last_change_sha,
        last_change_head,
        last_change_tail,
        last_change_msg,
        last_change_author,
        last_change_age,
        line_count,
        file_size,
        language,
        mode,
        encoding: encoding.to_string(),
        line_ending: line_ending.to_string(),
        lines,
    })
}

fn format_file_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn detect_language(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => "rust",
        "toml" => "toml",
        "md" => "markdown",
        "js" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "py" => "python",
        "go" => "go",
        "c" => "c",
        "h" => "c",
        "cpp" => "cpp",
        "hpp" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "sh" => "shell",
        "bash" => "shell",
        "zsh" => "shell",
        "fish" => "shell",
        "html" => "html",
        "css" => "css",
        "scss" => "scss",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "xml" => "xml",
        "sql" => "sql",
        "fnl" => "fennel",
        "lua" => "lua",
        "el" => "elisp",
        "dockerfile" => "dockerfile",
        "makefile" => "makefile",
        "lock" => "toml",
        "txt" => "text",
        "gitignore" => "gitignore",
        "gitattributes" => "gitignore",
        "editorconfig" => "ini",
        "justfile" => "just",
        "nix" => "nix",
        _ => "text",
    }
    .to_string()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
