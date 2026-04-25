# Fennel embedding

Design note for chunk 1 of step 5 (GitHub mirror via post-receive). Loads
`.fnl` config files into typed Rust structs. Used by global config
(`/var/quire/config.fnl`) and per-repo config (`.quire/config.fnl`, read
via `git show HEAD:.quire/config.fnl`). Will eventually support CI
pipeline definitions, but not yet designed for that.

## Components

- **`mlua`** — bindings to a Lua VM. Use the `serde` feature for
  `LuaSerdeExt`, which converts Lua values into anything
  `DeserializeOwned`. `lua54` for the runtime; no Fennel-specific
  reason.
- **Vendored Fennel compiler** — `fennel.lua` from upstream (BSD-3,
  single Lua file). Bundled via `include_str!`, registered into the VM
  as a module at construction.
- **`Fennel` struct** — owns the `Lua` instance and a reference to the
  loaded `fennel` module. Constructed once per process; `load_file` and
  `load_string` are methods.

## Decisions

Files evaluate to a single Lua table literal. Pure data, not a
DSL. PLAN.md sketches `(notifications :to [...] :on [...])` which reads
as a function call, but a DSL adds parser machinery for no v1 win. Move
to a DSL when CI lands and there's a real reason.

```fennel
{:mirror {:url "https://github.com/owner/repo.git"}
 :notifications {:to ["alpha@example.com"]
                 :on [:ci-failed :mirror-failed]}}
```

One `Fennel` per process, reused across loads. Hooks load 1–2 files;
`quire serve` loads many. Avoids re-loading the compiler on each
call. Cheap enough that tests construct freely.

`load_string` is the primitive; `load_file` wraps it. Per-repo config
comes from `git show` stdout, not a path on disk, so the string form is
load-bearing. The `name` argument is for error messages — a filename
or a synthetic label like `HEAD:.quire/config.fnl`.

Errors flow through miette. Wrap `mlua::Error` with the source name
and any line/column info Lua surfaces. Hook log lines should point at
the offending file and line, not just "syntax error."

New top-level `src/fennel.rs`. Used by the still-to-come
`src/config/global.rs` and `src/config/repo.rs`.

## Contracts

```rust
pub struct Fennel { /* private */ }

impl Fennel {
    pub fn new() -> Result<Self>;
    pub fn load_string<T: DeserializeOwned>(&self, source: &str, name: &str) -> Result<T>;
    pub fn load_file<T: DeserializeOwned>(&self, path: &Path) -> Result<T>;
}
```

Errors: file-not-found, parse error, eval error, type mismatch — all
`miette::Result` with named source labels where Lua provides them.

## Out of scope

- `SecretString` / `!cmd` resolution — chunk 2. Fennel produces plain
  strings; `SecretString` is a `serde` newtype that resolves on access.
- `git show HEAD:.quire/config.fnl` plumbing — chunk 3.
- Any `mirror`/`notifications`/`private` schema — defined when chunks 2
  and 3 land.

## Test plan

- `load_string` round-trip on a representative table → struct.
- `load_file` reads from disk and behaves the same as `load_string`.
- File-not-found surfaces as a distinct error.
- Malformed Fennel → error mentions the source name.
- Type mismatch (string where number expected) → error mentions the
  field.
- Empty file → error. An empty config file is almost always a mistake.
