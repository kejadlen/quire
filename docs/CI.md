# quire — CI design

How CI works in quire. Slots alongside PLAN.md; will likely fold in once the open questions settle. For the run/job state machines and what each state means in the database, see [CI-STATE.md](./CI-STATE.md).

## Shape

The runner lives **in-process with `quire serve`**, as a long-lived tokio task in the same binary. It owns a queue of pending runs (in-memory, reconstructed from disk on startup), watches it for new entries, materializes a workspace per run, starts a per-run container with the pipeline's declared image, evaluates `.quire/ci.fnl` in the host process, and tunnels each `(sh ...)` call from each job into the run's container via `docker exec`. The container is **per-run** — one started at run start, torn down at run end.

The runner itself is not a container. It's a tokio task. The thing the runner *spawns* is the run's sandbox container.

```
quire (one process)
  ├── HTTP server (quire serve)
  ├── ci-runner task
  │     ├── run #1: <sandbox> rust:1.75 ...    (per-run)
  │     ├── run #2: <sandbox> python:3.12 ...  (per-run)
  │     └── ...
  └── (shared state: run queue, log broadcasts)
```

Not the long-lived-per-image runner pool that GitHub Actions and GitLab use. That model amortizes startup at the cost of hermeticity — run N+1 inherits whatever run N left behind in the filesystem, which becomes a permanent class of "fails after the previous run" debugging. The speedup mostly comes from cache reuse, which is achievable with bind-mounted cache directories without taking on the statefulness debt. Personal forge doing dozens of runs/week, not thousands/day.

Per-run (vs per-job) is the simplest granularity for v1: one container start per run, jobs share workspace and toolchain caches naturally, and multi-job (when it lands) becomes concurrent `docker exec` into the same container. Per-job container differentiation can be added later if pipelines actually need it.

The runner doesn't get its own process because **it doesn't execute user code in its address space**. The runner reads files, builds a `docker run` argv to start the per-run container, then issues `docker exec` calls for each `(sh ...)` from each job, streams stdout/stderr from each exec into per-job log files, captures exit codes, records container ID for cancellation. None of these steps run user code in-process. A bug in `cargo test` can't crash the runner because it's running in a different container with its own kernel namespace. Process isolation between web and runner would buy nothing here — the docker boundary is doing that work. Don't pay twice for it.

Within the host process, `(sh ...)` is the only sanctioned host-effect primitive in the Lua VM. See "Sandbox the in-process VM" below — the compile-then-execute split removes `io`/`os`/`debug` from the execute VM so a buggy or hostile ci.fnl can't bypass the chokepoint.

## Communication: SQLite as state of record, channels as optimization

Run records in SQLite are the **durable truth** once written. The hook is a thin transport: it sends a push event over a Unix socket to `quire serve`, which is the sole writer of run records.

| Component | Reads from | Writes to | In-memory comms |
| --- | --- | --- | --- |
| Hook (`post-receive`) | — | — | push event → `quire serve` socket listener |
| Runner (in-process with `quire serve`) | SQLite on startup | SQLite, `jobs/*/`, logs | wakeup from listener (Notify); broadcast logs → web |
| Web (`quire serve`) | SQLite on demand | — | subscribe to log broadcasts |

The listener task (also inside `quire serve`) bridges the hook process boundary to the in-process runner. It binds `/var/quire/server.sock` on startup, parses incoming events, inserts the initial run row, and signals the runner. The wakeup signal carries no payload — the runner queries SQLite for the next pending run, so missed or duplicated wakes are idempotent.

On startup, the runner reconciles orphans: any `active` row whose container is no longer running gets marked `failed`. Crash resilience covers `quire serve` restart: any run row committed before the crash gets picked up.

**v1 limitation: zero-loss-on-server-down is not provided.** If `quire serve` is down at push time, the hook's socket connect fails, the pusher sees a stderr warning, and no run is created. The push itself remains accepted by git (post-receive runs after acceptance). The v1 mitigation is "run `quire serve` under a supervisor that restarts it"; a hook fallback that inserts directly into SQLite when the socket is unreachable is a deferred follow-up if this ever bites in practice.

The "we could one day extract the runner into its own process" door stays open: the SQLite schema doesn't change, the listener-to-runner wakeup becomes a Unix socket. Not building it now.

## Storage: SQLite

Run state, job state, and the run queue live in a single SQLite database at `<quire-data-root>/quire.db`. The database is the primary store for all run lifecycle data. The filesystem holds per-run workspaces and per-job log files only.

The database is project-scoped, not CI-scoped, even though the only tables today are CI tables. Future tables (config snapshots, hook event audit, etc.) live in the same file.

Migrations are SQL files under `migrations/` at the project root, embedded into the binary via `include_str!`. `rusqlite_migration` tracks `PRAGMA user_version` and applies missing migrations transactionally. Each future schema change adds a new file (`0002_*.sql`, `0003_*.sql`, …) and a corresponding `M::up` entry. Files are append-only — never edit a migration that has already shipped.

SQLite is the queue: `quire serve` finds the next pending run with a `SELECT`, not by scanning directories. The wakeup signal stays an in-process `tokio::sync::Notify` — used only to nudge the runner, not to carry data.

## Concurrency: max one run at a time

**One run executes at a time across the entire forge.** Job 2 of repo A waits for job 1 of repo B to finish.

Implications:

* Cache contention disappears entirely — no two jobs ever touch the same cache dir simultaneously.
* Resource limits are trivial: the box is dedicated to whatever's running. No `--cpus`/`--memory` math, no oversubscription.
* Queueing is FIFO from `runs/pending/`. No fairness story needed.

The cost is latency under load: push to repo A while a 5-minute build of repo B is running, and you wait. For personal scale this is almost never the experience. The escape valve is documented and small: add a `max_concurrent_runs` config knob and a per-repo cache file lock; spawn N runner tasks instead of 1. The queue, supersede logic, and on-disk schema don't change.

Within a run, **jobs form a DAG** (see next section), but the executor schedules them serially in topological order. Same constraint, same escape valve: the executor changes from "pick one ready job" to "pick up to N ready jobs"; the spec doesn't change.

### Supersede semantics

When a new push arrives for a ref that already has work in flight or queued for the same `(repo, ref)`:

* **Queued, not yet started:** new push replaces the queued one. Old run marked `superseded`. If you pushed twice in 30 seconds, you almost certainly only care about the second result.
* **Currently running:** kill the in-flight run container (`docker kill <id>`), mark the run `superseded`, enqueue the new one.
* **Different ref of same repo:** unaffected. Pushing to `feature-branch` should not kill a running build of `main`.

Cheap to get right *if* the run record stores the ref it's building from the start, and queue lookups are "any pending or active runs for `<repo>:<ref>`?" Both are one-line conditions.

## The job DAG

Jobs declare dependencies via `:needs`. Missing `:needs` means no dependencies — ready immediately. Failure of a job marks all transitive dependents as `skipped`, unless the failing job has `:allow-failure true` (in which case dependents proceed normally).

```
{:jobs
 [{:id "setup"
   :image "rust:1.75"
   :run "rustup component add clippy rustfmt"}

  {:id "lint"
   :image "rust:1.75"
   :needs ["setup"]
   :allow-failure true
   :run "cargo clippy -- -D warnings"}

  {:id "test"
   :image "rust:1.75"
   :needs ["setup"]
   :run "cargo test"}

  {:id "deploy"
   :image "alpine"
   :needs ["test"]
   :run "scp target/release/quire host:/usr/local/bin/"}]}
```

With max-concurrency 1, executor topo-sorts and picks one ready job at a time (FIFO among ready jobs = spec order). `lint` and `test` are both ready after `setup`; lint runs first, then test, then deploy. If `setup` fails, all three skip.

Schema decisions baked in:

* `:needs` is `needs-all` (job runs only when *all* listed jobs succeed). `needs-any` is a real but rare want; the schema can grow `:needs-any` later without breaking existing specs.
* Job ids are arbitrary non-empty strings. Cycle detection at parse time via Kahn's algorithm — fails closed, error message names the cycle.
* `:allow-failure` exists from v1. Without it, the only way to express "lint can fail and we still want to deploy" is to remove the dependency, which loses the ordering signal.

## Fennel evaluation

`.quire/ci.fnl` is **executed**, not parsed. The Fennel program runs to completion and produces a value — a table of jobs with `:needs` references. The runner schedules against the resulting structure. Eval happens once at run start; the DAG is static after eval returns.

Code, not data, means matrix builds, helpers, and conditionals fall out for free without dedicated schema features:

```
(local rust-versions [:1.75 :1.76 :stable])

{:jobs
 (collect [_ v (ipairs rust-versions)]
   {:id (.. "test-" v)
    :image (.. "rust:" v)
    :needs ["setup"]
    :run "cargo test"})}
```

### Eval runs in-process; the execute VM is sandboxed

Eval happens inside `quire serve`, in the same Lua/Fennel host that loads `config.fnl`. No subprocess, no wallclock cap, no memory limit. Every `ci.fnl` is code the operator wrote; the untrusted-code threat model that would justify external isolation doesn't exist.

A separate concern is in-process VM hardening: keeping a buggy or careless ci.fnl from bypassing the `(sh ...)` chokepoint by reaching for `os.execute` or `io.open` directly. The plan is a compile-then-execute VM split — the compile VM runs Lua 5.4 with full `debug` (Fennel's macroexpand and traceback need it); the execute VM is `Lua::new()` with `io`/`os`/`debug` removed and only `{sh, secret, jobs, string, table, math}` exposed. This makes `sh` the documented chokepoint and the JSONL persistence path unbypassable. See backlog `lsqluktu`. A subsequent task (`rzsonvsx`) layers Luau on the execute VM for bytecode-load validation and a tighter `debug` API as defense in depth.

The cost of in-process eval remains: a `ci.fnl` with an infinite loop or runaway allocation (`string.rep "x" 2^30`) can hang or OOM the server. Mitigation is "don't write that"; for the personal-forge case this is acceptable.

### Sandboxed eval — opt-in, future

The day `quire` runs `ci.fnl` written by someone other than the operator (a guest contributor, an automated pipeline pulling third-party templates, etc.) the in-process model stops being safe. The opt-in path is **bubblewrap**: same eval, same Fennel host, but invoked inside a bwrap sandbox that constrains filesystem access (workspace + the Fennel stdlib only), denies network, dies with the parent, and runs under a wallclock + memory cap.

Not built. Not designed in detail. The commitment is just: when sandboxing becomes necessary, it's a per-repo opt-in flag (`{:ci {:sandbox :bwrap}}` or similar), not a global default change. The default stays "in-process, unsandboxed."

The reason this is the chosen path rather than "subprocess + rlimit, no bwrap" — which also gets crash isolation and resource caps — is that the opt-in case *is* the untrusted-code case, and untrusted code wants filesystem and network isolation too. Bwrap covers all four (wallclock, memory, filesystem, network); subprocess+rlimit covers only the first two. The bwrap primitive is in the codebase already (the README commits to it), so reaching for the same primitive when it's needed is the simpler story.

## Run lifecycle

1. **`post-receive` hook** sends a push event (one JSON line: `{type, repo, pushed_at, refs: [{ref, old_sha, new_sha}, ...]}`) over `/var/quire/server.sock` and exits. The listener task in `quire serve` parses the event, allocates a run-id per ref, inserts a row into `runs` in `pending` state, and signals the runner. No CI work runs in the hook itself.
2. **Runner picks up** the entry from the queue. Single `UPDATE runs SET state = 'active'` in SQLite.
3. **Materialize workspace.** `git --git-dir=repos/foo.git archive <sha> | tar -x -C workspace/`. No worktree, no checkout state on the bare repo. Workspace is throwaway; deleted at end of run.
4. **Evaluate `.quire/ci.fnl`** in the host process (see above). Pipeline image is read from the `(ci.image ...)` registration; jobs are registered via `(ci.job ...)`; the run-fns are not yet invoked.
5. **Start the run container.** `docker run -d --rm --mount type=bind,src=<run-dir>,dst=/work -w /work <image> sleep infinity`. Container ID written to the `runs` row. The run's container hosts every `(sh ...)` call from every job in the run.
6. **Per ready job:** invoke its run-fn in topological order. Each `(sh ...)` call inside the run-fn issues `docker exec` (no TTY) into the run container, captures stdout/stderr and exit code, and returns `{exit, stdout, stderr, cmd}` to Lua.
7. **Tear down the run container.** `docker stop` + `docker rm`. Even on error paths — no orphaned containers if a run-fn errors. `container_stopped_at_ms` written to the `runs` row.
8. **Aggregate.** Write final status via `UPDATE runs SET state = 'complete'` (or `'failed'`). Per-`(sh ...)` log files are written to `jobs/<job-id>/sh-<n>.log` on disk before the final transition.

## Run record schema

```
quire.db
  runs table: id, repo, ref_name, sha, pushed_at_ms, state, failure_kind,
              queued_at_ms, started_at_ms, finished_at_ms, container_id,
              image_tag, build_started_at_ms, build_finished_at_ms,
              container_started_at_ms, container_stopped_at_ms, workspace_path
  jobs table:  run_id, job_id, state, exit_code, started_at_ms, finished_at_ms

runs/<repo>/<run-id>/
  workspace/               # materialized checkout
  jobs/
    <job-id>/
      sh-<n>.log           # one CRI-format log file per (sh ...) call
```

Per-`(sh ...)` log files are written in [k8s CRI log
format](https://github.com/kubernetes/cri-api) — each line is
`<RFC3339 ts> <stream> F <content>`, where stream is `stdout` or
`stderr` and `F` marks a full line. One file per `(sh ...)` call
keeps writes append-only and lets the web UI stream a single sh's
output without parsing a multiplexed stream.

## Sandbox backend — the real fork in the road

Polyglot toolchains rule out "just bind-mount host `/`" — that path requires every toolchain on the host. So the sandbox is either Docker images or OCI images extracted to disk and run under bubblewrap. Both work. They imply different overall architectures.

### Path A: Docker (DooD)

`docker run -d --rm --mount type=bind,src=<ws>,dst=/work -w /work --cpus=N --memory=M <image> sleep infinity` per run, then `docker exec` (no TTY) for each `(sh ...)` call from every job in the run. Shared image cache, well-trodden, every CI system on earth has done this.

Quire stays containerized. The container talks to the host's docker daemon via bind-mounted `/var/run/docker.sock`. Anyone with that socket effectively has root on the host — fine here since quire and the operator account already share the box.

The gotcha that will bite once and never again: when quire calls `docker run -v /var/quire/runs/foo/123/workspace:/workspace`, the host path is resolved by the *host* daemon, not interpreted from inside the quire container. So `/var/quire` must be at the *same path* on host and inside the quire container. Get this wrong in compose and you'll spend an hour on empty workspaces.

Cost: socket mount, the path-pinning rule, daemon-talking-to-daemon, quire stays containerized.

### Path B: OCI + bubblewrap

`skopeo copy docker://rust:1.75-slim oci:images/rust-1.75:latest`, then `umoci unpack`, then bwrap binds the rootfs and runs the run container. `docker exec`'s role is filled by spawning into the persistent bwrap namespace (or relaunching bwrap per `(sh ...)` if persistent processes prove painful — measure):

```
bwrap --bind rootfs/rust-1.75 / \
      --bind <workspace> /workspace --chdir /workspace \
      --bind <cache>/cargo /cache/cargo \
      --setenv CARGO_HOME /cache/cargo \
      --proc /proc --dev /dev --tmpfs /tmp \
      --unshare-pid --unshare-ipc --die-with-parent \
      sh -c 'cargo test'
```

Full Docker Hub image catalog. No daemon, no socket, no privilege, no DinD/DooD question. The cascade: quire becomes a systemd unit on the host; one process tree; the `/var/quire` path-pinning rule becomes irrelevant because nothing crosses a container boundary.

Costs that need real work:

* **Writable rootfs.** Most images expect to write outside the workspace (apt, scripts dropping files in `/etc`). Bwrap's `--overlay-src` gives a writable union with a throwaway upper layer. ~30 lines, but mandatory by the second image you try.
* **Image refresh.** No auto-pull on tag updates. Either explicit `quire ci pull` or digest-check before each run.
* **Resource limits.** No `--cpus`/`--memory`. Wrap with `systemd-run --user --scope -p MemoryMax=2G -p CPUQuota=200% bwrap ...` or write the bwrap PID into a cgroup directly.
* **OCI config.** Images carry `entrypoint`/`cmd`/`USER` in their config; bwrap doesn't read it. Parse the JSON yourself if you want to honor it. For CI it barely matters since you're overriding the command anyway.

Roughly 200-400 lines of Rust beyond the bind-host case, mostly shelling to `skopeo`/`umoci` and assembling the bwrap argv.

The bwrap primitive used here (running a job in a sandbox) is the same one as the opt-in eval sandbox. Building Path B for jobs and the eval opt-in for `ci.fnl` would share most of their plumbing.

### Recommendation

**DooD for v1, OCI+bwrap as a known migration path.**

* DooD gets CI working in a week. Polyglot is free.
* The runner is one tokio task in one binary. Swapping its backend is a contained change. The Fennel job spec doesn't care which backend ran it.
* Once the system has been used enough to know what's actually needed from it, the OCI+bwrap migration removes the last reason for quire to be containerized at all — which is the more on-brand endpoint given the rest of the design.

If the impulse is to skip straight to OCI+bwrap on aesthetic grounds: defensible, but you're paying ~2 weeks of sandbox plumbing before any CI runs at all. The intermediate state of "DooD works, here's what I actually want from it" is worth a lot.

## Caching

Per-repo named volume (DooD) or bind-mounted directory (bwrap) at `/cache`, with `CARGO_HOME=/cache/cargo`, `npm_config_cache=/cache/npm`, etc. set via env. Cache lives on the same volume as everything else under `/var/quire/cache/<repo>/`. Same model in both backends; just plumbed differently.

Punt on cache invalidation until it actually annoys. "Delete the cache dir" is a fine v1 escape hatch.

## Open questions

* **Fennel stdlib surface.** What does the Fennel eval expose? At minimum: env access (`(env :GITHUB_TOKEN)`, scoped to repo secrets), table-building for jobs, maybe a `matrix` helper. Bigger question: does eval get to read files from the workspace (`(read-file "Cargo.toml")` to decide what jobs to register)? "Yes" is the thin end of the dynamic-jobs wedge; "no" keeps the model strict.
* **Image pre-warming.** First run of any image pulls hundreds of MB. Want both implicit pull-on-demand and an explicit `quire ci pull <image>` to warm before pushing.
* **Log streaming UX.** SSE tailing the log file works for the web UI, but the broadcast-channel-vs-file-tail interaction has subtleties around "client connects mid-job, wants backlog + live."
* **Image GC.** Host accumulates layers. Weekly `docker image prune` via host cron is the dumb correct answer for DooD; OCI+bwrap needs a `quire ci gc` that walks `images/` and `rootfs/` against last-used timestamps.
* **Services / sidecars.** Some jobs want postgres or redis alongside. The shape is "bring up sidecar, run job against it, tear down." Adds a small orchestration layer. Not v1.
* **Secrets.** CI jobs that need API tokens. Probably env-injected from `config.fnl`, scoped per-repo. Worth designing the surface area before the first job needs one.
* **Cycle detection error UX.** Where do parse errors surface — does the push fail (post-receive returns nonzero) or does the run start and immediately error? Probably the latter, since hooks should be fast and CI errors belong in CI history.
* **Sandbox opt-in surface.** When the bwrap eval/job sandbox lands as an opt-in, what's the per-repo flag's exact shape? Probably one boolean covering both eval and jobs (you don't want one without the other if you don't trust the source), but the exact key in per-repo config wants designing alongside the rest of the per-repo schema.

## Locked-in decisions

* **Runner is in-process** with `quire serve` as a tokio task; not a separate process. Filesystem is the state of record; channels are the wakeup optimization.
* **SQLite is the primary store for run and job state.** Migrations under `migrations/`, embedded into the binary. The filesystem holds workspaces and per-job log files only.
* **Per-run container**, not per-job and not long-lived runners. One `docker run` at run start, `docker exec` per `(sh ...)` call from each job, `docker stop` at run end. Per-job container differentiation is a deferred extension.
* **`(sh ...)` is the only host-effect primitive in the Lua VM.** No `(container ...)` primitive. The execute VM is hardened (no `io`/`os`/`debug`) so `sh` becomes the documented chokepoint — every effect is auditable, persistable, redactable in one place.
* **Pipeline-level image declaration via `(ci.image ...)`.** Single image per pipeline; per-job override deferred until pipelines actually need heterogeneity.
* **DooD for v1**; OCI+bwrap as planned migration path.
* **Workspace materialized via `git archive`**, not worktree.
* **Max concurrency 1** across the whole forge. Escape valve is `max_concurrent_runs` config + per-repo cache file lock; not building it now.
* **Jobs are a DAG** with `:needs` (needs-all). Executor schedules serially in topological order under max-concurrency 1; lifting that constraint changes the executor, not the spec.
* **`:allow-failure`** flag exists from v1.
* **Supersede on same `(repo, ref)`**: replace queued, kill running.
* **`.quire/ci.fnl` is executed**, returns the DAG.
* **Eval runs in-process; the execute VM is sandboxed.** Compile VM keeps full Lua 5.4 (Fennel macroexpand/traceback need `debug`); execute VM removes `io`/`os`/`debug` and exposes only `{sh, secret, jobs, string, table, math}`. Trusted-code threat model — no external isolation. Bwrap-based eval sandbox stays available as an opt-in for the day quire runs `ci.fnl` from someone other than the operator. Not built; not v1.
* **Hook is a transport, not a writer.** `post-receive` sends a push event over `/var/quire/server.sock`; `quire serve` writes the run record. Hook never touches `runs/`. Tradeoff: zero-loss-on-server-down is dropped in v1 (push lands but no run is created). Fallback to direct disk write is a deferred follow-up.
* **Caches** are bind-mounted directories under `/var/quire/cache/<repo>/`.
