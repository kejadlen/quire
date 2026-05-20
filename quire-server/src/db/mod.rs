//! Database connection management and migration runner.
//!
//! [`Db::open`] creates a connection with WAL mode and foreign keys enabled.
//! [`Db::migrate`] runs pending migrations — call once at server startup.
//!
//! All SQL queries are in the [`runs`] submodule.

pub mod runs;

use std::path::Path;

use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

/// The ordered set of schema migrations. Append-only — never edit
/// a migration that has already shipped.
static MIGRATIONS: std::sync::LazyLock<Migrations<'static>> = std::sync::LazyLock::new(|| {
    Migrations::new(vec![
        M::up(include_str!("../../migrations/0001_initial.sql")),
        M::up(include_str!("../../migrations/0002_sh_events.sql")),
        M::up(include_str!("../../migrations/0003_ci_api.sql")),
        M::up(include_str!("../../migrations/0004_bootstrap_api.sql")),
        M::up(include_str!("../../migrations/0005_rename_run_token.sql")),
        M::up(include_str!("../../migrations/0006_traceparent.sql")),
        M::up(include_str!("../../migrations/0007_schema_cleanup.sql")),
        M::up(include_str!("../../migrations/0008_rename_sh.sql")),
        M::up(include_str!("../../migrations/0009_rename_ci_vocab.sql")),
        M::up(include_str!("../../migrations/0010_outcome_schema.sql")),
    ])
});

/// Error from running migrations.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum MigrationError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),
}

/// A newtype wrapper around a SQLite connection.
///
/// All SQL queries are exposed as methods on this type via `impl Db` blocks
/// in the submodules. Callers never access the inner [`Connection`] directly.
pub struct Db(Connection);

impl Db {
    /// Open the database at `path`, enable WAL mode and foreign keys.
    /// Creates the file if it doesn't exist.
    ///
    /// Does not run migrations. Call [`Db::migrate`] once at server startup.
    pub fn open(path: &Path) -> Result<Db, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        Ok(Db(conn))
    }

    /// Run any pending migrations on this connection.
    ///
    /// Call once at server startup, after [`Db::open`].
    pub fn migrate(&mut self) -> Result<(), MigrationError> {
        MIGRATIONS.to_latest(&mut self.0)?;
        Ok(())
    }

    /// Open an in-memory database with migrations applied (for tests).
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Db, MigrationError> {
        let mut conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        MIGRATIONS.to_latest(&mut conn)?;
        Ok(Db(conn))
    }
}
