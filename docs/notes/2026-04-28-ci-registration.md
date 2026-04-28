# Fennel registration + structural validation

## Scope

Evaluate `.quire/ci.fnl` via the existing Fennel integration and register jobs. Validate the resulting job graph. No execution — that's a future task.

## Capabilities

1. **Evaluate `ci.fnl`** in a fresh Lua VM with `job` in scope.
2. **`job` registers into a table** — `(job :test [:quire/push] (fn [_] nil))` records `{id: "test", inputs: ["quire/push"], run: <function>}`.
4. **Four structural validations** run after eval:
   - Acyclic (Kahn's algorithm)
   - Non-empty inputs
   - Reachability from a source ref
   - No `/` in user job ids
5. **Errors produce failed runs**

## Components

**`ci::EvalResult`** — what comes back from evaluating `ci.fnl`:
```
jobs: Vec<JobDef>   // id, inputs, run_fn (kept as mlua::Function for later)
```

**`ci::eval_ci`** — takes a `Fennel` instance and a source string, returns `EvalResult`. Creates a fresh VM, injects `job` global, evals the source, extracts the registration table.

**`ci::validate`** — takes `&[JobDef]`, returns `Result<(), Vec<ValidationError>>`. Runs the four rules. Pure function, no I/O.

**Integration point** — `dispatch_push` in `event.rs` currently does `runs.create(&meta)` then immediately completes the run. After this change: create the run, transition to `Active`, eval `ci.fnl`, validate, then either complete (success) or fail with the validation error.

## Contracts

```rust
struct JobDef {
    id: String,
    inputs: Vec<String>,
    run_fn: mlua::Function,  // kept for future execution, not called here
}

struct ValidationError {
    message: String,
}

fn eval_ci(fennel: &Fennel, source: &str, name: &str) -> Result<EvalResult>;
fn validate(jobs: &[JobDef]) -> Result<(), Vec<ValidationError>>;
```

## Key decisions

- **One eval context** — registration and "run start" collapse into a single eval per run, per the v0 note in CI-FENNEL.md.
- **`job` accumulates into a registration table** — a Lua table in the VM that `job` pushes into. After eval, we extract it from the globals.
- **`mlua::Function` stored but not called** — the `run_fn` field preserves the function for the future execution task. We don't call it here.
- **Validation errors are batched** — collect all violations, return them together. The run's state file records all of them, not just the first.
- **Fennel eval errors → failed run** — same path as validation failures. The caller (dispatch_push) catches either and transitions accordingly.
- **`container` deferred** — not even a marker for now. A `ci.fnl` that references `container` will get a Fennel "unknown global" error. That's acceptable until the execution task adds it.

## Out of scope

- Container execution (separate task)
- Per-job eval / run-fn invocation
- Multiple source types (just `:quire/push` for now)
- Job outputs, artifacts, caching
