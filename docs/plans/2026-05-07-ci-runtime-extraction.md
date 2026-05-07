# CI runtime extraction

Quire-the-server currently fills two roles in CI: it orchestrates runs (schedules, manages container lifecycle, captures logs, stores results) *and* it evaluates user code (`.quire/ci.fnl`, including each job's `(sh …)` calls). Most of the operational pain in CI today traces back to that conflation: `(sh …)` reaching across the container boundary as a `docker exec`, the `/var/quire` path-alignment rule, secrets sitting in the orchestrator's address space, and local execution that doesn't quite match the server's. This plan separates the two roles.

## Principles

1. **The orchestrator does not execute user code.** Container boundaries exist to limit what user code can do. An orchestrator that evaluates user code in-process, even just to dispatch shell calls, has put itself on the wrong side of its own boundary. Once a run is underway the orchestrator observes (logs, exit codes) and controls lifecycle (start, stop, kill).
2. **The runtime travels with the code it evaluates.** Job code lives in the run container, so the runtime goes there. Config (`config.fnl`, `.quire/config.fnl`) is quire's own code (the user supplies it but quire trusts it), so the runtime stays in the server for that. One runtime, two delivery targets.
3. **Local and server execution are the same code path.** Whatever runs a job inside a run container also runs that job on a developer's laptop. There is no second implementation to drift.

Everything below is a consequence of those three.

## What the split looks like

| Role         | Binary    | Knows about                                   |
|--------------|-----------|-----------------------------------------------|
| Orchestrator | `quire`   | Runs, jobs, containers, logs, web, db, config |
| Runtime      | `quire-ci`| Lua/Fennel, `sh`, env, workspace              |

Both live in the same Cargo crate. The split is at the binary boundary: `quire` carries the server's heavy deps (axum, rusqlite, web), and `quire-ci` carries only the runtime modules. `quire-ci` is built statically against musl libc so it can run inside any pipeline image.

## Consequences

### Workspace shape

One crate, two bin targets, server-only deps gated behind a feature flag:

```
src/
  lib.rs                  # ci, fennel, runtime, config — shared modules
  bin/
    quire/main.rs         # required-features = ["server"]
    quire-ci/main.rs      # runtime entry points only
```

```toml
[features]
default = ["server"]
server = ["dep:axum", "dep:rusqlite", "dep:tower-http", ...]

[[bin]]
name = "quire"
required-features = ["server"]

[[bin]]
name = "quire-ci"
```

`cargo build --no-default-features --bin quire-ci --target x86_64-unknown-linux-musl` produces a static binary with only the runtime modules compiled. Capability separation between config and run-fn environments stays where it already is, in how the runtime binds names per evaluation context, not at the link layer.

### What `quire-ci` exposes

Two subcommands, both reachable as the same code path locally and inside a run container:

- `quire-ci eval --job <name> [--workspace <path>] [--ci-file <path>]` evaluates a single job's run-fn against a workspace. Stdout and stderr are raw streams. Exit code is the job result.
- `quire-ci config <path>` evaluates a config file and prints the result as JSON, for the orchestrator to share a single parser and for local debugging.

Env carries the dynamic context (secrets, run id, repo, ref). The orchestrator forwards env via `docker exec --env`; locally the developer sets it in their shell.

### Server ↔ runtime boundary

The server's runner stops evaluating Lua. Per ready job, it execs `quire-ci eval --job <name>` inside the run container. Stdout and stderr stream to the per-job log file, and the exit code becomes the job result. The "tunnel each `(sh …)` via `docker exec`" machinery goes away: `(sh …)` is now a local subprocess inside a container the server started but does not re-enter.

Discovery and config keep their in-process path on the server. The server already needs the Lua runtime for those (it's quire's own code, principle 1), and shelling out would mean re-parsing a file the server has cached.

### Distribution into run containers

`quire-ci` ships in the orchestrator image at `/usr/local/bin/quire-ci`. Per ready job, the orchestrator places it into the run container before exec'ing it. Three viable mechanisms:

1. **`docker cp`.** After `docker run`, the orchestrator runs `docker cp /usr/local/bin/quire-ci <id>:/usr/local/bin/quire-ci`. `docker cp` is implemented at the CLI: it tar-streams the local file to the daemon's container-archive endpoint, so the daemon never resolves the source path — no path-pinning gotcha, even though "local" here means inside the orchestrator container. Executable bit preserved. Cost: one binary copy per run start.
2. **Bind mount.** `--mount type=bind,src=/var/quire/bin/quire-ci,dst=/usr/local/bin/quire-ci,readonly`. Cleaner at runtime but inherits the path-pinning rule — the source path is host-resolved by the daemon, so the orchestrator must write the binary into a host-aligned path at startup.
3. **Base image.** A `quire/ci-runtime` users `FROM`. Cleanest at runtime but constrains pipeline images to extend a quire-supplied base, which we don't want as a hard requirement.

Plan starts with `docker cp`. The other two stay viable if its copy cost or interaction model causes friction.

### Local dev

`quire-ci eval --job <name>` against a checkout with a `.quire/ci.fnl` is the primary local CI flow — no docker, no server, no SSH dispatch. It hits the same code that the orchestrator exec's inside a container, so "passes locally, fails on server" stops being a category of bug.

A higher-fidelity `quire-ci run` that spawns a container the way the orchestrator does is a follow-up; nothing in this design forecloses it.

### Migration

Staged so neither half breaks during the transition:

1. Land workspace shape and the `server` feature flag. Build pipeline runs both targets. `quire-ci` exists but isn't wired into runs.
2. Implement `quire-ci eval` against the runtime modules. Local invocation works; server still uses in-process evaluation.
3. Add the `docker cp` step and the new dispatch path to the runner, gated behind an extension of the `--executor host|docker` flag from `lpmoszxo`. Both paths run side-by-side under different executor variants.
4. Bake. Once confidence is there, flip the default and remove the in-process evaluator from the server.

### Tests

- Unit tests for `quire-ci eval` co-locate with the runtime modules — same crate, both bins exercise them.
- Build pipeline runs `cargo build --features server --bin quire` and `cargo build --no-default-features --bin quire-ci --target x86_64-unknown-linux-musl`. A stray `use axum::...` in shared code fails the second build immediately.
- Binary-size assertion on `quire-ci` (target: under 10MB stripped). Catches feature drift early.
- Integration tests for the dispatch path use the fake-docker shim from `lpmoszxo` — assert the `docker cp` and `docker exec quire-ci eval` argv sequence without a real daemon.

## Out of scope

- Removing Lua from the server (principle 1 says config is quire's own code, so the runtime stays).
- AOT-compiling `.fnl` to bytecode before shipping. Worth revisiting if startup latency becomes a complaint.
- Finer-grained cancellation than `docker kill` of the run container.
- A `quire/ci-runtime` base image distribution.

## Backlog references

This design extends `docs/plans/2026-05-04-per-run-container-lifecycle-design.md`. The per-run container lifecycle and `--executor` mechanics from that work stay; what's evaluated *inside* the run container changes.

Follow-ups to file:

- AOT-compile `.fnl` to bytecode in the orchestrator before dispatch.
- `quire/ci-runtime` base image as an alternative distribution channel.
- `quire-ci run` for local high-fidelity reproduction (spawns the container).
- Structured wire protocol between runner and `quire-ci` (step boundaries, progress, partial failures).
