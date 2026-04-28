# Project conventions

## Task management

The backlog lives in the `ranger` CLI, not GitHub Issues. Use `ranger` to read and update tasks.

## Before committing

Always run `just all` and verify everything passes before committing. No exceptions — this is not optional. If you commit without running it, you will break the build.

## Updating docs

When changing behavior (mirroring, config, hook dispatch, Docker layout, CI workflows), update the corresponding docs in the same commit. The docs to check:

- `README.md` — feature descriptions and project status.
- `docs/PLAN.md` — build sequence, architecture, and locked-in design decisions.
- `docs/config.md` — config file schemas and how they're loaded.
- `.github/workflows/` — workflow changes must be self-documenting (comments on permissions, triggers).

If you're unsure whether a doc needs updating, it probably does.
