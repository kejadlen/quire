# Per-run container lifecycle

Captures the design for `lpmoszxo` (per-run container lifecycle) and `knmkqkvx` (route `sh` through `docker exec`), tackled together so the executor abstraction lands with both modes wired end-to-end. A `--executor` flag on `quire ci run` keeps host execution available for A/B comparison while the docker path matures.

## Scope

In scope:

- One container per run, materialized workspace bind-mounted in.
- `(sh ...)` dispatched through `docker exec` against that container.
- Image built from `.quire/Dockerfile` at run start.
- Persistent record of container metadata in the run directory.
- A `--executor host|docker` flag on `quire ci run` for local A/B testing.

Out of scope (covered by the follow-ups in the last section):

- `(ci.image "name")` declared-image resolution and a baked-in default base image.
- Multi-job DAG (`sxllwuxk`), per-repo cache (`zopyouwu`), streaming JSONL logs (`xrupozur`), distinct container-died failure (`zmtuqwly`), sandbox layers (`lsqluktu`, `rzsonvsx`).
- Pruning old images and old workspace directories.

## Architecture

`Run::execute` gains an `executor: Executor` parameter:

```rust
enum Executor {
    Host,
    Docker(DockerOpts),
}
```

`Runtime` stores either `ExecutorRuntime::Host` (no extra state) or `ExecutorRuntime::Docker(ContainerSession)`. The variant is the only thing `(sh ...)` branches on.

In Host mode, `Cmd::run` keeps today's path but defaults `cwd` to the run's materialized workspace. In Docker mode, `(sh ...)` rewrites the command:

- `Cmd::Argv { program, args }` becomes `docker exec -i <container-id> [-w <cwd>] [-e KEY=VAL ...] <program> <args...>`.
- `Cmd::Shell(s)` becomes `docker exec -i <container-id> [-w ...] [-e ...] sh -c "<s>"`.

`-i` keeps stdin attached. `-t` is never set, so stdout and stderr stay as separate streams. `cwd` and `env` translate to `-w` and `-e` flags rather than mutating the local `std::process::Command`.

## Workspace materialization

Both modes materialize the workspace before any docker work:

1. `mkdir -p $XDG_CACHE_HOME/quire/runs/<repo>/<run-id>/workspace`.
2. `git -C <git-dir> archive <sha> | tar -x -C <workspace>`.
3. Failure surfaces as `Error::WorkspaceMaterializationFailed { source }`, taking the run Pending → Active → Failed.

The workspace lives under `$XDG_CACHE_HOME` (with the standard fallback to `~/.cache/`) rather than inside the run directory. The run directory holds small metadata (`meta.yml`, `times.yml`, per-job logs); the workspace is potentially a large source tree with cache semantics. Keeping them apart means a backup of `runs/` does not also back up source trees, and the cache directory can be blown away without losing run records.

In Host mode, materialization is the only setup before the job loop. In Docker mode, build and start follow.

## Container lifecycle

Docker mode runs three steps after materialization, before the topo loop:

1. **Build.** Shell out to `docker build -f <workspace>/.quire/Dockerfile -t quire-ci/<repo>:<run-id> <workspace>`. The repo segment of the tag is sanitized (`/` → `_`). The build context is the materialized workspace, so `COPY` instructions in the Dockerfile see the source tree at the run's SHA. A missing `.quire/Dockerfile` is left to `docker build` to report — daemon errors and Dockerfile errors take the same path. Failure surfaces as `Error::ImageBuildFailed { source }`.

2. **Start.** `docker run -d --rm --mount type=bind,src=<workspace>,dst=/work -w /work quire-ci/<repo>:<run-id> sleep infinity`. Stdout is the container ID. `--rm` makes the daemon remove the container record on stop. Failure surfaces as `Error::ContainerStartFailed { source }`.

3. **Construct `ContainerSession`** holding the container ID, image tag, run-dir path, and a `Drop` impl that calls `docker stop --time 5 <id>`. Stash on `Runtime` as `ExecutorRuntime::Docker(session)`.

The job loop then runs as today; each `(sh ...)` dispatches through `docker exec` as described above.

Teardown is RAII: when `Run::execute` returns or unwinds, `Runtime` drops, `ContainerSession` drops, `docker stop` runs. Failures during teardown log via `tracing::error!` and are otherwise swallowed — `Drop` cannot return `Result`, and the right answer is to log and let orphan reconciliation (a follow-up task) handle anything that survives. `--rm` removes the container record after stop.

Three new error variants land alongside the existing `JobFailed`:

- `Error::WorkspaceMaterializationFailed { source }`
- `Error::ImageBuildFailed { source }`
- `Error::ContainerStartFailed { source }`

`JobFailed` stays distinct from these. Non-zero `sh` exits remain reported through `ShOutput.exit`, not raised as errors.

## State persistence

Docker mode writes `container.yml` alongside `meta.yml` and `times.yml` in the run directory:

```yaml
image_tag: quire-ci/example_repo:01934abc-...
container_id: 9f3b8a72c1d4...
build_started_at: 2026-05-04T16:20:01Z
build_finished_at: 2026-05-04T16:20:14Z
container_started_at: 2026-05-04T16:20:14Z
container_stopped_at: 2026-05-04T16:21:09Z
```

Writes are incremental and use the existing `write_yaml` (temp file + rename) helper:

- After `docker build`: `image_tag`, `build_started_at`, `build_finished_at`.
- After `docker run`: `container_id`, `container_started_at`.
- In `ContainerSession::Drop`, before `docker stop`: `container_stopped_at`.

Each write is atomic per file, not per field group. A crash between writes leaves a partially populated file — orphan reconciliation can use the recorded `container_id` to clean up.

Host mode skips the file. The presence of `container.yml` is the signal that a run used docker mode; tooling can branch on `path.exists()`.

Errors writing `container.yml` are non-fatal. Failing the run because a status file did not write would orphan a container the run could no longer track for cleanup.

## CLI integration

`quire ci run` gains `--executor host|docker` as a clap `ValueEnum`, defaulting to `host`. The flag flows through `commands::ci::run` into `Run::execute(pipeline, secrets, git_dir, executor)`.

The default stays `host` until docker mode has had enough mileage to flip. The `serve` path does not get the flag — push-triggered runs always use `Executor::Docker` once the migration completes. Until then, `serve` keeps host mode through the same `Executor` parameter.

## Tests

Three categories:

1. **Unit, no docker.** Existing `Run::execute` tests pass `Executor::Host` and continue working unchanged. New unit tests cover workspace materialization against the temp-git-repo fixture pattern already used by mirror tests.

2. **Integration, docker required.** A small set of tests exercising build → run → exec → stop end-to-end. Gated behind `#[ignore]` and a runtime check that `docker info` succeeds; if docker is unavailable, they skip with a `tracing::warn!`. Opt-in via `cargo test -- --ignored docker_`.

3. **Faked dispatcher.** For the host/docker dispatch logic in `Runtime::sh`, a unit test constructs `ExecutorRuntime::Docker(session)` with a synthetic container ID and a fake `docker` shim on `$PATH` that records its argv. This validates the exact `docker exec` command shape without a real daemon.

## Backlog references

- `lpmoszxo` — Per-run container lifecycle (this design).
- `knmkqkvx` — Route `sh` through docker exec into the run container (this design).
- Prior design: `docs/plans/2026-05-01-ci-execution-architecture-design.md`.

Follow-ups to file alongside this work:

- `(ci.image "name")` declared-image resolution.
- Default base image when neither `(ci.image)` nor `.quire/Dockerfile` exists.
- Prune old `quire-ci/*` images and old workspace directories under `$XDG_CACHE_HOME/quire/runs/`.
- Reconcile container orphans on quire startup — cross-reference `docker ps` with `container.yml` files in `active/`, force-remove and mark runs failed.
- Investigate `git worktree` / `jj workspace` in place of `git archive` for workspace materialization, sharing the VCS backend.
