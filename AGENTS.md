# Project conventions

## Task management

The backlog lives in the `ranger` CLI, not GitHub Issues. Use `ranger` to read and update tasks. `RANGER_DEFAULT_BACKLOG=quire` is set in the shell, so `--backlog` can be omitted.

The backlog shifts between sessions and even within a session — tasks get reordered, retitled, moved between states, closed, or added by the user without notice. Before acting on a task you remember (or one referenced earlier in the conversation), re-run `ranger task show <key>` and confirm state, ordering, and description against ground truth. Do not trust earlier `ranger task list` output to still be accurate; refetch when placement matters (e.g. moving to top/back of ready).

When the user asks to add a task, the only command you should run is `ranger task create`. Do not run `ranger backlog list`, `ranger task list`, `ranger task show`, or read any code files to "verify" or "scope" the task — write the task from what the user said, verbatim if possible. The default backlog is already set via `RANGER_DEFAULT_BACKLOG`, so `--backlog` is unnecessary. Investigate only if the user asks you to pick up the task.

## Before committing

Always run `just all` and verify everything passes before committing. No exceptions — this is not optional. If you commit without running it, you will break the build.

## Updating docs

When changing behavior (mirroring, config, hook dispatch, Docker layout, CI workflows), update the corresponding docs in the same commit. The docs to check:

- `README.md` — feature descriptions and project status.
- `docs/PLAN.md` — build sequence, architecture, and locked-in design decisions.
- `docs/config.md` — config file schemas and how they're loaded.
- `.github/workflows/` — workflow changes must be self-documenting (comments on permissions, triggers).

If you're unsure whether a doc needs updating, it probably does.
