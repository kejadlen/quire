use std::path::{Path, PathBuf};

use miette::{IntoDiagnostic, Result};
use rusqlite::Connection;

use quire_core::fennel::Fennel;

use crate::db;

/// Parsed global configuration (`<base-dir>/config.fnl`).
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct GlobalConfig {
    #[serde(default)]
    pub sentry: Option<SentryConfig>,
    /// TCP port the HTTP server binds to on all interfaces (`0.0.0.0`).
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub webhook_secret: Option<quire_core::secret::SecretString>,
    // Reserved for future use: quire-ci will clone from this URL.
    #[allow(dead_code)]
    #[serde(default)]
    pub server_url: Option<String>,
}

fn default_port() -> u16 {
    3000
}

pub use quire_core::telemetry::SentryConfig;

/// Application runtime context.
///
/// Loads config at construction time so callers don't have to thread
/// Results around.
#[derive(Clone)]
pub struct QuireCi {
    config: GlobalConfig,
    db_path: PathBuf,
}

impl QuireCi {
    pub fn new(base_dir: PathBuf) -> Result<Self> {
        let config_path = base_dir.join("config.fnl");
        let config = if config_path.exists() {
            let fennel = Fennel::new().into_diagnostic()?;
            fennel.load_file(&config_path).into_diagnostic()?
        } else {
            GlobalConfig::default()
        };
        let db_path = base_dir.join("quire-ci.db");
        let mut conn = db::open(&db_path).into_diagnostic()?;
        db::migrate(&mut conn).into_diagnostic()?;
        Ok(Self { config, db_path })
    }

    pub fn config(&self) -> &GlobalConfig {
        &self.config
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn open_db(&self) -> Result<Connection, rusqlite::Error> {
        db::open(self.db_path())
    }
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            sentry: None,
            port: default_port(),
            webhook_secret: None,
            server_url: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_config_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.fnl");
        fs_err::write(&config_path, "{}").expect("write");

        let q = QuireCi::new(dir.path().to_path_buf()).expect("should load");
        assert_eq!(q.config().port, 3000);
    }

    #[test]
    fn global_config_parses_custom_port() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.fnl");
        fs_err::write(&config_path, r#"{:port 4000}"#).expect("write");

        let q = QuireCi::new(dir.path().to_path_buf()).expect("should load");
        assert_eq!(q.config().port, 4000);
    }

    #[test]
    fn global_config_missing_file_uses_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");

        let q = QuireCi::new(dir.path().to_path_buf()).expect("should load");
        assert_eq!(q.config().port, 3000);
        assert!(q.config().sentry.is_none());
    }

    #[test]
    fn global_config_loads_sentry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.fnl");
        fs_err::write(
            &config_path,
            r#"{:sentry {:dsn "https://key@sentry.io/123"}}"#,
        )
        .expect("write");

        let q = QuireCi::new(dir.path().to_path_buf()).expect("should load");
        let sentry = q
            .config()
            .sentry
            .as_ref()
            .expect("sentry should be present");
        assert_eq!(sentry.dsn.reveal().unwrap(), "https://key@sentry.io/123");
    }

    #[test]
    fn db_is_created_at_expected_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let q = QuireCi::new(dir.path().to_path_buf()).expect("should load");
        assert_eq!(q.db_path(), dir.path().join("quire-ci.db"));
        assert!(q.db_path().exists(), "db file should exist after new()");
    }
}
