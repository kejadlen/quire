# Fennel embedding

How quire loads `.fnl` config files into typed Rust structs. Covers the
global config at `/var/quire/config.fnl` and the per-repo config checked
in at `.quire/config.fnl` (read from the bare repo via
`git show <sha>:.quire/config.fnl`). CI pipelines reuse the same
embedding (see [`CI-FENNEL.md`](CI-FENNEL.md)), but their design is out
of scope here.

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
DSL. Earlier sketches had forms like `(notifications :to [...] :on [...])`
which read as function calls, but a DSL adds parser machinery for no
win — config stays data; only CI pipelines get real code.

A representative per-repo config:

```fennel
{:mirrors {"https://github.com/user/repo.git" :github-mirror}}
```

Today each call site constructs a fresh `Fennel` — that's what the
`load_config` / `load_config_str` associated functions do. Cheap enough
at current call volume; reusing one instance across loads is available
(construct a `Fennel` and call the methods directly) but not yet needed.

`load_string` is the primitive; `load_file` wraps it. Per-repo config
comes from `git show` stdout, not a path on disk, so the string form is
load-bearing. The `name` argument is for error messages — a filename
or a synthetic label like `HEAD:.quire/config.fnl`.

Unknown fields don't fail the load. Deserialization runs through
`serde_ignored`, and every key the target struct doesn't consume is
reported to an `on_unknown` callback. The `load_config*` wrappers
collect them into a single `tracing::warn!` so a typo'd config key is
visible without being fatal.

Errors are a typed `FennelError` enum (`thiserror`) that also derives
`miette::Diagnostic`, so CLI callers get source labels and line/column
info from Lua via `?`. Hook log lines should point at the offending
file and line, not just "syntax error."

Lives in `quire-core/src/fennel.rs`. Used by `Quire::global_config` and
`Repo::config` in `quire-server/src/quire/mod.rs`, which also define the
`GlobalConfig` and `RepoConfig` schemas.

## Contracts

```rust
pub struct Fennel { /* private */ }

impl Fennel {
    pub fn new() -> Result<Self, FennelError>;

    pub fn load_string<T: DeserializeOwned>(
        &self,
        source: &str,
        name: &str,
        on_unknown: impl FnMut(&serde_ignored::Path<'_>),
    ) -> Result<T, FennelError>;

    pub fn load_file<T: DeserializeOwned>(
        &self,
        path: &Path,
        on_unknown: impl FnMut(&serde_ignored::Path<'_>),
    ) -> Result<T, FennelError>;

    // Fresh-VM convenience wrappers; warn on unknown fields.
    pub fn load_config<T: DeserializeOwned>(path: &Path) -> Result<T, FennelError>;
    pub fn load_config_str<T: DeserializeOwned>(source: &str, name: &str) -> Result<T, FennelError>;
}
```

Errors: file-not-found, parse error, eval error, type mismatch — all
`FennelError` variants carrying named source labels where Lua provides
them.

## Related modules

- `quire-core/src/secret.rs` — `SecretString` wraps Fennel-loaded
  strings that resolve from a file on access.
- `quire-server/src/quire/mod.rs` — `Quire::global_config` reads global
  config from disk.

## Covered behavior

The tests in `quire-core/src/fennel.rs` pin: round-tripping flat and
nested tables into structs, `load_file` matching `load_string`,
file-not-found as a distinct error, malformed Fennel and type
mismatches naming their source, the unknown-field callback firing at
both top level and nested depth, error labels surviving a `:` in the
source name, line/column extraction from Lua errors, and the stdlib
and `quire.ci` placeholder modules being preloaded at construction.

Two adjacent cases worth not confusing: an *empty file* is an error
(`invalid type: nil, expected table` — the chunk returns nil), while an
*empty table* `{}` deserializes to a struct of defaults and is the
valid minimal global config.
