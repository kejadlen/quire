//! Web-layer data access. SQL lives in [`crate::db::runs`]; this module
//! re-exports the row types and wraps the connection-pool boilerplate.

pub use crate::db::runs::{JobRow, RunDetail, RunRow, ShEvent};

use crate::{Quire, Result};

pub fn load_runs(quire: &Quire, repo: &str) -> Result<Vec<RunRow>> {
    let db = quire.db_pool();
    Ok(db.list_runs(repo)?)
}

pub fn load_run_detail(quire: &Quire, repo: &str, run_id: &str) -> Result<RunDetail> {
    let db = quire.db_pool();
    Ok(db.get_run_detail(repo, run_id)?)
}

/// Resolve a URL slug to the on-disk repo name.
///
/// URLs use clean names (`foo`), disk/DB use `foo.git`.
pub fn resolve_repo_name(slug: &str) -> String {
    if slug.ends_with(".git") {
        slug.to_string()
    } else {
        format!("{slug}.git")
    }
}

/// True if the given run_id parses as a UUID.
///
/// CI runs are assigned UUIDv7 ids at creation time. Anything else
/// reaching the web layer is either a typo or a probe.
pub fn is_valid_run_id(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok()
}

/// True if the given string is safe to use as a single filesystem path
/// component.
///
/// Rejects empty strings, path separators, NUL, and the `.`/`..` traversal
/// segments. Used to gate DB-sourced job ids before they touch `Path::join`.
pub fn is_safe_path_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains('\0')
        && s != "."
        && s != ".."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_repo_name_appends_git() {
        assert_eq!(resolve_repo_name("foo"), "foo.git");
    }

    #[test]
    fn resolve_repo_name_preserves_git_suffix() {
        assert_eq!(resolve_repo_name("foo.git"), "foo.git");
    }

    #[test]
    fn resolve_repo_name_handles_grouped_repo() {
        assert_eq!(resolve_repo_name("work/proj"), "work/proj.git");
    }

    #[test]
    fn run_id_accepts_uuid() {
        assert!(is_valid_run_id("0194f3a5-2b3c-7000-8000-000000000000"));
    }

    #[test]
    fn run_id_rejects_traversal() {
        assert!(!is_valid_run_id("../etc/passwd"));
        assert!(!is_valid_run_id(""));
        assert!(!is_valid_run_id("foo"));
    }

    #[test]
    fn safe_path_segment_accepts_normal_names() {
        assert!(is_safe_path_segment("build"));
        assert!(is_safe_path_segment("test-job"));
        assert!(is_safe_path_segment("job_1"));
    }

    #[test]
    fn safe_path_segment_rejects_traversal_and_separators() {
        assert!(!is_safe_path_segment(""));
        assert!(!is_safe_path_segment("."));
        assert!(!is_safe_path_segment(".."));
        assert!(!is_safe_path_segment("a/b"));
        assert!(!is_safe_path_segment("a\\b"));
        assert!(!is_safe_path_segment("a\0b"));
    }
}
