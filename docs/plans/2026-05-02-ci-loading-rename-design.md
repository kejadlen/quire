# CI loading: rename and re-shape errors

**Goal:** Rationalize the names and error categories around CI loading so the structure matches how the code actually thinks. No behavior changes.

## What's wrong now

Reading `src/ci/{mod,pipeline,lua}.rs` and `src/error.rs` back, three things feel arbitrary:

1. **"load" is overloaded.** `Ci::load`, `Pipeline::load`, and `LoadError` all use the word but mean different things. The overloading is triggered by the error name — once that's gone, the methods read fine.
2. **`ValidationError` is a grab bag.** Variants come from two distinct stages — registration-time rules (`ReservedSlash`, `EmptyInputs`, `DuplicateImage`) and post-graph rules (`Cycle`, `Unreachable`) — but share one enum, one suffix, and no structure to signal the difference.
3. **`Error::Validation` names a stage, not a domain.** Compare with the sibling `Error::Fennel`, which names its source. "Validation failed" reads weirdly next to "fennel error."

## New shape

### Two error categories

The split that already exists in the code (per the comment in `validate_post_graph`) is "caught at registration time" vs. "caught after the graph is built." Make that explicit:

```rust
pub enum DefinitionError {
    ReservedSlash { job_id: String, span: SourceSpan },
    EmptyInputs   { job_id: String, span: SourceSpan },
    DuplicateImage { span: SourceSpan },
}

pub enum StructureError {
    Cycle       { cycle_jobs: Vec<String>, spans: Vec<SourceSpan> },
    Unreachable { job_id: String, span: SourceSpan },
}

pub enum Diagnostic {
    Definition(DefinitionError),
    Structure(StructureError),
}
```

The per-job vs. pipeline-singleton distinction (DuplicateImage is the lone singleton today) is an implementation detail of detection, not a user-visible category, so two buckets is the right count.

### Error bag and top-level variant

```rust
pub struct PipelineError {
    pub src: NamedSource<String>,
    pub diagnostics: Vec<Diagnostic>,
}
```

At the top level: `Error::Pipeline(Box<PipelineError>)`. Replaces `Error::Validation(Box<LoadError>)`.

### Method and module renames

| Old | New | Reason |
|---|---|---|
| `Ci::load(commit)` | `Ci::pipeline(commit)` | Returns the pipeline at a commit; noun-style accessor reads more naturally than "load". |
| `Pipeline::load(src, name)` (method) | `pipeline::compile(src, name)` (free fn) | Source → runnable artifact is compilation. Free fn avoids "the type compiles itself." |
| `lua::parse(...)` | `lua::register(...)` | Phase naming. Fennel's actual parser runs inside `eval_raw`; this function performs the registration phase. |
| `ParseOutput` | `Registrations` | Names what came out, not what step ran. |
| `ValidationError` | split into `DefinitionError` + `StructureError` (see above) | Stage-aligned. |
| `LoadError` | `PipelineError` | Domain-named bag. |
| `Error::Validation` | `Error::Pipeline` | Domain-named, matches the bag. |

The verb chain reads top-down:

```
Ci::pipeline → pipeline::compile → lua::register
```

Each verb describes its own layer. "Register" is imperfect (the Fennel script does the registering; the Rust function provides the sinks and harvests), but it names the phase clearly and avoids "evaluate" — which would collide with Lua's eval terminology.

## Scope

In-scope:

- Renames listed above
- Splitting the validation enum into two
- Adding the `Diagnostic` wrapper for miette's `#[related]` iteration
- Updating tests in `src/ci/pipeline.rs`, `src/ci/lua.rs`, `src/ci/mod.rs`, and `src/error.rs`
- Updating call sites — at minimum `src/bin/quire/commands/ci.rs`

Out of scope:

- Behavioral changes to validation rules
- Merging `Error::Fennel` and `Error::Pipeline` (the user-facing "your ci.fnl is bad" framing wasn't a concern in this pass)
- Restructuring `lua.rs` runtime types (`Runtime`, `RuntimeHandle`, `ShOutput`)

## Verification

- `cargo test` passes with no behavior changes
- Error rendering for a multi-error ci.fnl still produces the same miette output (modulo the type names in `Debug`)
- No public API used outside `src/ci/` and `src/bin/` should change names

## Open questions

- "Register" is acceptable but not loved. If a better verb surfaces during implementation, revisit before committing.
