//! Database connection management and migration runner.
//!
//! [`open`] creates a connection with WAL mode and foreign keys enabled.
//! [`migrate`] runs pending migrations — call once at server startup.

use std::path::Path;

use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

/// The ordered set of schema migrations. Append-only — never edit
/// a migration that has already shipped.
static MIGRATIONS: std::sync::LazyLock<Migrations<'static>> = std::sync::LazyLock::new(|| {
    Migrations::new(vec![
        M::up(include_str!("../migrations/0001_initial.sql")),
        M::up(include_str!("../migrations/0002_sh_events.sql")),
        M::up(include_str!("../migrations/0003_ci_api.sql")),
    ])
});

/// Error from running migrations.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration error: {0}")]
    Migration(#[source] rusqlite_migration::Error),
}

impl From<rusqlite_migration::Error> for MigrationError {
    fn from(e: rusqlite_migration::Error) -> Self {
        Self::Migration(e)
    }
}

/// Open the database at `path`, enable WAL mode and foreign keys.
/// Creates the file if it doesn't exist.
///
/// Does not run migrations. Call [`migrate`] once at server startup.
pub fn open(path: &Path) -> Result<Connection, rusqlite::Error> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(conn)
}

/// Run any pending migrations on the given connection.
///
/// Call once at server startup, after [`open`].
pub fn migrate(conn: &mut Connection) -> Result<(), MigrationError> {
    MIGRATIONS.to_latest(conn)?;
    Ok(())
}

/// Open an in-memory database with migrations applied (for tests).
#[cfg(test)]
pub fn open_in_memory() -> Result<Connection, MigrationError> {
    let mut conn = Connection::open_in_memory()?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    MIGRATIONS.to_latest(&mut conn)?;
    Ok(conn)
}
