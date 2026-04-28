# quire — CI design

How CI works in quire. Slots alongside PLAN.md; will likely fold in once the open questions settle.

## Shape

The runner lives **in-process with `quire serve`**, as a long-lived tokio task in the same binary. It owns a queue of pending runs (in-memory, reconstructed from disk on startup), watches it for new entries, materializes a workspace per run, evaluates `.quire/ci.fnl`, and shells out to execute each job. Jobs are **ephemeral** — fresh sandbox per job, torn down on exit.

The runner itself is not a container. It's a tokio task. The thing the runner *spawns* is the sandbox.

```
quire (one process)
  ├── HTTP server (quire serve)
  ├── ci-runner task
  │     ├── run #1: <sandbox> rust:1.75 ...    (ephemeral)
  │     ├── run #2: <sandbox> python:3.12 ...  (ephemeral)
  │     └── ...
  └── (shared state: run queue, log broadcasts)
```

Not the long-lived-per-image runner pool that GitHub Actions and GitLab use. That model amortizes startup at the cost of hermeticity — job N+1 inherits whatever job N left behind in the filesystem, which becomes a permanent class of "fails after the previous job" debugging. The speedup mostly comes from cache reuse, which is achievable with bind-mounted cache directories without taking on the statefulness debt. Personal forge doing dozens of runs/week, not thousands/day; container-per-job is strictly better here.

The runner doesn't get its own process because **it doesn't execute user code in its address space**. With container-per-job, the runner reads files, builds a `docker run` argv, spawns it, copies stdout to a log file, reads exit code. None of those steps run user code in-process. A bug in `cargo test` can't crash the runner because it's running in a different container with its own kernel namespace. Process isolation between web and runner would buy nothing here — the docker boundary is doing that work. Don't pay twice for it.

## Communication: filesystem as state of record, channels as optimization

Run records on disk are the **durable truth** once written. The hook is a thin transport: it sends a push event over a Unix socket to `quire serve`, which is the sole writer of run records on disk.

| Component | Reads from disk | Writes to disk | In-memory comms |
|---|---|---|---|
| Hook (`post-receive`) | — | — | push event → `quire serve` socket listener |
| Runner (in-process with `quire serve`) | run records on startup | `meta.json`, `state.json`, `jobs/*/`, logs | wakeup from listener (mpsc); broadcast logs → web |
| Web (`quire serve`) | run records on demand | — | subscribe to log broadcasts |

The listener task (also inside `quire serve`) bridges the hook process boundary to the in-process runner. It binds `/var/quire/server.sock` on startup, parses incoming events, writes the initial run record, and signals the runner via mpsc. The wakeup signal carries no payload — the runner re-derives state by scanning `pending/`, so missed or duplicated wakes are idempotent.

On startup, the runner walks `runs/pending/` and `runs/active/`, reconstructs the queue, and reconciles orphans (any `active/` entry whose container is no longer running gets marked failed). Crash resilience covers `quire serve` restart: any run record committed before the crash gets picked up.

**v1 limitation: zero-loss-on-server-down is not provided.** If `quire serve` is down at push time, the hook's socket connect fails, the pusher sees a stderr warning, and no run is created. The push itself remains accepted by git (post-receive runs after acceptance). The v1 mitigation is "run `quire serve` under a supervisor that restarts it"; a hook fallback that writes `meta.json` directly when the socket is unreachable is a deferred follow-up if this ever bites in practice.

The "we could one day extract the runner into its own process" door stays open: the on-disk schema doesn't change, the listener-to-runner mpsc becomes a Unix socket. Not building it now.

## Storage: no database

No SQLite in v1. Run records, job state, logs, and the queue all live as files under `/var/quire/runs/`. None of the queries the v1 web UI wants are slow at this scale; the in-memory queue handles low-latency enqueue/dequeue.

**The commitment, written down so it doesn't drift:** if SQLite ever earns its keep — most likely trigger is FTS5 over logs — it enters as a **secondary index over the filesystem**, never as a primary store. Files remain canonical. The database is rebuildable: `quire reindex` walks `runs/` and repopulates. If the database is corrupted or lost, recovery is mechanical. The rule: `rm /var/quire/quire.db && quire reindex` returns the system to a working state. If that ever stops being true, the database has crossed a line it shouldn't have.

This is the principle that prevents drift. The temptation to migrate state into SQLite ("just this one thing, it's so much easier") is constant once it exists; without the rule written down, the second time you reach for SQLite you'll have forgotten why you originally said no.

## Concurrency: max one run at a time

**One run executes at a time across the entire forge.** Job 2 of repo A waits for job 1 of repo B to finish.

Implications:
- Cache contention disappears entirely — no two jobs ever touch the same cache dir simultaneously.
- Resource limits are trivial: the box is dedicated to whatever's running. No `--cpus`/`--memory` math, no oversubscription.
- Queueing is FIFO from `runs/pending/`. No fairness story needed.

The cost is latency under load: push to repo A while a 5-minute build of repo B is running, and you wait. For personal scale this is almost never the experience. The escape valve is documented and small: add a `max_concurrent_runs` config knob and a per-repo cache file lock; spawn N runner tasks instead of 1. The queue, supersede logic, and on-disk schema don't change.

Within a run, **jobs form a DAG** (see next section), but the executor schedules them serially in topological order. Same constraint, same escape valve: the executor changes from "pick one ready job" to "pick up to N ready jobs"; the spec doesn't change.

### Supersede semantics

When a new push arrives for a ref that already has work in flight or queued for the same `(repo, ref)`:

- **Queued, not yet started:** new push replaces the queued one. Old run marked `superseded`. If you pushed twice in 30 seconds, you almost certainly only care about the second result.
- **Currently running:** kill the in-flight sandbox (`docker kill <id>`), mark the run `superseded`, enqueue the new one.
- **Different ref of same repo:** unaffected. Pushing to `feature-branch` should not kill a running build of `main`.

Cheap to get right *if* the run record stores the ref it's building from the start, and queue lookups are "any pending or active runs for `<repo>:<ref>`?" Both are one-line conditions.

## The job DAG

Jobs declare dependencies via `:needs`. Missing `:needs` means no dependencies — ready immediately. Failure of a job marks all transitive dependents as `skipped`, unless the failing job has `:allow-failure true` (in which case dependents proceed normally).

```fennel
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
- `:needs` is `needs-all` (job runs only when *all* listed jobs succeed). `needs-any` is a real but rare want; the schema can grow `:needs-any` later without breaking existing specs.
- Job ids are arbitrary non-empty strings. Cycle detection at parse time via Kahn's algorithm — fails closed, error message names the cycle.
- `:allow-failure` exists from v1. Without it, the only way to express "lint can fail and we still want to deploy" is to remove the dependency, which loses the ordering signal.

## Fennel evaluation

`.quire/ci.fnl` is **executed**, not parsed. The Fennel program runs to completion and produces a value — a table of jobs with `:needs` references. The runner schedules against the resulting structure. Eval happens once at run start; the DAG is static after eval returns.

Code, not data, means matrix builds, helpers, and conditionals fall out for free without dedicated schema features:

```fennel
(local rust-versions [:1.75 :1.76 :stable])

{:jobs
 (collect [_ v (ipairs rust-versions)]
   {:id (.. "test-" v)
    :image (.. "rust:" v)
    :needs ["setup"]
    :run "cargo test"})}
```

### Eval is sandboxed by subprocess + timeouts

> **v0 status:** initial implementation evaluates `ci.fnl` in-process inside `quire serve`, with no wallclock or memory cap. A buggy or hostile `ci.fnl` can hang or OOM the server. Subprocess + the limits described below are the eventual target; they land when ci.fnl runaway becomes a real liability.

Eval runs as a subprocess: `quire eval-ci-config <workspace>`. The child reads the file, runs the Fennel evaluator, serializes the result table to JSON on stdout, exits. Runner reads stdout, kills the child after the deadline:

```rust
let mut cmd = Command::new(env::current_exe()?);
cmd.args(["eval-ci-config", "--workspace"]).arg(workspace)
   .stdout(Stdio::piped()).stderr(Stdio::piped());

unsafe {
    cmd.pre_exec(|| {
        let lim = libc::rlimit {
            rlim_cur: 512 * 1024 * 1024,
            rlim_max: 512 * 1024 * 1024,
        };
        libc::setrlimit(libc::RLIMIT_AS, &lim);
        Ok(())
    });
}

let child = cmd.spawn()?;
let output = timeout(Duration::from_secs(10), child.wait_with_output()).await
    .map_err(|_| anyhow!("ci.fnl evaluation exceeded 10s deadline"))??;
```

Defaults, both global config in `config.fnl`:
- **10 second wallclock.** If `ci.fnl` needs longer than 10s to *decide what jobs to run*, something's wrong — that's design-time work, not runtime work.
- **512 MB memory.** Same logic; eval shouldn't be doing heavy computation.

Per-repo overrides intentionally not supported: the only reason a repo would need more is that the repo is doing something it shouldn't.

This isn't bwrap because it doesn't need to be. The threat model is "did I accidentally write a `ci.fnl` that infinite-loops or eats the disk," not "is someone attacking me." Wallclock kill works regardless of what the eval is doing in C; OOM kills the child, not the runner; crash isolation is free. Filesystem and network isolation that bwrap would add buy nothing — eval is reading files in a workspace it has legitimate access to anyway.

The in-process Lua `debug.sethook` approach is clever but has a blind spot inside C functions (`string.rep("x", 2^30)` returns instantly from Lua's perspective, the hook never fires during it, the runner OOMs). Subprocess + kill-on-timeout is boring and correct.

## Run lifecycle

1. **`post-receive` hook** sends a push event (one JSON line: `{type, repo, pushed_at, refs: [{ref, old_sha, new_sha}, ...]}`) over `/var/quire/server.sock` and exits. The listener task in `quire serve` parses the event, allocates a run-id per ref, writes `runs/<repo>/<run-id>/{meta.json, state.json}`, and signals the runner via mpsc. No CI work runs in the hook itself.
2. **Runner picks up** the entry from the queue. Atomic rename `pending/<id>` → `active/<id>` for state-machine clarity.
3. **Materialize workspace.** `git --git-dir=repos/foo.git archive <sha> | tar -x -C workspace/`. No worktree, no checkout state on the bare repo. Workspace is throwaway; deleted at end of run.
4. **Evaluate `.quire/ci.fnl`** via subprocess (see above). Result is the job DAG.
5. **Per ready job:** spawn the sandbox with workspace + caches mounted, stream stdout/stderr to `jobs/<job-id>/log` (and broadcast for live web tailing), capture exit code, record container ID for cancellation.
6. **Aggregate.** Write final status to the run directory. Move `active/<id>` → `complete/<id>` (or `failed/<id>`).

## Run record schema

```
runs/<repo>/<run-id>/
  meta.json        # immutable: sha, ref, pusher, pushed_at
  state.json       # mutable: status, started_at, finished_at, runner_pid, sandbox_id
  jobs/
    <job-id>/
      spec.json    # immutable: image, cmds, env, needs (extracted from ci.fnl)
      state.json   # mutable: status, started_at, finished_at, exit_code, sandbox_id
      log          # append-only stdout+stderr
  cancel           # touch-file; runner checks before each job
```

Two principles fall out:
- **Immutable vs. mutable files are separate.** `meta.json` is written once and never touched. Readers (the web UI) can cache `meta.json` indefinitely and only re-read `state.json`.
- **Append-only logs.** Web UI tails the log file; runner appends; no coordination needed. Live tailing also goes through a `tokio::sync::broadcast` channel for sub-second latency, but the file is the source of truth.

## Sandbox backend — the real fork in the road

Polyglot toolchains rule out "just bind-mount host `/`" — that path requires every toolchain on the host. So the sandbox is either Docker images or OCI images extracted to disk and run under bubblewrap. Both work. They imply different overall architectures.

### Path A: Docker (DooD)

`docker run --rm -v <ws>:/workspace -w /workspace --cpus=N --memory=M <image> sh -c '<cmds>'` per job. Shared image cache, well-trodden, every CI system on earth has done this.

Quire stays containerized. The container talks to the host's docker daemon via bind-mounted `/var/run/docker.sock`. Anyone with that socket effectively has root on the host — fine here since quire and the operator account already share the box.

The gotcha that will bite once and never again: when quire calls `docker run -v /var/quire/runs/foo/123/workspace:/workspace`, the host path is resolved by the *host* daemon, not interpreted from inside the quire container. So `/var/quire` must be at the *same path* on host and inside the quire container. Get this wrong in compose and you'll spend an hour on empty workspaces.

Cost: socket mount, the path-pinning rule, daemon-talking-to-daemon, quire stays containerized.

### Path B: OCI + bubblewrap

`skopeo copy docker://rust:1.75-slim oci:images/rust-1.75:latest`, then `umoci unpack`, then bwrap binds the rootfs and runs the job:

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
- **Writable rootfs.** Most images expect to write outside the workspace (apt, scripts dropping files in `/etc`). Bwrap's `--overlay-src` gives a writable union with a throwaway upper layer. ~30 lines, but mandatory by the second image you try.
- **Image refresh.** No auto-pull on tag updates. Either explicit `quire ci pull` or digest-check before each run.
- **Resource limits.** No `--cpus`/`--memory`. Wrap with `systemd-run --user --scope -p MemoryMax=2G -p CPUQuota=200% bwrap ...` or write the bwrap PID into a cgroup directly.
- **OCI config.** Images carry `entrypoint`/`cmd`/`USER` in their config; bwrap doesn't read it. Parse the JSON yourself if you want to honor it. For CI it barely matters since you're overriding the command anyway.

Roughly 200-400 lines of Rust beyond the bind-host case, mostly shelling to `skopeo`/`umoci` and assembling the bwrap argv.

### Recommendation

**DooD for v1, OCI+bwrap as a known migration path.**

- DooD gets CI working in a week. Polyglot is free.
- The runner is one tokio task in one binary. Swapping its backend is a contained change. The Fennel job spec doesn't care which backend ran it.
- Once the system has been used enough to know what's actually needed from it, the OCI+bwrap migration removes the last reason for quire to be containerized at all — which is the more on-brand endpoint given the rest of the design.

If the impulse is to skip straight to OCI+bwrap on aesthetic grounds: defensible, but you're paying ~2 weeks of sandbox plumbing before any CI runs at all. The intermediate state of "DooD works, here's what I actually want from it" is worth a lot.

## Caching

Per-repo named volume (DooD) or bind-mounted directory (bwrap) at `/cache`, with `CARGO_HOME=/cache/cargo`, `npm_config_cache=/cache/npm`, etc. set via env. Cache lives on the same volume as everything else under `/var/quire/cache/<repo>/`. Same model in both backends; just plumbed differently.

Punt on cache invalidation until it actually annoys. "Delete the cache dir" is a fine v1 escape hatch.

## Open questions

- **Fennel stdlib surface.** What does `eval-ci-config` expose? At minimum: env access (`(env :GITHUB_TOKEN)`, scoped to repo secrets), table-building for jobs, maybe a `matrix` helper. Bigger question: does eval get to read files from the workspace (`(read-file "Cargo.toml")` to decide what jobs to register)? "Yes" is the thin end of the dynamic-jobs wedge; "no" keeps the model strict.
- **Image pre-warming.** First run of any image pulls hundreds of MB. Want both implicit pull-on-demand and an explicit `quire ci pull <image>` to warm before pushing.
- **Log streaming UX.** SSE tailing the log file works for the web UI, but the broadcast-channel-vs-file-tail interaction has subtleties around "client connects mid-job, wants backlog + live."
- **Image GC.** Host accumulates layers. Weekly `docker image prune` via host cron is the dumb correct answer for DooD; OCI+bwrap needs a `quire ci gc` that walks `images/` and `rootfs/` against last-used timestamps.
- **Services / sidecars.** Some jobs want postgres or redis alongside. The shape is "bring up sidecar, run job against it, tear down." Adds a small orchestration layer. Not v1.
- **Secrets.** CI jobs that need API tokens. Probably env-injected from `config.fnl`, scoped per-repo. Worth designing the surface area before the first job needs one.
- **Cycle detection error UX.** Where do parse errors surface — does the push fail (post-receive returns nonzero) or does the run start and immediately error? Probably the latter, since hooks should be fast and CI errors belong in CI history.

## Locked-in decisions

- **Runner is in-process** with `quire serve` as a tokio task; not a separate process. Filesystem is the state of record; channels are the wakeup optimization.
- **No SQLite in v1.** If it enters later, it's a secondary index over the filesystem, never primary. `rm quire.db && quire reindex` must always recover.
- **Container-per-job**, not long-lived runners.
- **DooD for v1**; OCI+bwrap as planned migration path.
- **Workspace materialized via `git archive`**, not worktree.
- **Max concurrency 1** across the whole forge. Escape valve is `max_concurrent_runs` config + per-repo cache file lock; not building it now.
- **Jobs are a DAG** with `:needs` (needs-all). Executor schedules serially in topological order under max-concurrency 1; lifting that constraint changes the executor, not the spec.
- **`:allow-failure`** flag exists from v1.
- **Supersede on same `(repo, ref)`**: replace queued, kill running.
- **`.quire/ci.fnl` is executed**, returns the DAG.
- **Eval runs as a subprocess** with 10s wallclock and 512 MB memory cap. Not bwrap. **v0 deviation:** initial implementation evaluates in-process; subprocess sandbox lands when ci.fnl runaway becomes a real liability.
- **Hook is a transport, not a writer.** `post-receive` sends a push event over `/var/quire/server.sock`; `quire serve` writes the run record. Hook never touches `runs/`. Tradeoff: zero-loss-on-server-down is dropped in v1 (push lands but no run is created). Fallback to direct disk write is a deferred follow-up.
- **Caches** are bind-mounted directories under `/var/quire/cache/<repo>/`.
