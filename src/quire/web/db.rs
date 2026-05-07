//! Data access structs and DB loading functions for the web view.

use rusqlite::Connection;

use crate::{Quire, Result};

/// Raw run row from the database.
pub struct RunRow {
    pub id: String,
    pub state: String,
    pub sha: String,
    pub ref_name: String,
    pub queued_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
}

/// Raw job row from the database.
pub struct JobRow {
    pub job_id: String,
    pub state: String,
    pub exit_code: Option<i32>,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
}

/// Raw sh event row from the database.
pub struct ShEvent {
    pub job_id: String,
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub exit_code: i32,
    pub cmd: String,
}

pub fn load_runs(quire: &Quire, repo: &str) -> Result<Vec<RunRow>> {
    let db = Connection::open(quire.db_path())?;
    let mut stmt = db.prepare(
        "SELECT id, state, sha, ref_name, queued_at_ms, started_at_ms, finished_at_ms
         FROM runs WHERE repo = ?1
         ORDER BY queued_at_ms DESC
         LIMIT 50",
    )?;

    let rows = stmt
        .query_map(rusqlite::params![repo], |row| {
            Ok(RunRow {
                id: row.get(0)?,
                state: row.get(1)?,
                sha: row.get(2)?,
                ref_name: row.get(3)?,
                queued_at_ms: row.get(4)?,
                started_at_ms: row.get(5)?,
                finished_at_ms: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(rows)
}

pub fn load_run_detail(
    quire: &Quire,
    repo: &str,
    run_id: &str,
) -> Result<(RunRow, Vec<JobRow>, Vec<ShEvent>)> {
    let db = Connection::open(quire.db_path())?;

    let run = db.query_row(
        "SELECT id, state, sha, ref_name, queued_at_ms, started_at_ms, finished_at_ms
         FROM runs WHERE id = ?1 AND repo = ?2",
        rusqlite::params![run_id, repo],
        |row| {
            Ok(RunRow {
                id: row.get(0)?,
                state: row.get(1)?,
                sha: row.get(2)?,
                ref_name: row.get(3)?,
                queued_at_ms: row.get(4)?,
                started_at_ms: row.get(5)?,
                finished_at_ms: row.get(6)?,
            })
        },
    )?;

    let mut job_stmt = db.prepare(
        "SELECT job_id, state, exit_code, started_at_ms, finished_at_ms
         FROM jobs WHERE run_id = ?1
         ORDER BY rowid",
    )?;

    let jobs = job_stmt
        .query_map(rusqlite::params![run_id], |row| {
            Ok(JobRow {
                job_id: row.get(0)?,
                state: row.get(1)?,
                exit_code: row.get(2)?,
                started_at_ms: row.get(3)?,
                finished_at_ms: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut sh_stmt = db.prepare(
        "SELECT job_id, started_at_ms, finished_at_ms, exit_code, cmd
         FROM sh_events WHERE run_id = ?1
         ORDER BY job_id, started_at_ms",
    )?;

    let sh_events = sh_stmt
        .query_map(rusqlite::params![run_id], |row| {
            Ok(ShEvent {
                job_id: row.get(0)?,
                started_at_ms: row.get(1)?,
                finished_at_ms: row.get(2)?,
                exit_code: row.get(3)?,
                cmd: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok((run, jobs, sh_events))
}

/// Determine the 1-based sh index for an event within its job.
pub fn sh_index_for_event(events: &[ShEvent], job_id: &str, event_idx: usize) -> usize {
    let mut n = 0;
    for (i, ev) in events.iter().enumerate() {
        if ev.job_id == job_id && i <= event_idx {
            n += 1;
        }
    }
    n
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
