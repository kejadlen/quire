# Project conventions

## Before committing

Always run `cargo test` and verify all tests pass before committing.
Never skip this step, even for small or "obvious" changes.

## Rust conventions

- Use `fs_err` instead of `std::fs` (enforced by clippy)
- Use miette's `bail!` and `ensure!` macros instead of `return Err(miette!(...))`
- Prefer `for` loops over `Iterator::for_each` for side-effects (enforced by clippy)
