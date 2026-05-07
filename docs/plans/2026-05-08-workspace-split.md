# Workspace split for runtime extraction

## Crates

```
quire-lib/      fennel, secret, event, ci::{pipeline,registration,mirror,runtime,error}, Repo, CommitRef, RunMeta
quire-server/   depends on quire-lib + axum, rusqlite, sentry, etc. db, web, Quire, ci::{run,docker,logs}, Ci, trigger, all commands
quire-ci/       depends on quire-lib only. eval subcommand
```

## Notes

Docker executor dies — quire-ci runs inside the container, so (sh ...) is always local. That removes the runtime ↔ run coupling that would've complicated the split.

## Stages

1. Create workspace, move modules into quire-lib, get both bins compiling.
2. Wire quire-ci eval to the runtime modules.
3. Server dispatches quire-ci inside run containers instead of evaluating in-process.
4. Remove in-process evaluator.
