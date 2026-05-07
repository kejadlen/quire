//! Dev utilities: seed data for local development.

use miette::{Context, IntoDiagnostic, Result};
use rusqlite::params;

use quire::Quire;

/// Base timestamp for seed data: 2026-05-06T12:00:00Z.
const BASE_MS: i64 = 1746532800000;

/// Seed a tempdir with realistic CI run data and return a `Quire` pointing at it.
///
/// Creates a fresh tempdir under `std::env::temp_dir()`, inserts a fixed corpus
/// of runs covering every interesting state (complete, failed, active, pending,
/// superseded) with matching on-disk log artifacts. Idempotent — same input,
/// same output.
pub fn seed() -> Result<Quire> {
    let dir = tempfile::tempdir()
        .into_diagnostic()
        .context("failed to create tempdir")?;

    // Leak the TempDir so it outlives the function. The server will
    // clean up on shutdown, or the OS will when the process exits.
    let base_dir = dir.keep();
    tracing::info!(path = %base_dir.display(), "seeded tempdir");

    let quire = Quire::new(base_dir);
    let db_path = quire.db_path();

    // Open and migrate.
    let mut db = quire::db::open(&db_path)
        .into_diagnostic()
        .context("failed to open database")?;
    quire::db::migrate(&mut db)
        .into_diagnostic()
        .context("failed to run migrations")?;

    insert_runs(&db)?;
    insert_jobs(&db)?;
    insert_sh_events(&db)?;
    write_log_artifacts(&quire)?;

    let run_count: i64 = db
        .query_row("SELECT count(*) FROM runs", [], |row| row.get(0))
        .into_diagnostic()?;

    tracing::info!(%run_count, "seeded database");
    Ok(quire)
}

fn insert_runs(db: &rusqlite::Connection) -> Result<()> {
    let repo = "example.git";
    let workspace = "/tmp/quire-seed";

    let runs = [
        // Complete run — all jobs passed.
        (
            "aaaaaaaa-0000-0000-0000-000000000001",
            "complete",
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            "refs/heads/main",
            BASE_MS,
            Some(BASE_MS + 1000),
            Some(BASE_MS + 5000),
        ),
        // Failed run — one job failed.
        (
            "aaaaaaaa-0000-0000-0000-000000000002",
            "failed",
            "cafebabecafebabecafebabecafebabecafebabe",
            "refs/heads/main",
            BASE_MS - 600_000,
            Some(BASE_MS - 600_000 + 1000),
            Some(BASE_MS - 600_000 + 8000),
        ),
        // Superseded run — pushed then rebased.
        (
            "aaaaaaaa-0000-0000-0000-000000000003",
            "superseded",
            "1111111111111111111111111111111111111111",
            "refs/heads/feature",
            BASE_MS - 1_200_000,
            Some(BASE_MS - 1_200_000 + 1000),
            Some(BASE_MS - 1_200_000 + 2000),
        ),
        // Active run — still running.
        (
            "aaaaaaaa-0000-0000-0000-000000000004",
            "active",
            "2222222222222222222222222222222222222222",
            "refs/heads/main",
            BASE_MS - 5000,
            Some(BASE_MS - 4000),
            None,
        ),
        // Pending run — queued but not started.
        (
            "aaaaaaaa-0000-0000-0000-000000000005",
            "pending",
            "3333333333333333333333333333333333333333",
            "refs/heads/main",
            BASE_MS - 1000,
            None,
            None,
        ),
        // Complete run on a different branch with multiple jobs.
        (
            "aaaaaaaa-0000-0000-0000-000000000006",
            "complete",
            "4444444444444444444444444444444444444444",
            "refs/heads/v2",
            BASE_MS - 3_600_000,
            Some(BASE_MS - 3_600_000 + 2000),
            Some(BASE_MS - 3_600_000 + 12000),
        ),
        // Failed run — orphaned (container died).
        (
            "aaaaaaaa-0000-0000-0000-000000000007",
            "failed",
            "5555555555555555555555555555555555555555",
            "refs/heads/main",
            BASE_MS - 7_200_000,
            Some(BASE_MS - 7_200_000 + 1000),
            Some(BASE_MS - 7_200_000 + 60000),
        ),
    ];

    let mut stmt = db.prepare(
        "INSERT INTO runs (id, repo, ref_name, sha, pushed_at_ms, state, failure_kind,
                           queued_at_ms, started_at_ms, finished_at_ms,
                           container_id, image_tag, build_started_at_ms, build_finished_at_ms,
                           container_started_at_ms, container_stopped_at_ms, workspace_path)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?9, NULL, NULL, NULL, NULL, NULL, NULL, ?10)"
    ).into_diagnostic()?;

    for (id, state, sha, ref_name, pushed_at_ms, started_at_ms, finished_at_ms) in &runs {
        stmt.execute(params![
            id,
            repo,
            ref_name,
            sha,
            pushed_at_ms,
            state,
            pushed_at_ms, // queued_at_ms = pushed_at_ms
            started_at_ms,
            finished_at_ms,
            workspace,
        ])
        .into_diagnostic()?;
    }

    Ok(())
}

fn insert_jobs(db: &rusqlite::Connection) -> Result<()> {
    let jobs = [
        // Run 1 (complete): two passing jobs.
        (
            "aaaaaaaa-0000-0000-0000-000000000001",
            "build",
            "complete",
            Some(0),
            BASE_MS + 1000,
            Some(BASE_MS + 3000),
        ),
        (
            "aaaaaaaa-0000-0000-0000-000000000001",
            "test",
            "complete",
            Some(0),
            BASE_MS + 3000,
            Some(BASE_MS + 5000),
        ),
        // Run 2 (failed): one pass, one fail.
        (
            "aaaaaaaa-0000-0000-0000-000000000002",
            "build",
            "complete",
            Some(0),
            BASE_MS - 600_000 + 1000,
            Some(BASE_MS - 600_000 + 3000),
        ),
        (
            "aaaaaaaa-0000-0000-0000-000000000002",
            "test",
            "failed",
            Some(1),
            BASE_MS - 600_000 + 3000,
            Some(BASE_MS - 600_000 + 8000),
        ),
        // Run 3 (superseded): one job started then cancelled.
        (
            "aaaaaaaa-0000-0000-0000-000000000003",
            "build",
            "complete",
            Some(0),
            BASE_MS - 1_200_000 + 1000,
            Some(BASE_MS - 1_200_000 + 2000),
        ),
        // Run 4 (active): build running.
        (
            "aaaaaaaa-0000-0000-0000-000000000004",
            "build",
            "active",
            None,
            BASE_MS - 4000,
            None,
        ),
        // Run 5 (pending): nothing started.
        // (no jobs yet)
        // Run 6 (complete, multi-job): lint + build + test.
        (
            "aaaaaaaa-0000-0000-0000-000000000006",
            "lint",
            "complete",
            Some(0),
            BASE_MS - 3_600_000 + 2000,
            Some(BASE_MS - 3_600_000 + 4000),
        ),
        (
            "aaaaaaaa-0000-0000-0000-000000000006",
            "build",
            "complete",
            Some(0),
            BASE_MS - 3_600_000 + 4000,
            Some(BASE_MS - 3_600_000 + 8000),
        ),
        (
            "aaaaaaaa-0000-0000-0000-000000000006",
            "test",
            "complete",
            Some(0),
            BASE_MS - 3_600_000 + 8000,
            Some(BASE_MS - 3_600_000 + 12000),
        ),
        // Run 7 (failed, orphaned): build passed, test was running when container died.
        (
            "aaaaaaaa-0000-0000-0000-000000000007",
            "build",
            "complete",
            Some(0),
            BASE_MS - 7_200_000 + 1000,
            Some(BASE_MS - 7_200_000 + 4000),
        ),
        (
            "aaaaaaaa-0000-0000-0000-000000000007",
            "test",
            "failed",
            Some(137),
            BASE_MS - 7_200_000 + 4000,
            Some(BASE_MS - 7_200_000 + 60000),
        ),
    ];

    let mut stmt = db
        .prepare(
            "INSERT INTO jobs (run_id, job_id, state, exit_code, started_at_ms, finished_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .into_diagnostic()?;

    for (run_id, job_id, state, exit_code, started_at_ms, finished_at_ms) in &jobs {
        stmt.execute(params![
            run_id,
            job_id,
            state,
            exit_code,
            started_at_ms,
            finished_at_ms
        ])
        .into_diagnostic()?;
    }

    Ok(())
}

fn insert_sh_events(db: &rusqlite::Connection) -> Result<()> {
    let events = [
        // Run 1, build job.
        (
            "aaaaaaaa-0000-0000-0000-000000000001",
            "build",
            BASE_MS + 1000,
            BASE_MS + 2500,
            0,
            "cargo build --release",
        ),
        (
            "aaaaaaaa-0000-0000-0000-000000000001",
            "build",
            BASE_MS + 2500,
            BASE_MS + 3000,
            0,
            "cargo clippy -- -D warnings",
        ),
        // Run 1, test job.
        (
            "aaaaaaaa-0000-0000-0000-000000000001",
            "test",
            BASE_MS + 3000,
            BASE_MS + 4800,
            0,
            "cargo test --workspace",
        ),
        // Run 2, build job.
        (
            "aaaaaaaa-0000-0000-0000-000000000002",
            "build",
            BASE_MS - 600_000 + 1000,
            BASE_MS - 600_000 + 3000,
            0,
            "cargo build --release",
        ),
        // Run 2, test job — fails.
        (
            "aaaaaaaa-0000-0000-0000-000000000002",
            "test",
            BASE_MS - 600_000 + 3000,
            BASE_MS - 600_000 + 5000,
            0,
            "cargo test --workspace",
        ),
        (
            "aaaaaaaa-0000-0000-0000-000000000002",
            "test",
            BASE_MS - 600_000 + 5000,
            BASE_MS - 600_000 + 8000,
            1,
            "cargo test -- --ignored",
        ),
        // Run 3, build job.
        (
            "aaaaaaaa-0000-0000-0000-000000000003",
            "build",
            BASE_MS - 1_200_000 + 1000,
            BASE_MS - 1_200_000 + 2000,
            0,
            "cargo build --release",
        ),
        // Run 4, active build.
        (
            "aaaaaaaa-0000-0000-0000-000000000004",
            "build",
            BASE_MS - 4000,
            BASE_MS - 2000,
            0,
            "cargo build --release",
        ),
        (
            "aaaaaaaa-0000-0000-0000-000000000004",
            "build",
            BASE_MS - 2000,
            BASE_MS - 1000,
            0,
            "cargo clippy -- -D warnings",
        ),
        // Run 6, lint job.
        (
            "aaaaaaaa-0000-0000-0000-000000000006",
            "lint",
            BASE_MS - 3_600_000 + 2000,
            BASE_MS - 3_600_000 + 4000,
            0,
            "cargo fmt --check",
        ),
        // Run 6, build job.
        (
            "aaaaaaaa-0000-0000-0000-000000000006",
            "build",
            BASE_MS - 3_600_000 + 4000,
            BASE_MS - 3_600_000 + 8000,
            0,
            "cargo build --release",
        ),
        // Run 6, test job.
        (
            "aaaaaaaa-0000-0000-0000-000000000006",
            "test",
            BASE_MS - 3_600_000 + 8000,
            BASE_MS - 3_600_000 + 12000,
            0,
            "cargo test --workspace",
        ),
        // Run 7, build job.
        (
            "aaaaaaaa-0000-0000-0000-000000000007",
            "build",
            BASE_MS - 7_200_000 + 1000,
            BASE_MS - 7_200_000 + 4000,
            0,
            "cargo build --release",
        ),
        // Run 7, test job — container died mid-run.
        (
            "aaaaaaaa-0000-0000-0000-000000000007",
            "test",
            BASE_MS - 7_200_000 + 4000,
            BASE_MS - 7_200_000 + 60000,
            137,
            "cargo test --workspace",
        ),
    ];

    let mut stmt = db
        .prepare(
            "INSERT INTO sh_events (run_id, job_id, started_at_ms, finished_at_ms, exit_code, cmd)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .into_diagnostic()?;

    for (run_id, job_id, started_at_ms, finished_at_ms, exit_code, cmd) in &events {
        stmt.execute(params![
            run_id,
            job_id,
            started_at_ms,
            finished_at_ms,
            exit_code,
            cmd
        ])
        .into_diagnostic()?;
    }

    Ok(())
}

fn write_log_artifacts(quire: &Quire) -> Result<()> {
    let b = quire.base_dir().join("runs").join("example.git");

    // Run 1 — complete, clean build output.
    write_sh_log(
        &b,
        run_id(1),
        "build",
        1,
        include_str!("fixtures/run-1-build-1.log"),
    );
    write_sh_log(
        &b,
        run_id(1),
        "build",
        2,
        include_str!("fixtures/run-1-build-2.log"),
    );
    write_sh_log(
        &b,
        run_id(1),
        "test",
        1,
        include_str!("fixtures/run-1-test-1.log"),
    );

    // Run 2 — failed, test output with failures.
    write_sh_log(
        &b,
        run_id(2),
        "build",
        1,
        include_str!("fixtures/run-2-build-1.log"),
    );
    write_sh_log(
        &b,
        run_id(2),
        "test",
        1,
        include_str!("fixtures/run-2-test-1.log"),
    );
    write_sh_log(
        &b,
        run_id(2),
        "test",
        2,
        include_str!("fixtures/run-2-test-2.log"),
    );

    // Run 7 — orphaned, long output that was interrupted.
    write_sh_log(
        &b,
        run_id(7),
        "build",
        1,
        include_str!("fixtures/run-7-build-1.log"),
    );
    write_sh_log(
        &b,
        run_id(7),
        "test",
        1,
        include_str!("fixtures/run-7-test-1.log"),
    );

    Ok(())
}

/// Short-hand for the seed run UUIDs (1-indexed).
fn run_id(n: u8) -> &'static str {
    match n {
        1 => "aaaaaaaa-0000-0000-0000-000000000001",
        2 => "aaaaaaaa-0000-0000-0000-000000000002",
        3 => "aaaaaaaa-0000-0000-0000-000000000003",
        4 => "aaaaaaaa-0000-0000-0000-000000000004",
        5 => "aaaaaaaa-0000-0000-0000-000000000005",
        6 => "aaaaaaaa-0000-0000-0000-000000000006",
        7 => "aaaaaaaa-0000-0000-0000-000000000007",
        _ => panic!("no seed run with index {n}"),
    }
}

fn write_sh_log(
    runs_base: &std::path::Path,
    run_id: &str,
    job_id: &str,
    sh_n: usize,
    content: &str,
) {
    let dir = runs_base.join(run_id).join("jobs").join(job_id);
    fs_err::create_dir_all(&dir).expect("failed to create log dir");
    fs_err::write(dir.join(format!("sh-{sh_n}.log")), content).expect("failed to write log");
}
