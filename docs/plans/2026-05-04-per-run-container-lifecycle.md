# Per-run container lifecycle implementation plan

> **For Claude:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. For each commit, use the `commit` skill (jj-based; see user CLAUDE.md). For each test cycle, use `superpowers:test-driven-development`.

**Goal:** Land `lpmoszxo` (per-run container lifecycle) and `knmkqkvx` (route `sh` through docker exec) end-to-end, gated behind a `--executor host|docker` flag on `quire ci run` so host mode remains available for A/B comparison.

**Architecture:** See `docs/plans/2026-05-04-per-run-container-lifecycle-design.md`. Summary:

- `Run::execute` gains `workspace: &Path` and `executor: Executor` parameters.
- Workspace materialization (`git archive | tar -x` to `$XDG_CACHE_HOME/quire/runs/<repo>/<run-id>/workspace/`) happens in the CLI layer before `Run::execute` is called.
- Host mode runs `sh` locally with `cwd` defaulted to the workspace.
- Docker mode builds `.quire/Dockerfile`, starts a long-lived container, dispatches each `sh` through `docker exec`, and tears down via RAII.

**Tech stack:** Rust 2024, mlua + Fennel, std::process::Command (no new crates), jiff for timestamps, `xdg` crate (already in deps? — verify in Task 1).

---

## Pre-flight check

Before Task 1, verify the design doc commit landed and the working copy is clean:

```bash
jj log -r 'main..@ | @' --limit 5
```

Expected: design doc commit (`Document per-run container lifecycle design`) on top of `main`, with `@` either empty or holding new edits.

---

## Phase 1 — Foundations (no docker required)

### Task 1: Plumb `Executor` enum and `workspace` parameter through `Run::execute`

This is a no-op signature change: `Executor::Host` is the only variant, and the workspace path is accepted but unused. Goal is to update every call site once so later tasks add behavior without touching signatures.

**Files:**
- Modify: `src/ci/mod.rs` — re-export `Executor`.
- Modify: `src/ci/run.rs` — define `Executor` enum, update `Run::execute` signature, update all 14 in-module test call sites.
- Modify: `src/bin/quire/commands/ci.rs:79` — pass `Executor::Host` and a workspace `&Path`.

**Step 1: Add the enum**

```rust
// src/ci/run.rs (top, near RunState)
/// The execution mode for a run. Host runs `sh` directly on the host.
/// Docker materializes a container and routes `sh` through `docker exec`.
#[derive(Debug, Clone)]
pub enum Executor {
    Host,
    // Docker variant added in Task 5.
}
```

**Step 2: Update `Run::execute` signature**

Change to:
```rust
pub fn execute(
    mut self,
    pipeline: Pipeline,
    secrets: HashMap<String, SecretString>,
    git_dir: &std::path::Path,
    workspace: &std::path::Path,
    executor: Executor,
) -> Result<HashMap<String, Vec<ShOutput>>>
```

Body unchanged. The `workspace` and `executor` parameters are not yet read.

**Step 3: Update test call sites**

Each test in `src/ci/run.rs` that calls `run.execute(pipeline, ..., Path::new("."))` becomes:
```rust
let workspace = _dir.path().join("ws");
fs_err::create_dir_all(&workspace).expect("mkdir workspace");
run.execute(pipeline, secrets, Path::new("."), &workspace, Executor::Host)
```

The 14 in-module call sites all use `tmp_quire()` which gives back a `_dir: TempDir`. Hoist the workspace creation into a helper if it gets repetitive.

**Step 4: Update CLI call site (`src/bin/quire/commands/ci.rs:79`)**

```rust
let workspace = tmp.path().join("workspace");
fs_err::create_dir_all(&workspace).into_diagnostic()?;
let exec_result = run.execute(
    pipeline,
    secrets,
    &repo_path.join(".git"),
    &workspace,
    Executor::Host,
);
```

**Step 5: Verify**

```bash
cargo check --tests
cargo test --lib ci::run
```

Expected: clean compile, all 14 `Run::execute` tests pass unchanged.

**Step 6: Commit** — invoke the `commit` skill. Title: `Plumb Executor and workspace through Run::execute`.

---

### Task 2: Materialize workspace via `git archive | tar -x` in the CLI

Add the materialization step in `commands::ci::run`. `Run::execute` still ignores the workspace path; this task is exclusively about CLI plumbing and a new helper.

**Files:**
- Create: helper `materialize_workspace(git_dir: &Path, sha: &str, workspace: &Path) -> Result<()>` in `src/ci/run.rs` (or a new `src/ci/workspace.rs` if you prefer — judgment call; mirror tests put their git-shelling in `mirror.rs`, so co-locating here is fine).
- Modify: `src/bin/quire/commands/ci.rs` — call materialization before `run.execute`.
- Test: in-module test in `src/ci/run.rs`.

**Step 1: Failing test for materialization**

Use the same `git init` pattern as `src/ci/mirror.rs:259-273`. Set up a temp repo with one file committed, capture the SHA, call `materialize_workspace`, and assert the file is present in the destination.

```rust
#[test]
fn materialize_workspace_extracts_archive_at_sha() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_repo = dir.path().join("src");
    fs_err::create_dir_all(&src_repo).expect("mkdir src");

    let env_vars: [(&str, &str); 6] = [
        ("GIT_AUTHOR_NAME", "test"),
        ("GIT_AUTHOR_EMAIL", "test@test"),
        ("GIT_COMMITTER_NAME", "test"),
        ("GIT_COMMITTER_EMAIL", "test@test"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ];

    for cmd in [
        vec!["init", "-b", "main"],
        vec!["commit", "--allow-empty", "-m", "initial"],
    ] {
        let out = std::process::Command::new("git")
            .args(&cmd)
            .current_dir(&src_repo)
            .envs(env_vars)
            .output()
            .expect("run git");
        assert!(out.status.success());
    }
    fs_err::write(src_repo.join("hello.txt"), "hi\n").expect("write");
    for cmd in [vec!["add", "."], vec!["commit", "-m", "add file"]] {
        std::process::Command::new("git")
            .args(&cmd)
            .current_dir(&src_repo)
            .envs(env_vars)
            .output()
            .expect("run git");
    }
    let sha_out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&src_repo)
        .envs(env_vars)
        .output()
        .expect("rev-parse");
    let sha = String::from_utf8(sha_out.stdout).unwrap().trim().to_string();

    let workspace = dir.path().join("ws");
    materialize_workspace(&src_repo.join(".git"), &sha, &workspace).expect("materialize");
    assert_eq!(fs_err::read_to_string(workspace.join("hello.txt")).unwrap(), "hi\n");
}
```

**Step 2: Run, verify failure**

`cargo test --lib materialize_workspace_extracts_archive_at_sha` — expect failure (function doesn't exist).

**Step 3: Implement**

```rust
/// Materialize a working tree at `sha` into `workspace` via
/// `git archive | tar -x`. The workspace dir must exist and be empty.
pub(crate) fn materialize_workspace(
    git_dir: &Path,
    sha: &str,
    workspace: &Path,
) -> Result<()> {
    use std::process::{Command, Stdio};

    fs_err::create_dir_all(workspace)?;

    let mut archive = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["archive", sha])
        .stdout(Stdio::piped())
        .spawn()?;
    let archive_stdout = archive.stdout.take().expect("piped stdout");

    let mut tar = Command::new("tar")
        .args(["-x", "-C"])
        .arg(workspace)
        .stdin(Stdio::from(archive_stdout))
        .spawn()?;

    let archive_status = archive.wait()?;
    let tar_status = tar.wait()?;
    if !archive_status.success() || !tar_status.success() {
        return Err(Error::WorkspaceMaterializationFailed {
            // Wire `source` in Task 3 once the variant exists; for now
            // use a placeholder Error::Io.
            source: std::io::Error::other(format!(
                "git archive exited {archive_status}, tar exited {tar_status}"
            )),
        });
    }
    Ok(())
}
```

NOTE: `Error::WorkspaceMaterializationFailed` doesn't exist yet — Task 3 adds it. For this task, surface failures via the existing `Error::Io` (or `Error::Git` if more apt). Task 3 swaps it.

**Step 4: Run test, verify passes**

`cargo test --lib materialize_workspace_extracts_archive_at_sha` — expect pass.

**Step 5: Wire into CLI**

In `src/bin/quire/commands/ci.rs`, before `run.execute(...)`:

```rust
let workspace = tmp.path().join("workspace");
quire::ci::materialize_workspace(&repo_path.join(".git"), &commit.sha, &workspace)
    .into_diagnostic()?;
```

(Re-export `materialize_workspace` from `src/ci/mod.rs`.)

**Step 6: Smoke test**

```bash
cargo run -- ci run
```

Expected: works as before — but workspace is now populated. Add a `ls $TMPDIR/.../workspace` check via `eprintln!` or just trust the unit test.

**Step 7: Commit** — invoke `commit` skill. Title: `Materialize workspace via git archive before run`.

---

### Task 3: Add new error variants

**Files:**
- Modify: `src/error.rs` (or wherever `Error` is defined).
- Modify: `src/ci/run.rs` — swap the placeholder `Error::Io` in `materialize_workspace` for the new variant.

**Step 1: Locate the Error enum**

`grep -n "pub enum Error" src/error.rs src/lib.rs` — find the canonical definition.

**Step 2: Add three variants**

```rust
#[error("workspace materialization failed: {source}")]
WorkspaceMaterializationFailed { source: std::io::Error },

#[error("image build failed: {source}")]
ImageBuildFailed { source: std::io::Error },

#[error("container start failed: {source}")]
ContainerStartFailed { source: std::io::Error },
```

(Match the existing thiserror/miette idiom in `src/error.rs`. If the project uses `Diagnostic` with codes, follow that pattern.)

**Step 3: Rewire `materialize_workspace`**

Swap the placeholder error in Task 2's implementation for `Error::WorkspaceMaterializationFailed`.

**Step 4: Failing test for the error class**

```rust
#[test]
fn materialize_workspace_errors_on_unknown_sha() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src_repo = dir.path().join("src");
    fs_err::create_dir_all(&src_repo).expect("mkdir");
    // ... git init -b main in src_repo (use the env_vars block from Task 2) ...

    let workspace = dir.path().join("ws");
    let err = materialize_workspace(
        &src_repo.join(".git"),
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        &workspace,
    )
    .expect_err("expected failure on unknown SHA");
    assert!(matches!(err, Error::WorkspaceMaterializationFailed { .. }));
}
```

**Step 5: Run, verify pass**

**Step 6: Commit** — Title: `Add container-lifecycle error variants`.

---

### Task 4: Use workspace as default `cwd` in host mode

`Cmd::run` currently inherits the parent process's CWD when `opts.cwd` is `None`. Host mode should default to the materialized workspace.

**Files:**
- Modify: `src/ci/runtime.rs` — `Runtime::sh` passes a workspace fallback into `Cmd::run` opts.
- Modify: `src/ci/run.rs` — `Run::execute` passes `workspace` into the runtime.
- Test: extend an existing host-mode test to confirm default cwd is the workspace.

**Step 1: Failing test**

```rust
// src/ci/run.rs tests
#[test]
fn host_mode_defaults_cwd_to_workspace() {
    let (_dir, quire) = tmp_quire();
    let runs = test_runs(&quire);
    let run = runs.create(&test_meta()).expect("create");

    let workspace = quire.base_dir().join("ws");
    fs_err::create_dir_all(&workspace).expect("mkdir ws");
    fs_err::write(workspace.join("marker"), "x").expect("write");

    let pipeline = load(
        r#"(local ci (require :quire.ci))
(ci.job :pwd [:quire/push] (fn [{: sh}] (sh ["ls"])))"#,
    );

    let outputs = run
        .execute(pipeline, HashMap::new(), Path::new("."), &workspace, Executor::Host)
        .expect("execute");
    let pwd = &outputs["pwd"];
    assert!(pwd[0].stdout.contains("marker"), "expected workspace ls, got: {}", pwd[0].stdout);
}
```

**Step 2: Run, verify failure**

Currently `ls` runs in the parent process's CWD, so `marker` won't be in stdout.

**Step 3: Implement workspace fallback**

Plumb `workspace: PathBuf` into `Runtime::new` and store it. In `Runtime::sh`, when `opts.cwd` is `None`, set it to the workspace:

```rust
pub(super) fn sh(&self, cmd: Cmd, mut opts: ShOpts) -> crate::Result<ShOutput> {
    if opts.cwd.is_none() {
        opts.cwd = Some(self.workspace.to_string_lossy().into_owned());
    }
    let output = cmd.run(opts)?;
    // ... rest unchanged
}
```

`Run::execute` passes `workspace.to_path_buf()` into `Runtime::new`.

**Step 4: Run test, verify pass**

**Step 5: Audit — does this break existing tests?**

The `sh_honors_cwd` test in `runtime.rs` sets `cwd` explicitly, so no break. Tests that use `pwd` or rely on parent CWD via implicit inheritance might break. Run `cargo test --lib` and inspect failures.

**Step 6: Commit** — Title: `Default sh cwd to workspace in host mode`.

---

## Phase 2 — Docker primitives

### Task 5: `docker_build` helper

Implements `docker build --file <dockerfile> --tag <tag> <context>`. The helper is layout-agnostic: callers compose the dockerfile path themselves. Task 8 (the only caller) passes `workspace.join(".quire/Dockerfile")` as the dockerfile and `workspace` as the build context. Keeping the path policy at the orchestration layer avoids coupling the docker shell-out helper to quire's specific workspace layout.

**Files:**
- Create: `src/ci/docker.rs` — module for docker shell-out helpers.
- Modify: `src/ci/mod.rs` — `pub(crate) mod docker;`.
- Test: in-module test gated behind a `docker` runtime check.

**Step 1: Decide on the docker-availability gate**

Helper at top of `docker.rs`:
```rust
/// Returns true if `docker info` exits 0 within ~3s. Tests that
/// require docker `return;` early when this returns false.
pub(crate) fn is_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
```

**Step 2: Failing test** (gated)

```rust
#[test]
#[ignore = "requires docker"]
fn docker_build_succeeds_with_minimal_dockerfile() {
    if !is_available() {
        eprintln!("docker not available, skipping");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let context = dir.path();
    let dockerfile = context.join("Dockerfile");
    fs_err::write(&dockerfile, "FROM alpine:3.19\nRUN echo built\n")
        .expect("write Dockerfile");

    let tag = "quire-ci/test-task5:test";
    docker_build(&dockerfile, context, tag).expect("build should succeed");

    // Cleanup.
    let _ = std::process::Command::new("docker")
        .args(["image", "rm", tag])
        .output();
}
```

**Step 3: Run with `--ignored`**

```bash
cargo test --lib docker_build_succeeds_with_minimal_dockerfile -- --ignored
```

Expect failure (function doesn't exist).

**Step 4: Implement**

```rust
pub(crate) fn docker_build(dockerfile: &Path, context: &Path, tag: &str) -> Result<()> {
    let output = std::process::Command::new("docker")
        .arg("build")
        .arg("--file")
        .arg(dockerfile)
        .arg("--tag")
        .arg(tag)
        .arg(context)
        .output()
        .map_err(|e| Error::ImageBuildFailed { source: e })?;
    if !output.status.success() {
        return Err(Error::ImageBuildFailed {
            source: std::io::Error::other(String::from_utf8_lossy(&output.stderr).into_owned()),
        });
    }
    Ok(())
}
```

**Step 5: Run, verify pass**

**Step 6: Failing-build test** (gated)

```rust
#[test]
#[ignore = "requires docker"]
fn docker_build_errors_on_bad_dockerfile() {
    if !is_available() { return; }
    let dir = tempfile::tempdir().expect("tempdir");
    let context = dir.path();
    let dockerfile = context.join("Dockerfile");
    fs_err::write(&dockerfile, "GARBAGE\n").expect("write");

    let err = docker_build(&dockerfile, context, "quire-ci/test-task5-bad:test")
        .expect_err("should fail");
    assert!(matches!(err, Error::ImageBuildFailed { .. }));
}
```

Run, verify pass.

**Step 7: Commit** — Title: `Add docker_build helper`.

---

### Task 6: `docker_run` helper + `ContainerSession` with Drop

`docker run -d --rm --mount type=bind,src=<workspace>,dst=/work -w /work <tag> sleep infinity`. Capture container ID. Drop calls `docker stop --time 5 <id>`.

**Files:**
- Modify: `src/ci/docker.rs`.

**Step 1: Failing test for `docker_run`**

```rust
#[test]
#[ignore = "requires docker"]
fn docker_run_starts_container_and_returns_id() {
    if !is_available() { return; }
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path();
    // Use alpine directly — no build step needed for this test.
    let tag = "alpine:3.19";

    let session = ContainerSession::start(workspace, tag).expect("start");
    assert!(!session.container_id.is_empty());
    assert_eq!(session.container_id.len(), 64); // docker ID hex length

    // session drops here, docker stop runs.
}
```

**Step 2: Implement `ContainerSession`**

```rust
pub(crate) struct ContainerSession {
    pub(crate) container_id: String,
    pub(crate) image_tag: String,
    pub(crate) container_started_at: jiff::Timestamp,
    // run_dir + container.yml writer added in Task 7.
}

impl ContainerSession {
    pub(crate) fn start(workspace: &Path, image_tag: &str) -> Result<Self> {
        let mount = format!(
            "type=bind,src={},dst=/work",
            workspace.to_string_lossy()
        );
        let output = std::process::Command::new("docker")
            .args(["run", "-d", "--rm", "--mount"])
            .arg(&mount)
            .args(["-w", "/work", image_tag, "sleep", "infinity"])
            .output()
            .map_err(|e| Error::ContainerStartFailed { source: e })?;
        if !output.status.success() {
            return Err(Error::ContainerStartFailed {
                source: std::io::Error::other(
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                ),
            });
        }
        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(Self {
            container_id,
            image_tag: image_tag.to_string(),
            container_started_at: jiff::Timestamp::now(),
        })
    }
}

impl Drop for ContainerSession {
    fn drop(&mut self) {
        let result = std::process::Command::new("docker")
            .args(["stop", "--time", "5"])
            .arg(&self.container_id)
            .output();
        if let Err(e) = result {
            tracing::error!(container_id = %self.container_id, error = %e, "docker stop failed");
        } else if let Ok(out) = result {
            if !out.status.success() {
                tracing::error!(
                    container_id = %self.container_id,
                    stderr = %String::from_utf8_lossy(&out.stderr),
                    "docker stop returned non-zero",
                );
            }
        }
    }
}
```

**Step 3: Run, verify pass**

**Step 4: Verify cleanup**

After the test, `docker ps -a --filter id=<id>` should show no container (the `--rm` flag handled removal once stop completed).

Add a follow-up test:
```rust
#[test]
#[ignore = "requires docker"]
fn container_session_drop_stops_container() {
    if !is_available() { return; }
    let dir = tempfile::tempdir().expect("tempdir");
    let id = {
        let session = ContainerSession::start(dir.path(), "alpine:3.19").expect("start");
        session.container_id.clone()
    }; // drop here

    // Give docker a moment to clean up.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let out = std::process::Command::new("docker")
        .args(["ps", "-a", "-q", "--filter"])
        .arg(format!("id={id}"))
        .output()
        .expect("docker ps");
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "container should be removed after drop",
    );
}
```

**Step 5: Commit** — Title: `Add ContainerSession with RAII teardown`.

---

### Task 7: `container.yml` writes

Persist build/start/stop timestamps. Each write is atomic via `write_yaml`.

**Files:**
- Modify: `src/ci/run.rs` — define `ContainerYaml` (or place in `docker.rs`).
- Modify: `src/ci/docker.rs` — `ContainerSession` accepts a run-dir path and writes timestamps.

**Step 1: Define the schema**

```rust
// src/ci/run.rs
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ContainerRecord {
    pub image_tag: Option<String>,
    pub container_id: Option<String>,
    pub build_started_at: Option<jiff::Timestamp>,
    pub build_finished_at: Option<jiff::Timestamp>,
    pub container_started_at: Option<jiff::Timestamp>,
    pub container_stopped_at: Option<jiff::Timestamp>,
}

impl Run {
    pub(crate) fn write_container_record(&self, rec: &ContainerRecord) -> Result<()> {
        write_yaml(&self.path().join("container.yml"), rec)
    }
    pub(crate) fn read_container_record(&self) -> Result<ContainerRecord> {
        read_yaml(&self.path().join("container.yml"))
    }
}
```

**Step 2: Failing test**

```rust
#[test]
fn container_record_round_trips_through_yaml() {
    let (_dir, quire) = tmp_quire();
    let runs = test_runs(&quire);
    let run = runs.create(&test_meta()).expect("create");

    let now = jiff::Timestamp::now();
    let rec = ContainerRecord {
        image_tag: Some("quire-ci/test:abc".into()),
        container_id: Some("9f3b8a72c1d4".into()),
        build_started_at: Some(now),
        ..Default::default()
    };
    run.write_container_record(&rec).expect("write");
    let read = run.read_container_record().expect("read");
    assert_eq!(read.image_tag, Some("quire-ci/test:abc".into()));
    assert_eq!(read.container_id, Some("9f3b8a72c1d4".into()));
}
```

**Step 3: Run, verify pass**

(Should be straightforward — leveraging existing `write_yaml`/`read_yaml`.)

**Step 4: Wire `ContainerSession` to write timestamps**

`ContainerSession::start` accepts `&Run` (or a `WriteContainerYaml` callback) and writes `container_started_at` after a successful `docker run`. `Drop` writes `container_stopped_at` before calling `docker stop`. Build timestamps come from a separate path (Task 8 sequence).

This changes `ContainerSession::start`'s signature. Update the Task 6 tests accordingly — they can pass an option/callback that writes to a tempfile.

**Step 5: Commit** — Title: `Persist container metadata to container.yml`.

---

## Phase 3 — Wire docker mode into the runtime

### Task 8: Add `Executor::Docker` variant; build/start sequence in `Run::execute`

Now the host-only `Executor` enum gains a docker variant, and `Run::execute` orchestrates build → start → run → drop.

**Files:**
- Modify: `src/ci/run.rs` — `Executor::Docker { tag_prefix: String }` (or similar opts struct).
- Modify: `src/ci/run.rs` — `Run::execute` branches on `executor`.

**Step 1: Extend `Executor`**

```rust
pub enum Executor {
    Host,
    Docker, // opts struct can land later if needed
}
```

**Step 2: In `Run::execute`, branch the setup**

After the existing `Runtime` construction, before the topo loop:

```rust
let _container = match executor {
    Executor::Host => None,
    Executor::Docker => {
        let mut rec = ContainerRecord::default();
        let tag = format!("quire-ci/{}:{}", repo_segment, self.id);
        rec.build_started_at = Some(Timestamp::now());
        self.write_container_record(&rec)?;

        let dockerfile = workspace.join(".quire/Dockerfile");
        crate::ci::docker::docker_build(&dockerfile, workspace, &tag)?;
        rec.image_tag = Some(tag.clone());
        rec.build_finished_at = Some(Timestamp::now());
        self.write_container_record(&rec)?;

        let session = crate::ci::docker::ContainerSession::start(workspace, &tag)?;
        rec.container_id = Some(session.container_id.clone());
        rec.container_started_at = Some(session.container_started_at);
        self.write_container_record(&rec)?;

        Some(session)
    }
};
```

`repo_segment` is derived from the run-dir path (or from `meta`-driven repo identity). Sanitize `/` → `_`.

**Step 3: Stash session on `Runtime`**

Add an `executor: ExecutorRuntime` field to `Runtime`:
```rust
pub(super) enum ExecutorRuntime {
    Host,
    Docker(crate::ci::docker::ContainerSession),
}
```

Pass into `Runtime::new`. The drop semantics work because `Runtime` owns the session; when `Run::execute` returns, `Runtime` drops, session drops, container is stopped.

**Step 4: Failing integration test** (gated)

```rust
#[test]
#[ignore = "requires docker"]
fn execute_docker_mode_runs_sh_in_container() {
    if !crate::ci::docker::is_available() { return; }
    // Set up a real git repo, materialize, and execute a pipeline in docker.
    // ... (use the same git-init pattern as Task 2) ...
    // Pipeline: (sh ["uname" "-a"]) — assert output contains "Linux"
    //           even if host is macOS.
}
```

**Step 5: Implement, run, verify**

**Step 6: Commit** — Title: `Build and run the per-run container`.

---

### Task 9: Route `(sh ...)` through `docker exec`

`Runtime::sh` rewrites the command when `executor: Docker` is active.

**Files:**
- Modify: `src/ci/runtime.rs` — `Runtime::sh` and `Cmd::into` (or a new method) emit `docker exec` argv.

**Step 1: Faked-dispatcher unit test**

A unit test that doesn't need real docker — points the executor at a shim `docker` script in a temp `$PATH` that just records its argv.

```rust
#[test]
fn runtime_sh_in_docker_mode_invokes_docker_exec() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("bin");
    fs_err::create_dir_all(&bin).expect("mkdir bin");

    // Shim that prints its argv to stdout (one per line) and exits 0.
    let shim = bin.join("docker");
    fs_err::write(&shim, "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\"; done\n").expect("write shim");
    use std::os::unix::fs::PermissionsExt;
    fs_err::set_permissions(&shim, fs_err::Permissions::from_mode(0o755)).expect("chmod");

    // Construct an ExecutorRuntime::Docker with a synthetic ContainerSession.
    // ... assert stdout contains exec, container-id, the program, and args ...
}
```

(The synthetic `ContainerSession` requires a constructor that doesn't shell out — add a `pub(crate) fn for_test(id: String) -> Self`.)

**Step 2: Implement docker-exec dispatch**

In `Runtime::sh`, before calling `Cmd::run(opts)`:

```rust
let cmd = match &self.executor {
    ExecutorRuntime::Host => cmd,
    ExecutorRuntime::Docker(session) => cmd.wrap_in_docker_exec(&session.container_id, &opts),
};
```

`wrap_in_docker_exec` constructs a new `Cmd::Argv` with prefix `["docker", "exec", "-i", "-w", &cwd, "-e", "K=V", ..., id, program, args...]`. After wrapping, clear `opts.cwd` and `opts.env` since they've been embedded into the argv.

**Step 3: Run faked test, verify pass**

**Step 4: Real-docker integration test** (extends Task 8's test)

Confirms `(sh ["sh" "-c" "echo $HOSTNAME"])` returns the container's hostname, not the host's.

**Step 5: Commit** — Title: `Route sh through docker exec in docker mode`.

---

## Phase 4 — CLI flag and verification

### Task 10: Add `--executor host|docker` flag to `quire ci run`

**Files:**
- Modify: `src/bin/quire/main.rs` — add `--executor` to `CiCommands::Run`.
- Modify: `src/bin/quire/commands/ci.rs` — accept `Executor`, materialize, pass through.

**Step 1: Add the flag**

```rust
// src/bin/quire/main.rs
#[derive(clap::ValueEnum, Clone, Debug)]
enum CliExecutor { Host, Docker }

CiCommands::Run {
    sha: Option<String>,
    #[arg(long, value_enum, default_value_t = CliExecutor::Host)]
    executor: CliExecutor,
}
```

**Step 2: Translate at the call site**

In `main.rs:188`, pass `executor` through. In `commands::ci::run`, take a `CliExecutor` (or a translated `Executor`) and pass to `run.execute`.

**Step 3: Smoke test both modes**

```bash
cargo run -- ci run                         # host
cargo run -- ci run --executor docker       # docker (requires .quire/Dockerfile)
```

For the docker mode smoke test, ensure `.quire/Dockerfile` exists in the working repo (write a one-liner if not). The smoke test is informational — formal coverage comes from the gated integration tests in Tasks 8–9.

**Step 4: Commit** — Title: `Add --executor flag to quire ci run`.

---

### Task 11: Verification pass

**Step 1: Full test sweep**

```bash
cargo test --lib            # all unit tests
cargo test                  # plus integration tests in tests/
cargo test -- --ignored docker_  # gated docker tests, if docker is available
```

Expected: all green.

**Step 2: Manual end-to-end**

In a checkout with a working `.quire/Dockerfile`:
1. `cargo run -- ci run` — host mode runs to completion.
2. `cargo run -- ci run --executor docker` — docker mode builds, starts, runs jobs, tears down.
3. `cat <run-dir>/container.yml` — confirm tag/id/timestamps populated.
4. `docker ps -a` — confirm no leftover `quire-ci/*` containers.
5. `docker images quire-ci/*` — confirm image is tagged with the run-id.

**Step 3: Update task tracker**

Mark `lpmoszxo` and `knmkqkvx` as done in ranger:

```bash
ranger task edit lpmoszxo --state done
ranger task edit knmkqkvx --state done
```

**Step 4: Final commit if any cleanup landed**

---

## Out of scope (explicitly deferred)

These follow-ups already exist as ranger icebox tasks; do **not** address in this plan:

- `xkyuzkoz` — Resolve runtime image from declared `(ci.image ...)`.
- `ptqsovvz` — Default base image when no declaration or Dockerfile.
- `uvwnkwmx` — Prune old `quire-ci/*` images and run workspaces.
- `lylszxrn` — Reconcile container orphans on quire startup.
- `kstutwkw` — Investigate git worktree / jj workspace for materialization.
- `xrupozur` — Streaming JSONL log persistence.
- `zmtuqwly` — Distinct container-died failure mode.
- `vzzrxntq` — May be archived once `--executor host` ships (decide at Task 11).

---

## Notes on jj usage

- All commits via `jj commit` (use the `commit` skill).
- The skill enforces `Assisted-by` trailer and avoids destructive operations.
- Keep each task to one commit unless a task has been split mid-way for hygiene.
