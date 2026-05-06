//! Read-only CI web view.
//!
//! Two pages:
//! - `GET /repo/<name>/ci` — most-recent runs for a repo.
//! - `GET /repo/<name>/ci/<run-id>` — per-run detail with jobs and logs.
//!
//! Server-rendered HTML. JavaScript-optional. Follows docs/STYLE_GUIDE.md.

use axum::extract::{Path as AxumPath, State};
use axum::http::HeaderMap;
use axum::response::Html;
use rusqlite::Connection;

use crate::Quire;

/// Extract the Remote-User header set by the reverse proxy.
fn remote_user(headers: &HeaderMap) -> String {
    headers
        .get("Remote-User")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

// ── Run list page ──────────────────────────────────────────────────

struct RunRow {
    id: String,
    state: String,
    sha: String,
    ref_name: String,
    queued_at_ms: i64,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
}

pub async fn run_list(
    State(quire): State<Quire>,
    AxumPath(repo): AxumPath<String>,
    headers: HeaderMap,
) -> Html<String> {
    let _user = remote_user(&headers);
    let repo_display = repo.trim_end_matches(".git");

    let runs = match load_runs(&quire, &repo) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(repo = %repo, error = %e, "failed to load runs");
            return Html(error_page("Failed to load runs", &e, repo_display));
        }
    };

    Html(run_list_html(repo_display, &runs))
}

fn load_runs(quire: &Quire, repo: &str) -> Result<Vec<RunRow>, String> {
    let db = Connection::open(quire.db_path()).map_err(|e| e.to_string())?;
    let mut stmt = db
        .prepare(
            "SELECT id, state, sha, ref_name, queued_at_ms, started_at_ms, finished_at_ms
             FROM runs WHERE repo = ?1
             ORDER BY queued_at_ms DESC
             LIMIT 50",
        )
        .map_err(|e| e.to_string())?;

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
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    Ok(rows)
}

// ── Run detail page ────────────────────────────────────────────────

struct JobRow {
    job_id: String,
    state: String,
    exit_code: Option<i32>,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
}

struct ShEvent {
    job_id: String,
    started_at_ms: i64,
    finished_at_ms: i64,
    exit_code: i32,
    cmd: String,
}

pub async fn run_detail(
    State(quire): State<Quire>,
    AxumPath((repo, run_id)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Html<String> {
    let _user = remote_user(&headers);
    let repo_display = repo.trim_end_matches(".git");

    let result = load_run_detail(&quire, &repo, &run_id);
    let (run, jobs, sh_events) = match result {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(repo = %repo, run_id = %run_id, error = %e, "failed to load run detail");
            return Html(error_page("Failed to load run", &e, repo_display));
        }
    };

    // Load CRI log contents for each sh event.
    let runs_base = quire.base_dir().join("runs").join(&repo);
    let mut log_contents: std::collections::HashMap<(String, usize), String> =
        std::collections::HashMap::new();
    for (idx, ev) in sh_events.iter().enumerate() {
        // Only load for events matching the current job.
        let sh_n = sh_index_for_event(&sh_events, &ev.job_id, idx);
        let key = (ev.job_id.clone(), sh_n);
        if log_contents.contains_key(&key) {
            continue;
        }
        let log_path = runs_base
            .join(&run_id)
            .join("jobs")
            .join(&ev.job_id)
            .join(format!("sh-{sh_n}.log"));
        if log_path.exists() {
            match fs_err::read_to_string(&log_path) {
                Ok(content) => {
                    log_contents.insert(key, content);
                }
                Err(e) => {
                    tracing::warn!(path = %log_path.display(), error = %e, "failed to read CRI log");
                }
            }
        }
    }

    Html(run_detail_html(
        repo_display,
        &run,
        &jobs,
        &sh_events,
        &log_contents,
    ))
}

/// Determine the 1-based sh index for an event within its job.
fn sh_index_for_event(events: &[ShEvent], job_id: &str, event_idx: usize) -> usize {
    let mut n = 0;
    for (i, ev) in events.iter().enumerate() {
        if ev.job_id == job_id && i <= event_idx {
            n += 1;
        }
    }
    n
}

fn load_run_detail(
    quire: &Quire,
    repo: &str,
    run_id: &str,
) -> Result<(RunRow, Vec<JobRow>, Vec<ShEvent>), String> {
    let db = Connection::open(quire.db_path()).map_err(|e| e.to_string())?;

    let run = db
        .query_row(
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
        )
        .map_err(|e| e.to_string())?;

    let mut job_stmt = db
        .prepare(
            "SELECT job_id, state, exit_code, started_at_ms, finished_at_ms
             FROM jobs WHERE run_id = ?1
             ORDER BY rowid",
        )
        .map_err(|e| e.to_string())?;

    let jobs = job_stmt
        .query_map(rusqlite::params![run_id], |row| {
            Ok(JobRow {
                job_id: row.get(0)?,
                state: row.get(1)?,
                exit_code: row.get(2)?,
                started_at_ms: row.get(3)?,
                finished_at_ms: row.get(4)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    let mut sh_stmt = db
        .prepare(
            "SELECT job_id, started_at_ms, finished_at_ms, exit_code, cmd
             FROM sh_events WHERE run_id = ?1
             ORDER BY job_id, started_at_ms",
        )
        .map_err(|e| e.to_string())?;

    let sh_events = sh_stmt
        .query_map(rusqlite::params![run_id], |row| {
            Ok(ShEvent {
                job_id: row.get(0)?,
                started_at_ms: row.get(1)?,
                finished_at_ms: row.get(2)?,
                exit_code: row.get(3)?,
                cmd: row.get(4)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    Ok((run, jobs, sh_events))
}

// ── HTML rendering ─────────────────────────────────────────────────

fn run_list_html(repo: &str, runs: &[RunRow]) -> String {
    let rows_html = if runs.is_empty() {
        r#"<tr><td colspan="5" style="padding:16px;color:var(--muted)">no runs yet</td></tr>"#
            .to_string()
    } else {
        runs.iter()
            .map(|r| {
                let state_color = match r.state.as_str() {
                    "complete" => "var(--ok)",
                    "failed" => "var(--bad)",
                    _ => "var(--muted)",
                };
                let sha_short = &r.sha[..r.sha.len().min(8)];
                let ref_short = r.ref_name.trim_start_matches("refs/heads/");
                let queued = format_timestamp(r.queued_at_ms);
                let duration = format_duration(r.started_at_ms, r.finished_at_ms);
                format!(
                    r#"<tr>
  <td style="padding:6px 8px"><span style="display:inline-block;width:6px;height:6px;border-radius:3px;background:{state_color}"></span></td>
  <td style="padding:6px 8px"><a href="/repo/{repo}/ci/{id}" style="color:var(--accent);text-decoration:none;border-bottom:1px dotted var(--rule2)">{sha_short}</a></td>
  <td style="padding:6px 8px">{ref_short}</td>
  <td style="padding:6px 8px">{queued}</td>
  <td style="padding:6px 8px">{duration}</td>
</tr>"#,
                    repo = repo,
                    id = r.id,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let style = css();
    let nav = top_nav(repo, "ci");
    let foot = footer();
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ci · {repo}</title>
<style>{style}</style>
</head>
<body>
{nav}
<main style="padding:22px 56px 32px">
<h2 style="font-family:var(--font-mono);font-size:19px;font-weight:600;margin:0 0 16px">ci runs</h2>
<table style="width:100%;border-collapse:collapse;font-family:var(--font-mono);font-size:12.5px;line-height:1.6">
<thead>
<tr style="border-bottom:1px solid var(--rule2)">
  <th style="text-align:left;padding:6px 8px;font-weight:400;color:var(--mutedFaint)"></th>
  <th style="text-align:left;padding:6px 8px;font-weight:400;color:var(--mutedFaint)">sha</th>
  <th style="text-align:left;padding:6px 8px;font-weight:400;color:var(--mutedFaint)">ref</th>
  <th style="text-align:left;padding:6px 8px;font-weight:400;color:var(--mutedFaint)">queued</th>
  <th style="text-align:left;padding:6px 8px;font-weight:400;color:var(--mutedFaint)">duration</th>
</tr>
</thead>
<tbody style="border-top:1px solid var(--rule)">
{rows_html}
</tbody>
</table>
</main>
{foot}
</body>
</html>"#,
    )
}

fn run_detail_html(
    repo: &str,
    run: &RunRow,
    jobs: &[JobRow],
    sh_events: &[ShEvent],
    log_contents: &std::collections::HashMap<(String, usize), String>,
) -> String {
    let state_color = match run.state.as_str() {
        "complete" => "var(--ok)",
        "failed" => "var(--bad)",
        _ => "var(--muted)",
    };
    let sha_short = &run.sha[..run.sha.len().min(8)];
    let ref_short = run.ref_name.trim_start_matches("refs/heads/");
    let queued = format_timestamp(run.queued_at_ms);
    let started = run.started_at_ms.map_or("—".to_string(), format_timestamp);
    let finished = run.finished_at_ms.map_or("—".to_string(), format_timestamp);
    let duration = format_duration(run.started_at_ms, run.finished_at_ms);

    let meta_html = format!(
        r#"<div style="padding:16px 0;border-bottom:1px solid var(--rule)">
<div style="font-family:var(--font-mono);font-size:15px;line-height:1.6">
<span style="color:{state_color}">{state}</span> · <span style="color:var(--accent)">{sha_short}</span> · {ref_short}
</div>
<div style="font-family:var(--font-mono);font-size:12px;color:var(--mutedFaint);margin-top:4px">
queued {queued} · started {started} · finished {finished} · {duration}
</div>
</div>"#,
        state = run.state,
    );

    // Group sh_events by job and render.
    let mut jobs_html = String::new();
    for job in jobs {
        let job_state_color = match job.state.as_str() {
            "complete" => "var(--ok)",
            "failed" => "var(--bad)",
            _ => "var(--muted)",
        };
        let job_duration = format_duration(job.started_at_ms, job.finished_at_ms);
        let exit_str = job
            .exit_code
            .map(|c| format!(" · exit {c}"))
            .unwrap_or_default();

        jobs_html.push_str(&format!(
            r#"<div style="margin:24px 0 0">
<div style="font-family:var(--font-mono);font-size:13px;font-weight:500;padding:8px 0;border-bottom:1px solid var(--rule2)">
<span style="color:{job_state_color}">{job_state}</span> · {job_id} · {job_duration}{exit_str}
</div>"#,
            job_state = job.state,
            job_id = job.job_id,
        ));

        let job_shs: Vec<(usize, &ShEvent)> = sh_events
            .iter()
            .enumerate()
            .filter(|(_, e)| e.job_id == job.job_id)
            .collect();

        for (global_idx, ev) in &job_shs {
            let sh_n = sh_index_for_event(sh_events, &ev.job_id, *global_idx);
            let ev_duration = format_duration(Some(ev.started_at_ms), Some(ev.finished_at_ms));
            let cmd_display = if ev.cmd.len() > 120 {
                &ev.cmd[..120]
            } else {
                &ev.cmd
            };

            jobs_html.push_str(&format!(
                r#"<div style="margin:8px 0">
<div style="font-family:var(--font-mono);font-size:11px;color:var(--mutedFaint);margin-bottom:2px">
sh-{sh_n} · {ev_duration} · exit {exit_code}
</div>
<div style="font-family:var(--font-mono);font-size:12px;color:var(--muted);margin-bottom:4px">
{cmd_display}
</div>"#,
                exit_code = ev.exit_code,
            ));

            if let Some(content) = log_contents.get(&(ev.job_id.clone(), sh_n)) {
                let escaped = html_escape(content);
                jobs_html.push_str(&format!(
                    r#"<pre style="font-family:var(--font-mono);font-size:12px;line-height:1.65;background:var(--code);color:var(--ink);padding:10px 14px;border-left:2px solid var(--accent);overflow:auto;margin:0 0 8px">{escaped}</pre>"#
                ));
            }

            jobs_html.push_str("</div>");
        }

        jobs_html.push_str("</div>");
    }

    if jobs.is_empty() {
        jobs_html =
            r#"<div style="padding:16px 0;color:var(--muted)">no jobs recorded</div>"#.to_string();
    }

    let style = css();
    let nav = top_nav(repo, &format!("ci · {sha_short}"));
    let foot = footer();
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ci · {repo} · {sha_short}</title>
<style>{style}</style>
</head>
<body>
{nav}
<main style="padding:22px 56px 32px">
{meta_html}
{jobs_html}
</main>
{foot}
</body>
</html>"#,
    )
}

fn error_page(title: &str, detail: &str, repo: &str) -> String {
    let escaped = html_escape(detail);
    let style = css();
    let nav = top_nav(repo, "error");
    let foot = footer();
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>{title}</title>
<style>{style}</style>
</head>
<body>
{nav}
<main style="padding:22px 56px 32px">
<p style="color:var(--bad);font-family:var(--font-mono)">{title}</p>
<pre style="font-family:var(--font-mono);font-size:12px;background:var(--code);padding:14px 18px;overflow:auto">{escaped}</pre>
</main>
{foot}
</body>
</html>"#,
    )
}

// ── Shared HTML components ─────────────────────────────────────────

fn top_nav(repo: &str, page: &str) -> String {
    format!(
        r#"<nav style="padding:14px 56px;border-bottom:1px solid var(--rule);font-family:var(--font-mono);font-size:14px;font-weight:500;letter-spacing:-0.2px;display:flex;justify-content:space-between;align-items:center">
<div><span style="color:var(--mutedFaint)">quire</span> <span style="color:var(--rule2)">/</span> <span>{repo}</span> <span style="color:var(--rule2)">/</span> <span style="color:var(--muted)">{page}</span></div>
<div style="font-size:11px;color:var(--mutedFaint)">press [?] for shortcuts</div>
</nav>"#
    )
}

fn footer() -> String {
    r#"<footer style="padding:16px 56px 24px;border-top:1px solid var(--rule);font-family:var(--font-mono);font-size:11px;color:var(--mutedFaint);letter-spacing:0.2px;display:flex;justify-content:space-between">
<span>quire</span>
<span>?</span>
</footer>"#
        .to_string()
}

fn css() -> &'static str {
    // Paper palette, light variant.
    r#":root {
  --font-humanist: "iA Writer Quattro", "iA Writer Quattro V", -apple-system, system-ui, sans-serif;
  --font-mono: "IBM Plex Mono", ui-monospace, monospace;
  --bg: #f8f4ea;
  --ink: #1d1a15;
  --muted: #6b6257;
  --mutedFaint: #9a9184;
  --rule: #ddd4c1;
  --rule2: #c7bfae;
  --code: #efe8d6;
  --accent: #3a3a3a;
  --ok: #4a7a3a;
  --bad: #9a3a28;
}
body { margin:0; background:var(--bg); color:var(--ink); font-family:var(--font-humanist); font-size:15px; line-height:1.6; }
a { color:var(--accent); }
pre { white-space:pre-wrap; word-break:break-word; }"#
}

// ── Helpers ────────────────────────────────────────────────────────

fn format_timestamp(ms: i64) -> String {
    use jiff::Timestamp;
    match Timestamp::from_millisecond(ms) {
        Ok(ts) => {
            let now = Timestamp::now();
            let span = now.since(ts).unwrap_or_else(|_| jiff::Span::new());
            let hours = span.get_hours().abs();
            let minutes = span.get_minutes().abs();
            if hours < 1 {
                if minutes < 1 {
                    "just now".to_string()
                } else {
                    format!("{minutes}m ago")
                }
            } else if hours < 24 {
                format!("{hours}h ago")
            } else {
                ts.to_string()
            }
        }
        Err(_) => format!("{ms}ms"),
    }
}

fn format_duration(start: Option<i64>, end: Option<i64>) -> String {
    match (start, end) {
        (Some(s), Some(e)) => {
            let ms = e - s;
            if ms < 1000 {
                format!("{ms}ms")
            } else {
                format!("{}s", ms / 1000)
            }
        }
        _ => "—".to_string(),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ── Router ─────────────────────────────────────────────────────────

pub fn router(quire: Quire) -> axum::Router {
    axum::Router::new()
        .route("/repo/{repo}/ci", axum::routing::get(run_list))
        .route("/repo/{repo}/ci/{run_id}", axum::routing::get(run_detail))
        .with_state(quire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_list_html_renders_empty() {
        let html = run_list_html("test.git", &[]);
        assert!(html.contains("no runs yet"));
        assert!(html.contains("ci · test.git"));
    }

    #[test]
    fn run_list_html_renders_runs() {
        let runs = vec![RunRow {
            id: "abc123".to_string(),
            state: "complete".to_string(),
            sha: "deadbeef".to_string(),
            ref_name: "refs/heads/main".to_string(),
            queued_at_ms: 1000,
            started_at_ms: Some(2000),
            finished_at_ms: Some(3000),
        }];
        let html = run_list_html("test.git", &runs);
        assert!(html.contains("deadbeef"));
        assert!(html.contains("main"));
        assert!(html.contains("/repo/test.git/ci/abc123"));
    }

    #[test]
    fn html_escape_escapes_special_chars() {
        assert_eq!(html_escape("<script>"), "&lt;script&gt;");
        assert_eq!(html_escape("a&b"), "a&amp;b");
    }

    #[test]
    fn format_duration_shows_ms_for_subsecond() {
        assert_eq!(format_duration(Some(0), Some(500)), "500ms");
    }

    #[test]
    fn format_duration_shows_seconds() {
        assert_eq!(format_duration(Some(0), Some(3500)), "3s");
    }

    #[test]
    fn format_duration_dash_when_missing() {
        assert_eq!(format_duration(None, None), "—");
    }
}
