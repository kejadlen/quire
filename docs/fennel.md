# Fennel embedding

How quire loads `.fnl` config files into typed Rust structs. Covers the
global config at `/var/quire/config.fnl` and the per-repo config checked
in at `.quire/config.fnl` (read from the bare repo via
`git show HEAD:.quire/config.fnl`). CI pipeline support will reuse this
machinery later, but its design is out of scope here.

## Components

- **`mlua`** — bindings to a Lua VM. Use the `serde` feature for
  `LuaSerdeExt`, which converts Lua values into anything
  `DeserializeOwned`. `lua54` for the runtime; no Fennel-specific
  reason.
- **Vendored Fennel compiler** — `fennel.lua` from upstream (BSD-3,
  single Lua file). Bundled via `include_str!`, registered into the VM
  as a module at construction.
- **`Fennel` struct** — owns a `Lua` instance with the Fennel compiler
  registered as a Lua global. `load_file` and `load_string` are methods
  that look the global up on each call.

## Decisions

Files evaluate to a single Lua table literal. Pure data, not a
DSL. PLAN.md sketches `(notifications :to [...] :on [...])` which reads
as a function call, but a DSL adds parser machinery for no v1 win. Move
to a DSL when CI lands and there's a real reason.

A representative per-repo config:

```fennel
{:mirror {:url "https://github.com/owner/repo.git"}
 :notifications {:to ["alpha@example.com"]
                 :on [:ci-failed :mirror-failed]}}
```

Today each call site (`Quire::global_config`, `Repo::config`)
constructs a fresh `Fennel`. Cheap enough at current call volume.
Reusing a single instance across loads is a planned optimization for
when `quire serve` lands and starts loading per-request.

`load_string` is the primitive; `load_file` wraps it. Per-repo config
comes from `git show` stdout, not a path on disk, so the string form is
load-bearing. The `name` argument is for error messages — a filename
or a synthetic label like `HEAD:.quire/config.fnl`.

Errors flow through miette. Wrap `mlua::Error` with the source name
and any line/column info Lua surfaces. Hook log lines should point at
the offending file and line, not just "syntax error."

Lives in `src/fennel.rs`. Used by `Quire::global_config` and
`Repo::config` in `src/quire.rs`, which also define the `GlobalConfig`
and `RepoConfig` schemas.

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

## Related modules

- `src/secret.rs` — `SecretString` wraps Fennel-loaded strings that
  resolve from a file or shell command on access.
- `src/quire.rs` — `Repo::config` reads per-repo Fennel via `git show
  HEAD:.quire/config.fnl`; `Quire::global_config` reads the global
  config from disk. Both define the schema structs they parse into.

## Test plan

- `load_string` round-trip on a representative table → struct.
- `load_file` reads from disk and behaves the same as `load_string`.
- File-not-found surfaces as a distinct error.
- Malformed Fennel → error mentions the source name.
- Type mismatch (string where number expected) → error mentions the
  field.
- Empty file → error. An empty config file is almost always a mistake.
