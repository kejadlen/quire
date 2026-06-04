//! Handler for the file (blob) view.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::super::db;
use super::super::templates::{FileViewTemplate, nav_sections};
use super::git::RepoView;
use super::render;
use crate::Quire;

pub async fn file_view(
    State(quire): State<Quire>,
    AxumPath((repo, path)): AxumPath<(String, String)>,
) -> Response {
    let repo_display = repo.trim_end_matches(".git").to_string();
    let repo_name = db::resolve_repo_name(&repo);
    let git_repo = match quire.repo(&repo_name) {
        Ok(r) if r.exists() => r,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let path_clone = path.clone();
    let result = tokio::task::spawn_blocking(move || {
        let reader = RepoView::new(&git_repo);
        read_file_data(&reader, &path_clone)
    })
    .await
    .unwrap_or(None);

    let file_data = match result {
        Some(v) => v,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let tmpl = FileViewTemplate {
        sections: nav_sections(&repo_display, "tree", false),
        repo: repo_display,
        crumbs: vec![],
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
        line_nums: (1..=file_data.line_count).collect(),
        lines: file_data.lines,
    };
    render(&tmpl)
}

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

fn read_file_data(reader: &RepoView<'_>, path: &str) -> Option<FileData> {
    let bookmark = reader
        .run(&["symbolic-ref", "--short", "HEAD"])
        .unwrap_or_else(|| "main".to_string());

    let sha_short = reader
        .run(&["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|| "unknown".to_string());

    // Read the blob content.
    let blob = reader.run(&["show", &format!("HEAD:{path}")])?;

    // File mode.
    let mode = reader
        .run(&["ls-tree", "--format=%(objectmode)", "HEAD", path])
        .unwrap_or_else(|| "100644".to_string());

    // Last commit that touched this file.
    let log = reader.run(&["log", "-1", "--format=%H|%s|%an|%cr", "HEAD", "--", path])?;
    let mut log_parts = log.splitn(4, '|');
    let last_change_sha = log_parts.next()?.to_string();
    let last_change_head = last_change_sha[..last_change_sha.len().min(4)].to_string();
    let last_change_tail =
        last_change_sha[last_change_sha.len().min(4)..last_change_sha.len().min(8)].to_string();
    let last_change_msg = log_parts.next().unwrap_or("").to_string();
    let last_change_author = log_parts.next().unwrap_or("").to_string();
    let last_change_age = log_parts.next().unwrap_or("").to_string();

    // File size.
    let file_size = reader
        .run(&["cat-file", "-s", &format!("HEAD:{path}")])
        .unwrap_or_default();
    let file_size = format_file_size(file_size.parse().unwrap_or(0));

    // Detect language from extension.
    let language = detect_language(path);

    // Detect encoding and line ending from raw blob.
    let raw = reader.run(&["cat-file", "-p", &format!("HEAD:{path}")])?;
    let encoding = if raw.is_ascii() { "ascii" } else { "utf-8" };
    let line_ending = if raw.contains("\r\n") { "crlf" } else { "lf" };

    // Split into lines, HTML-escape each line.
    let lines: Vec<String> = blob.lines().map(html_escape).collect();
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
