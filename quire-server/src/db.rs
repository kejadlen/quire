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
        M::up(include_str!("../migrations/0004_bootstrap_api.sql")),
        M::up(include_str!("../migrations/0005_rename_run_token.sql")),
        M::up(include_str!("../migrations/0006_traceparent.sql")),
        M::up(include_str!("../migrations/0007_schema_cleanup.sql")),
        M::up(include_str!("../migrations/0008_rename_sh.sql")),
        M::up(include_str!("../migrations/0009_rename_ci_vocab.sql")),
        M::up(include_str!("../migrations/0010_outcome_schema.sql")),
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
///
/// Foreign key enforcement is disabled for the duration of the migration and
/// re-enabled afterward. This is required because several migrations rebuild
/// the `runs` table using the SQLite "rename trick" (`CREATE runs_new`,
/// `DROP runs`, `RENAME runs_new TO runs`), and SQLite's `DROP TABLE`
/// triggers `ON DELETE CASCADE` when foreign keys are enabled — silently
/// deleting all child rows in `jobs`. Disabling foreign keys prevents the
/// cascade while still preserving referential integrity (the parent IDs don't
/// change, only the schema does).
pub fn migrate(conn: &mut Connection) -> Result<(), MigrationError> {
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = MIGRATIONS.to_latest(conn);
    conn.pragma_update(None, "foreign_keys", "ON")?;
    result?;
    Ok(())
}

/// Open an in-memory database with migrations applied (for tests).
#[cfg(test)]
pub fn open_in_memory() -> Result<Connection, MigrationError> {
    let mut conn = Connection::open_in_memory()?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    migrate(&mut conn)?;
    Ok(conn)
}

#[cfg(test)]
mod migration_fk_tests {
    use super::*;

    #[test]
    fn test_migrations_with_existing_data_preserve_jobs() {
        // Apply migrations 0001-0008 and seed data, then apply 0009+0010.
        // Verifies that table-rebuild migrations don't accidentally cascade-
        // delete child rows when foreign_keys is ON.
        let migrations_to_8 = rusqlite_migration::Migrations::new(vec![
            rusqlite_migration::M::up(include_str!("../migrations/0001_initial.sql")),
            rusqlite_migration::M::up(include_str!("../migrations/0002_sh_events.sql")),
            rusqlite_migration::M::up(include_str!("../migrations/0003_ci_api.sql")),
            rusqlite_migration::M::up(include_str!("../migrations/0004_bootstrap_api.sql")),
            rusqlite_migration::M::up(include_str!("../migrations/0005_rename_run_token.sql")),
            rusqlite_migration::M::up(include_str!("../migrations/0006_traceparent.sql")),
            rusqlite_migration::M::up(include_str!("../migrations/0007_schema_cleanup.sql")),
            rusqlite_migration::M::up(include_str!("../migrations/0008_rename_sh.sql")),
        ]);

        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        // Apply migrations 0001-0008 with FK off so the rebuild in 0007 doesn't
        // cascade (same as migrate() does for the full set).
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        migrations_to_8.to_latest(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        conn.execute_batch("
            INSERT INTO runs (id, repo, ref_name, sha, pushed_at_ms, state, queued_at_ms, started_at_ms, finished_at_ms)
            VALUES ('run1', 'repo', 'refs/heads/main', 'abc', 1000, 'complete', 1000, 1001, 1002);
            INSERT INTO jobs (run_id, job_id, state, started_at_ms, finished_at_ms)
            VALUES ('run1', 'job1', 'complete', 1001, 1002);
        ").unwrap();

        // Apply remaining migrations via migrate() which disables FK during migration.
        migrate(&mut conn).expect("migrations must succeed");

        let job_count: i64 = conn
            .query_row("SELECT count(*) FROM jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(job_count, 1, "jobs should survive table-rebuild migrations");
    }
}
