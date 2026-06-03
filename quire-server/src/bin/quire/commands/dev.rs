//! Dev utilities: seed data for local development.

use miette::{Context, IntoDiagnostic, Result};
use rusqlite::params;
use uuid::Uuid;

use quire::Quire;

/// Seed a tempdir with realistic CI run data and return a `Quire` pointing at it.
///
/// Creates a fresh tempdir under `std::env::temp_dir()`, inserts a fixed corpus
/// of runs covering every interesting state (succeeded, failed, active, queued,
/// canceled) with matching on-disk log artifacts. Idempotent — same input,
/// same output.
pub fn seed() -> Result<Quire> {
    Seeder::new()?.run()
}

/// One run with its jobs. `pushed_delta_ms` is offset from "now" at seed time;
/// `dispatched_delta_ms` is offset from `pushed`; `duration_ms` is how long the
/// run ran after dispatching.
struct SeedRun {
    outcome: Option<&'static str>,
    sha: &'static str,
    ref_name: &'static str,
    pushed_delta_ms: i64,
    dispatched_delta_ms: Option<i64>,
    duration_ms: Option<i64>,
    jobs: Vec<SeedJob>,
}

/// `started_delta_ms` is offset from the run's start; `duration_ms` is how long
/// the job ran (None if still active).
struct SeedJob {
    job_id: &'static str,
    state: &'static str,
    exit_code: Option<i32>,
    started_delta_ms: i64,
    duration_ms: Option<i64>,
    events: Vec<SeedShEvent>,
}

/// `started_delta_ms` is offset from the job's start.
struct SeedShEvent {
    started_delta_ms: i64,
    duration_ms: i64,
    exit_code: i32,
    cmd: &'static str,
    log: Option<&'static str>,
}

struct Seeder {
    quire: Quire,
    db: rusqlite::Connection,
    base_ms: i64,
    runs: Vec<SeedRun>,
}

impl Seeder {
    fn new() -> Result<Self> {
        let dir = tempfile::tempdir()
            .into_diagnostic()
            .context("failed to create tempdir")?;

        // Leak the TempDir so it outlives the function. The server will
        // clean up on shutdown, or the OS will when the process exits.
        let base_dir = dir.keep();
        tracing::info!(path = %base_dir.display(), "seeded tempdir");

        let quire = Quire::load(base_dir)?;

        // Clone the current working directory as a bare repo so the web view
        // has real git data to render.
        fs_err::create_dir_all(quire.repos_dir())
            .into_diagnostic()
            .context("failed to create repos dir")?;
        let bare_repo = quire.repos_dir().join("example.git");
        let src = std::env::current_dir()
            .into_diagnostic()
            .context("failed to get current directory")?;
        let status = std::process::Command::new("git")
            .args(["clone", "--bare"])
            .arg(&src)
            .arg(&bare_repo)
            .status()
            .into_diagnostic()
            .context("failed to run git clone")?;
        if !status.success() {
            miette::bail!("git clone --bare failed with {status}");
        }

        let mut db = quire::db::open(&quire.db_path())
            .into_diagnostic()
            .context("failed to open database")?;
        quire::db::migrate(&mut db).context("failed to run migrations")?;

        Ok(Self {
            quire,
            db,
            base_ms: jiff::Timestamp::now().as_millisecond(),
            runs: build_runs(),
        })
    }

    fn run(self) -> Result<Quire> {
        for run in &self.runs {
            self.insert_run(run)?;
        }

        let run_count: i64 = self
            .db
            .query_row("SELECT count(*) FROM runs", [], |row| row.get(0))
            .into_diagnostic()?;

        tracing::info!(%run_count, "seeded database");
        Ok(self.quire)
    }

    fn insert_run(&self, run: &SeedRun) -> Result<()> {
        let repo = "example.git";
        // v7 UUIDs so the IDs are time-sortable in addition to unique.
        let run_id = Uuid::now_v7().to_string();

        let pushed_at = self.base_ms + run.pushed_delta_ms;
        let dispatched_at = run.dispatched_delta_ms.map(|d| pushed_at + d);
        let resolved_at = dispatched_at.zip(run.duration_ms).map(|(s, d)| s + d);

        self.db
            .execute(
                "INSERT INTO runs (id, repo, ref_name, sha, pushed_at_ms,
                               created_at, dispatched_at, resolved_at, outcome)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    run_id,
                    repo,
                    run.ref_name,
                    run.sha,
                    pushed_at,
                    pushed_at, // created_at = pushed_at_ms
                    dispatched_at,
                    resolved_at,
                    run.outcome,
                ],
            )
            .into_diagnostic()?;

        let Some(run_dispatched_at) = dispatched_at else {
            return Ok(()); // queued run; no jobs to insert.
        };

        let logs_base = self
            .quire
            .base_dir()
            .join("runs")
            .join("example.git")
            .join(&run_id);

        for job in &run.jobs {
            let job_started_at = run_dispatched_at + job.started_delta_ms;
            let job_finished_at = job.duration_ms.map(|d| job_started_at + d);

            self.db
                .execute(
                    "INSERT INTO jobs (run_id, job_id, state, exit_code, started_at_ms, finished_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        run_id,
                        job.job_id,
                        job.state,
                        job.exit_code,
                        job_started_at,
                        job_finished_at,
                    ],
                )
                .into_diagnostic()?;

            for (idx, event) in job.events.iter().enumerate() {
                let started_at = job_started_at + event.started_delta_ms;
                let finished_at = started_at + event.duration_ms;
                self.db
                    .execute(
                        "INSERT INTO sh (run_id, job_id, started_at_ms, finished_at_ms, exit_code, cmd)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            run_id,
                            job.job_id,
                            started_at,
                            finished_at,
                            event.exit_code,
                            event.cmd,
                        ],
                    )
                    .into_diagnostic()?;

                if let Some(content) = event.log {
                    let dir = logs_base.join("jobs").join(job.job_id);
                    fs_err::create_dir_all(&dir)
                        .into_diagnostic()
                        .context("failed to create log dir")?;
                    fs_err::write(dir.join(format!("sh-{}.log", idx + 1)), content)
                        .into_diagnostic()
                        .context("failed to write log")?;
                }
            }
        }

        Ok(())
    }
}

fn build_runs() -> Vec<SeedRun> {
    vec![
        // Run 1 — succeeded, all jobs passed.
        SeedRun {
            outcome: Some("succeeded"),
            sha: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            ref_name: "refs/heads/main",
            pushed_delta_ms: 0,
            dispatched_delta_ms: Some(1000),
            duration_ms: Some(4000),
            jobs: vec![
                SeedJob {
                    job_id: "build",
                    state: "succeeded",
                    exit_code: Some(0),
                    started_delta_ms: 0,
                    duration_ms: Some(2000),
                    events: vec![
                        SeedShEvent {
                            started_delta_ms: 0,
                            duration_ms: 1500,
                            exit_code: 0,
                            cmd: "cargo build --release",
                            log: Some(include_str!("fixtures/run-1-build-1.log")),
                        },
                        SeedShEvent {
                            started_delta_ms: 1500,
                            duration_ms: 500,
                            exit_code: 0,
                            cmd: "cargo clippy -- -D warnings",
                            log: Some(include_str!("fixtures/run-1-build-2.log")),
                        },
                    ],
                },
                SeedJob {
                    job_id: "test",
                    state: "succeeded",
                    exit_code: Some(0),
                    started_delta_ms: 2000,
                    duration_ms: Some(2000),
                    events: vec![SeedShEvent {
                        started_delta_ms: 0,
                        duration_ms: 1800,
                        exit_code: 0,
                        cmd: "cargo test --workspace",
                        log: Some(include_str!("fixtures/run-1-test-1.log")),
                    }],
                },
            ],
        },
        // Run 2 — failed, one job failed.
        SeedRun {
            outcome: Some("failed-pipeline"),
            sha: "cafebabecafebabecafebabecafebabecafebabe",
            ref_name: "refs/heads/main",
            pushed_delta_ms: -600_000,
            dispatched_delta_ms: Some(1000),
            duration_ms: Some(7000),
            jobs: vec![
                SeedJob {
                    job_id: "build",
                    state: "succeeded",
                    exit_code: Some(0),
                    started_delta_ms: 0,
                    duration_ms: Some(2000),
                    events: vec![SeedShEvent {
                        started_delta_ms: 0,
                        duration_ms: 2000,
                        exit_code: 0,
                        cmd: "cargo build --release",
                        log: Some(include_str!("fixtures/run-2-build-1.log")),
                    }],
                },
                SeedJob {
                    job_id: "test",
                    state: "failed",
                    exit_code: Some(1),
                    started_delta_ms: 2000,
                    duration_ms: Some(5000),
                    events: vec![
                        SeedShEvent {
                            started_delta_ms: 0,
                            duration_ms: 2000,
                            exit_code: 0,
                            cmd: "cargo test --workspace",
                            log: Some(include_str!("fixtures/run-2-test-1.log")),
                        },
                        SeedShEvent {
                            started_delta_ms: 2000,
                            duration_ms: 3000,
                            exit_code: 1,
                            cmd: "cargo test -- --ignored",
                            log: Some(include_str!("fixtures/run-2-test-2.log")),
                        },
                    ],
                },
            ],
        },
        // Run 3 — superseded, pushed then rebased.
        SeedRun {
            outcome: Some("superseded"),
            sha: "1111111111111111111111111111111111111111",
            ref_name: "refs/heads/feature",
            pushed_delta_ms: -1_200_000,
            dispatched_delta_ms: Some(1000),
            duration_ms: Some(1000),
            jobs: vec![SeedJob {
                job_id: "build",
                state: "succeeded",
                exit_code: Some(0),
                started_delta_ms: 0,
                duration_ms: Some(1000),
                events: vec![SeedShEvent {
                    started_delta_ms: 0,
                    duration_ms: 1000,
                    exit_code: 0,
                    cmd: "cargo build --release",
                    log: None,
                }],
            }],
        },
        // Run 4 — active, still running.
        SeedRun {
            outcome: None,
            sha: "2222222222222222222222222222222222222222",
            ref_name: "refs/heads/main",
            pushed_delta_ms: -5000,
            dispatched_delta_ms: Some(1000),
            duration_ms: None,
            jobs: vec![SeedJob {
                job_id: "build",
                state: "active",
                exit_code: None,
                started_delta_ms: 0,
                duration_ms: None,
                events: vec![
                    SeedShEvent {
                        started_delta_ms: 0,
                        duration_ms: 2000,
                        exit_code: 0,
                        cmd: "cargo build --release",
                        log: None,
                    },
                    SeedShEvent {
                        started_delta_ms: 2000,
                        duration_ms: 1000,
                        exit_code: 0,
                        cmd: "cargo clippy -- -D warnings",
                        log: None,
                    },
                ],
            }],
        },
        // Run 5 — queued but not started.
        SeedRun {
            outcome: None,
            sha: "3333333333333333333333333333333333333333",
            ref_name: "refs/heads/main",
            pushed_delta_ms: -1000,
            dispatched_delta_ms: None,
            duration_ms: None,
            jobs: vec![],
        },
        // Run 6 — succeeded, multi-job: lint + build + test.
        SeedRun {
            outcome: Some("succeeded"),
            sha: "4444444444444444444444444444444444444444",
            ref_name: "refs/heads/v2",
            pushed_delta_ms: -3_600_000,
            dispatched_delta_ms: Some(2000),
            duration_ms: Some(10_000),
            jobs: vec![
                SeedJob {
                    job_id: "lint",
                    state: "succeeded",
                    exit_code: Some(0),
                    started_delta_ms: 0,
                    duration_ms: Some(2000),
                    events: vec![SeedShEvent {
                        started_delta_ms: 0,
                        duration_ms: 2000,
                        exit_code: 0,
                        cmd: "cargo fmt --check",
                        log: None,
                    }],
                },
                SeedJob {
                    job_id: "build",
                    state: "succeeded",
                    exit_code: Some(0),
                    started_delta_ms: 2000,
                    duration_ms: Some(4000),
                    events: vec![SeedShEvent {
                        started_delta_ms: 0,
                        duration_ms: 4000,
                        exit_code: 0,
                        cmd: "cargo build --release",
                        log: None,
                    }],
                },
                SeedJob {
                    job_id: "test",
                    state: "succeeded",
                    exit_code: Some(0),
                    started_delta_ms: 6000,
                    duration_ms: Some(4000),
                    events: vec![SeedShEvent {
                        started_delta_ms: 0,
                        duration_ms: 4000,
                        exit_code: 0,
                        cmd: "cargo test --workspace",
                        log: None,
                    }],
                },
            ],
        },
        // Run 7 — failed, orphaned (container died mid-run).
        SeedRun {
            outcome: Some("failed-orphaned"),
            sha: "5555555555555555555555555555555555555555",
            ref_name: "refs/heads/main",
            pushed_delta_ms: -7_200_000,
            dispatched_delta_ms: Some(1000),
            duration_ms: Some(59_000),
            jobs: vec![
                SeedJob {
                    job_id: "build",
                    state: "succeeded",
                    exit_code: Some(0),
                    started_delta_ms: 0,
                    duration_ms: Some(3000),
                    events: vec![SeedShEvent {
                        started_delta_ms: 0,
                        duration_ms: 3000,
                        exit_code: 0,
                        cmd: "cargo build --release",
                        log: Some(include_str!("fixtures/run-7-build-1.log")),
                    }],
                },
                SeedJob {
                    job_id: "test",
                    state: "failed",
                    exit_code: Some(137),
                    started_delta_ms: 3000,
                    duration_ms: Some(56_000),
                    events: vec![SeedShEvent {
                        started_delta_ms: 0,
                        duration_ms: 56_000,
                        exit_code: 137,
                        cmd: "cargo test --workspace",
                        log: Some(include_str!("fixtures/run-7-test-1.log")),
                    }],
                },
            ],
        },
    ]
}
