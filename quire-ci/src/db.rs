use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

static MIGRATIONS: std::sync::LazyLock<Migrations<'static>> = std::sync::LazyLock::new(|| {
    Migrations::new(vec![M::up(include_str!("../migrations/0001_initial.sql"))])
});

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),
}

/// An opened, migrated SQLite database.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, Error> {
        tracing::debug!(path = %path.display(), "opening database");
        let mut conn = open_connection(path)?;
        MIGRATIONS.to_latest(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }
}

fn open_connection(path: &Path) -> Result<Connection, rusqlite::Error> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(conn)
}

#[cfg(test)]
pub fn open_in_memory() -> Result<Db, Error> {
    let mut conn = Connection::open_in_memory()?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    MIGRATIONS.to_latest(&mut conn)?;
    Ok(Db {
        conn: Arc::new(Mutex::new(conn)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_without_panicking() {
        open_in_memory().expect("migrations should apply cleanly");
    }

    #[test]
    fn runs_table_has_expected_columns() {
        let db = open_in_memory().expect("open_in_memory");
        let conn = db.lock();
        let mut stmt = conn
            .prepare("PRAGMA table_info(runs)")
            .expect("prepare pragma");
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .expect("query")
            .map(|r| r.expect("column name"))
            .collect();

        for expected in &[
            "id",
            "repo",
            "ref",
            "sha",
            "created_at",
            "dispatched_at",
            "resolved_at",
            "outcome",
            "traceparent",
        ] {
            assert!(
                columns.contains(&expected.to_string()),
                "missing column: {expected}"
            );
        }
    }
}
