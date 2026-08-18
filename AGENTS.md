# Project conventions

## Task management

The backlog lives in the `ranger` CLI, not GitHub Issues. Use `ranger` to read and update tasks.

Skip pre-flight verification of things already guaranteed. `RANGER_DEFAULT_BACKLOG=quire` is set in the shell — don't `echo` it, don't pass `--backlog`, don't run `ranger backlog list` to "confirm." Ranger creates tags on first use, so `ranger tag list` before `tag add` is unnecessary. Investigation commands (`ranger task list`, `ranger task show`, reading code) are fine when they're actually useful for the work at hand — just don't run them reflexively before every operation.

When the user asks to add a task, run `ranger task create` followed by `ranger tag add` to apply relevant tags. Always tag new tasks — look at the existing tag list and apply whatever fits. Write the description from what was discussed; don't pad it with material the user didn't raise.

The backlog shifts between sessions and even within a session — tasks get reordered, retitled, moved between states, closed, or added by the user without notice. Before acting on a task you remember (or one referenced earlier in the conversation), re-run `ranger task show <key>` and confirm state, ordering, and description against ground truth. Do not trust earlier `ranger task list` output to still be accurate; refetch when placement matters (e.g. moving to top/back of ready).

## Error handling

**Library code** (anything that isn't a binary entry point or CLI command handler)
must use typed error enums derived with `thiserror`. Return `Result<T, SomeError>`
— never `miette::Result`. Do not use `bail!`, `ensure!`, or `IntoDiagnostic` in
library code; construct errors explicitly.

**CLI and binary entry points** (`main.rs`, `commands/`) are the miette boundary.
Use `bail!` and `ensure!` for ad-hoc checks. Use `?` to propagate typed errors
upward — it converts them to `miette::Report` automatically because every typed
error in this codebase implements `miette::Diagnostic`. Use `IntoDiagnostic` (via
`.into_diagnostic()`) only to bridge non-miette, non-`Diagnostic` error types like
`std::io::Error` from third-party calls.

`miette::Context` (`.context()`, `.with_context()`) is available in CLI code to
add human-readable narrative around propagated errors.

## Before committing

Always run `just all` and verify everything passes before committing. No exceptions — this is not optional. If you commit without running it, you will break the build.

## Updating docs

When changing behavior (mirroring, config, hook dispatch, Docker layout, CI workflows), update the corresponding docs in the same commit. `docs/README.md` indexes the whole tree; the docs to check:

- `README.md` — feature descriptions and project status.
- `docs/ARCHITECTURE.md` — architecture and locked-in design decisions.
- `docs/CI.md`, `docs/CI-STATE.md`, `docs/CI-FENNEL.md` — CI design, run/job lifecycle, and the pipeline DSL. `CI-STATE.md` tracks what the code actually does; keep it honest when CI behavior changes.
- `docs/config.md` — config file schemas and how they're loaded.
- `.github/workflows/` — workflow changes must be self-documenting (comments on permissions, triggers).

Documents under `docs/plans/` and `docs/notes/` are dated historical records. Don't retro-edit them to match new behavior; write a new one instead.

If you're unsure whether a doc needs updating, it probably does.
