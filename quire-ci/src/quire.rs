use std::path::PathBuf;

use miette::{Result, ensure};

use quire_core::fennel::Fennel;

/// Parsed global configuration (`<base-dir>/config.fnl`).
#[derive(serde::Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct GlobalConfig {
    #[serde(default)]
    pub sentry: Option<SentryConfig>,
    /// TCP port the HTTP server binds to on all interfaces (`0.0.0.0`).
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_port() -> u16 {
    3000
}

#[derive(serde::Deserialize, Debug)]
pub struct SentryConfig {
    pub dsn: quire_core::secret::SecretString,
}

/// Application runtime context.
///
/// Carries configuration and provides resolved paths.
#[derive(Clone)]
pub struct QuireCi {
    base_dir: PathBuf,
}

impl QuireCi {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn config_path(&self) -> PathBuf {
        self.base_dir.join("config.fnl")
    }

    /// Load and parse the global Fennel config file.
    pub fn global_config(&self) -> Result<GlobalConfig> {
        let config_path = self.config_path();
        ensure!(
            config_path.exists(),
            "config not found: {}",
            config_path.display()
        );
        let fennel = Fennel::new()?;
        Ok(fennel.load_file(&config_path)?)
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

        let q = QuireCi::new(dir.path().to_path_buf());
        let config = q.global_config().expect("global_config should load");
        assert_eq!(config.port, 3000);
    }

    #[test]
    fn global_config_parses_custom_port() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.fnl");
        fs_err::write(&config_path, r#"{:port 4000}"#).expect("write");

        let q = QuireCi::new(dir.path().to_path_buf());
        let config = q.global_config().expect("global_config should load");
        assert_eq!(config.port, 4000);
    }

    #[test]
    fn global_config_missing_file_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let q = QuireCi::new(dir.path().to_path_buf());
        let err = q.global_config().unwrap_err();
        assert!(
            err.to_string().contains("config not found"),
            "expected config not found error, got {err:?}"
        );
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

        let q = QuireCi::new(dir.path().to_path_buf());
        let config = q.global_config().expect("global_config should load");
        let sentry = config.sentry.expect("sentry should be present");
        assert_eq!(sentry.dsn.reveal().unwrap(), "https://key@sentry.io/123");
    }
}
