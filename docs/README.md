# quire docs

## Current

Living documents. Keep these true as behavior changes.

| Doc | Covers |
|---|---|
| [`ARCHITECTURE.md`](./ARCHITECTURE.md) | System shape, host/container split, access model, locked-in decisions, open questions. |
| [`CI.md`](./CI.md) | CI design: runner shape, storage, concurrency, sandbox backends. Target design — see the status note at the top. |
| [`CI-STATE.md`](./CI-STATE.md) | Run and job lifecycle **as the code implements it today**, plus the gaps against `CI.md`. |
| [`CI-FENNEL.md`](./CI-FENNEL.md) | The `.quire/ci.fnl` pipeline DSL: `(job id inputs run)`, sources, runtime primitives. |
| [`config.md`](./config.md) | Global and per-repo config schemas, secrets, redaction. |
| [`fennel.md`](./fennel.md) | How `.fnl` files are loaded into typed Rust structs. |
| [`STYLE_GUIDE.md`](./STYLE_GUIDE.md) | Product personality, vocabulary, typography, color, web UI components. |
| [`host/`](./host/README.md) | Reference host configs — sshd block, container start, docker-out-of-docker prerequisites. |

When behavior changes, update the relevant doc in the same commit. `AGENTS.md`
lists which docs to check.

## Historical

Dated records of decisions as they were made. **Not maintained** — a plan
describes what was intended on its date, not necessarily what the code does
now. Read them for rationale and context; trust the current docs and the code
for present behavior.

- [`plans/`](./plans/) — design documents and implementation plans, one per
  change. Newest first is roughly the story of the project. Two are worth
  knowing about because they still describe where things are going:
  - [`2026-08-12-ci-rearchitecture.md`](./plans/2026-08-12-ci-rearchitecture.md)
    — the decided path for CI: per-run containers, a real queue, a per-run
    Unix socket replacing HTTP, mirroring moved out of CI. Supersedes parts of
    `CI.md` and `CI-FENNEL.md`; those docs point at it where they diverge.
  - [`2026-05-08-workspace-split.md`](./plans/2026-05-08-workspace-split.md)
    — the crate split into `quire-core` / `quire-server` / `quire-ci`.
- [`notes/`](./notes/) — smaller scoped design notes that didn't warrant a
  full plan.

A stale reference inside a dated document is expected and not a bug. If you
find one in a **current** doc, fix it.
