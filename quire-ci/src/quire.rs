use std::path::PathBuf;

use miette::{IntoDiagnostic, Result, bail};

use quire_core::fennel::Fennel;
use quire_core::secret::SecretString;

use crate::db::Db;

/// Parsed global configuration (`<base-dir>/config.fnl`).
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct GlobalConfig {
    #[serde(default)]
    pub sentry: Option<SentryConfig>,
    /// TCP port the HTTP server binds to on all interfaces (`0.0.0.0`).
    #[serde(default = "default_port")]
    pub port: u16,
    pub webhook_secret: SecretString,
}

fn default_port() -> u16 {
    3000
}

pub use quire_core::telemetry::SentryConfig;

/// Application runtime context.
#[derive(Clone)]
pub struct QuireCi {
    config: GlobalConfig,
    db: Db,
}

impl QuireCi {
    pub fn new(base_dir: PathBuf) -> Result<Self> {
        let config_path = base_dir.join("config.fnl");
        if !config_path.exists() {
            bail!("config file not found: {}", config_path.display());
        }
        let fennel = Fennel::new().into_diagnostic()?;
        let config: GlobalConfig = fennel.load_file(&config_path).into_diagnostic()?;
        let db = Db::open(&base_dir.join("quire-ci.db")).into_diagnostic()?;
        Ok(Self { config, db })
    }

    pub fn config(&self) -> &GlobalConfig {
        &self.config
    }

    pub fn db(&self) -> &Db {
        &self.db
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &tempfile::TempDir, content: &str) {
        fs_err::write(dir.path().join("config.fnl"), content).expect("write config");
    }

    fn minimal_config(secret: &str) -> String {
        format!(r#"{{:webhook-secret "{secret}"}}"#)
    }

    #[test]
    fn global_config_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_config(&dir, &minimal_config("s3cret"));

        let q = QuireCi::new(dir.path().to_path_buf()).expect("should load");
        assert_eq!(q.config().port, 3000);
    }

    #[test]
    fn global_config_parses_custom_port() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_config(&dir, r#"{:webhook-secret "s3cret" :port 4000}"#);

        let q = QuireCi::new(dir.path().to_path_buf()).expect("should load");
        assert_eq!(q.config().port, 4000);
    }

    #[test]
    fn global_config_missing_file_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = QuireCi::new(dir.path().to_path_buf());
        assert!(result.is_err(), "should error when config file is missing");
    }

    #[test]
    fn global_config_loads_sentry() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_config(
            &dir,
            r#"{:webhook-secret "s3cret" :sentry {:dsn "https://key@sentry.io/123"}}"#,
        );

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
        write_config(&dir, &minimal_config("s3cret"));
        QuireCi::new(dir.path().to_path_buf()).expect("should load");
        assert!(
            dir.path().join("quire-ci.db").exists(),
            "db file should exist after new()"
        );
    }
}
