//! Data access structs and DB loading functions for the web view.

use std::collections::HashMap;

use quire_core::ci::event::{Event, EventKind, JobOutcome};

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
    let db = quire
        .db_pool()
        .lock()
        .map_err(|_| crate::error::Error::Io(std::io::Error::other("db mutex poisoned")))?;
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

/// Aggregated run detail from the database.
pub struct RunDetail {
    pub run: RunRow,
    pub jobs: Vec<JobRow>,
    pub sh_events: Vec<ShEvent>,
}

pub fn load_run_detail(quire: &Quire, repo: &str, run_id: &str) -> Result<RunDetail> {
    let db = quire
        .db_pool()
        .lock()
        .map_err(|_| crate::error::Error::Io(std::io::Error::other("db mutex poisoned")))?;

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

    let event_jsons: Vec<String> = db
        .prepare("SELECT event FROM events WHERE run_id = ?1 ORDER BY seq")?
        .query_map(rusqlite::params![run_id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let events: Vec<Event> = event_jsons
        .iter()
        .map(|s| serde_json::from_str(s))
        .collect::<std::result::Result<_, _>>()?;

    // Reconstruct job rows by pairing JobStarted with JobFinished.
    let mut pending_jobs: HashMap<String, i64> = HashMap::new();
    let mut jobs: Vec<JobRow> = Vec::new();
    for event in &events {
        match &event.kind {
            EventKind::JobStarted { job_id } => {
                pending_jobs.insert(job_id.clone(), event.at_ms);
            }
            EventKind::JobFinished { job_id, outcome } => {
                let started_at = pending_jobs.remove(job_id.as_str());
                jobs.push(JobRow {
                    job_id: job_id.clone(),
                    state: match outcome {
                        JobOutcome::Complete => "complete",
                        JobOutcome::Failed => "failed",
                    }
                    .to_string(),
                    exit_code: None,
                    started_at_ms: started_at,
                    finished_at_ms: Some(event.at_ms),
                });
            }
            _ => {}
        }
    }
    // Match old SQL order: non-null started_at first, then by started_at, then job_id.
    jobs.sort_by(|a, b| {
        let a_null = a.started_at_ms.is_none() as u8;
        let b_null = b.started_at_ms.is_none() as u8;
        a_null
            .cmp(&b_null)
            .then_with(|| a.started_at_ms.cmp(&b.started_at_ms))
            .then(a.job_id.cmp(&b.job_id))
    });

    // Reconstruct sh events by pairing ShStarted with ShFinished.
    let mut pending_sh: HashMap<String, (i64, String)> = HashMap::new();
    let mut sh_events: Vec<ShEvent> = Vec::new();
    for event in &events {
        match &event.kind {
            EventKind::ShStarted { job_id, cmd } => {
                pending_sh.insert(job_id.clone(), (event.at_ms, cmd.clone()));
            }
            EventKind::ShFinished { job_id, exit_code } => {
                let Some((started_at, cmd)) = pending_sh.remove(job_id.as_str()) else {
                    continue;
                };
                sh_events.push(ShEvent {
                    job_id: job_id.clone(),
                    started_at_ms: started_at,
                    finished_at_ms: event.at_ms,
                    exit_code: *exit_code,
                    cmd,
                });
            }
            _ => {}
        }
    }
    // Match old SQL order: by job_id then started_at_ms.
    sh_events.sort_by(|a, b| {
        a.job_id
            .cmp(&b.job_id)
            .then(a.started_at_ms.cmp(&b.started_at_ms))
    });

    Ok(RunDetail {
        run,
        jobs,
        sh_events,
    })
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
