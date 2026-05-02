# CI execution architecture

Captures the pivot from "run-fn returns a `(container {...})` spec" to "per-run container, `sh` tunnels via `docker exec`," and the surrounding decisions that fall out of it.

## Context

Today, ci.fnl evaluates in-process inside `quire serve`, and `(sh ...)` shells out on the host. There is no container; `sh` runs commands as the quire user. A buggy or hostile ci.fnl can `os.execute("rm -rf ~")` and bypass everything — the Lua VM has full standard libraries.

The next iceboxed CI story (`uutoospp`, since archived) framed containerization as: the run-fn returns a `(container {:image ... :cmd ...})` table; the runner spawns a one-shot container per job with that spec. Container is a fire-and-forget primitive. The run-fn is a planner.

This session reconsidered that model.

## Three architectures

A. **VM-in-container** — Lua/Fennel runs inside the per-run container. Heaviest image (Lua + Fennel + quire glue per job), and a double-evaluation problem: graph extraction has to happen outside the container, per-job execution inside.

B. **VM-on-host, `(container {...})` spec** — what `uutoospp` described. Fennel reduces to a configuration DSL; the run-fn emits a static spec. The container is fire-and-forget and the run-fn cannot react to mid-command output. Most of Fennel-the-language's value (branching, data manipulation, reuse) is wasted because the only thing crossing the container boundary is a static spec table.

C. **VM-on-host, `sh` tunnels via `docker exec`** — the run-fn executes inside the host process; each `(sh ...)` call execs into the run's container. Fennel becomes the orchestrator: branching on real `sh` output, parsing intermediate results, conditional follow-up commands, helper functions. The container is the sandbox boundary for individual commands.

C wins because the entire reason for using Fennel rather than YAML or JSON-with-templates is dynamic orchestration. Under B, you lose that. Under C, you get it.

## Granularity: per-run, not per-job

One container per run, shared across all jobs in the run, instead of one container per job.

Per-run is simpler: one container start per run, workspace and toolchain caches shared across jobs naturally, multi-job (when it lands) becomes concurrent `docker exec` into the same container. Per-run gives up per-job image differentiation (mitigation: pipeline-level image suffices for v1; per-job override can be added later if needed) and hard isolation between jobs (not a concern at personal-forge scale).

## API changes

`(container ...)` is removed as a primitive. `(sh cmd opts?)` becomes the only host-effect channel — the chokepoint that makes the in-process Lua VM sandbox actually meaningful (every effect goes through one auditable Rust function instead of `os.execute`, `io.open`, etc. quietly providing alternates).

`(ci.image <name>)` is added as a top-level pipeline registration form. Single image per pipeline. Per-job override can be a third opts arg to `ci.job` later if pipelines need heterogeneity. YAGNI for now.

The run-fn signature stays `(fn [{: sh : secret : jobs}] ...)`. Returning `nil` still skips the job; returning anything else marks it complete and records the value as outputs.

## Persistence: streaming JSONL

Replaces today's buffered `output()`-then-`write_all_logs` flow.

Per-job log: `<run-dir>/jobs/<id>/log.jsonl`, one JSON object per line:

- `{ts, kind: "sh-start", n, cmd}`
- `{ts, kind: "stdout"|"stderr", n, data, encoding?}` — `encoding: "base64"` marker for non-UTF-8 bytes; default UTF-8
- `{ts, kind: "sh-exit", n, exit, signal?, duration_ms}`

Per-run log: `<run-dir>/log.jsonl`:

- `{ts, kind: "container-start", image, container-id}`
- `{ts, kind: "container-died", reason}` — distinct from sh-exit-non-zero (OOMKill, image-pull failure, daemon kill)
- `{ts, kind: "container-end", status}`

JSONL is append-only and tail-able; the future web view streams the file with no extra protocol. Crash-safe (truncate at the last complete line). The Lua-side `ShOutput` table return shape doesn't change — Rust accumulates while writing.

## stdout/stderr separation

`docker exec` without `-t` keeps stdout and stderr as distinct streams. Docker multiplexes them in its frame protocol (8-byte header: stream-ID byte + length, payload follows); the Docker CLI and `bollard` both demux for the caller. Always invoke without TTY allocation. Cross-stream byte ordering is approximate; per-event timestamps preserve temporal ordering for replay.

## In-process VM sandbox

Two layers, additive:

1. **Compile-then-execute split** (`lsqluktu`). Keep a Lua 5.4 VM with full `debug` for Fennel macroexpansion and traceback; execute compiled output in a separate `Lua::new()` VM with `io`/`os`/`debug` removed and only `{sh, secret, jobs, string, table, math}` exposed. Cheap; doesn't touch Fennel internals.

2. **Luau as defense in depth** (new icebox `rzsonvsx`). Swap mlua's execute-VM backend from Lua 5.4 to Luau. Adds bytecode-load validation and a tighter `debug` API that closes runtime introspection escapes pure-Lua sandboxes leak through (`debug.getupvalue`, metatable manipulation). Depends on Fennel's *compiled* output being Luau-compatible at runtime — needs verification before adopting. The previous Luau investigation (`nlvwpspv`) flagged Fennel's *compile-time* use of debug; runtime is a different question.

Both layer cleanly because the sandbox lives on the *execute* VM only; the compile VM stays Lua 5.4 throughout.

## What this design does not address

- **Multi-job DAG** (`sxllwuxk`) under per-run container. Parallel jobs become concurrent `docker exec` calls into the same container. Read-only parallel jobs (lint + test) compose cleanly; parallel jobs that mutate `/work` will collide. Solved later when multi-job lands.
- **Per-repo cache** (`zopyouwu`). Bind-mounted into the run container instead of per-job. Same principle, different mount point.
- **Mirror push job**. Under per-run, runs in the same container as user jobs. Image needs `git`. Most workload images have it.
- **Preflight gating** (`zvvkmrlx`). Less valuable under per-run (the container is already up, so skipping a job only saves the run-fn invocation and any `sh` calls). Kept as low-priority icebox.

## Backlog references

- `vowkxpuz` — Pipeline-level container image declaration
- `lpmoszxo` — Per-run container lifecycle
- `knmkqkvx` — Route sh through docker exec into the run container
- `xrupozur` — Streaming JSONL log persistence per job
- `zmtuqwly` — Detect container-died as a distinct failure mode
- `lsqluktu` — Sandbox CI execution with compile-then-run separation
- `rzsonvsx` — Adopt Luau for the execute VM as defense in depth
- `zvvkmrlx` — Preflight gating to skip jobs via :when predicate
- Archived: `uutoospp` (B-shaped, superseded)
- Prior investigation: `nlvwpspv` (Luau)
