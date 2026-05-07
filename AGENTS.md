# Project conventions

## Task management

The backlog lives in the `ranger` CLI, not GitHub Issues. Use `ranger` to read and update tasks.

The backlog shifts between sessions and even within a session — tasks get reordered, retitled, moved between states, closed, or added by the user without notice. Before acting on a task you remember (or one referenced earlier in the conversation), re-run `ranger task show <key>` and confirm state, ordering, and description against ground truth. Do not trust earlier `ranger task list` output to still be accurate; refetch when placement matters (e.g. moving to top/back of ready).

## Before committing

Always run `just all` and verify everything passes before committing. No exceptions — this is not optional. If you commit without running it, you will break the build.

## Updating docs

When changing behavior (mirroring, config, hook dispatch, Docker layout, CI workflows), update the corresponding docs in the same commit. The docs to check:

- `README.md` — feature descriptions and project status.
- `docs/PLAN.md` — build sequence, architecture, and locked-in design decisions.
- `docs/config.md` — config file schemas and how they're loaded.
- `.github/workflows/` — workflow changes must be self-documenting (comments on permissions, triggers).

If you're unsure whether a doc needs updating, it probably does.
