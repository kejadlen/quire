use std::path::PathBuf;

/// Application configuration resolved from environment and defaults.
#[derive(Debug, Clone)]
pub struct Config {
    /// Root directory containing bare Git repositories.
    pub repos_dir: PathBuf,
}

impl Config {
    /// Environment variable overriding the default repository root.
    const REPOS_DIR_ENV: &'static str = "QUIRE_REPOS_DIR";

    /// Default repository root.
    const DEFAULT_REPOS_DIR: &'static str = "/var/quire/repos";

    /// Build config from environment, falling back to defaults.
    pub fn load() -> Self {
        let repos_dir = std::env::var(Self::REPOS_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(Self::DEFAULT_REPOS_DIR));

        Self { repos_dir }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_repos_dir() {
        let config = Config::load();
        assert_eq!(config.repos_dir, PathBuf::from(Config::DEFAULT_REPOS_DIR));
    }

    #[test]
    fn env_override_repos_dir() {
        // SAFETY: single-threaded test, no other code reading this env var concurrently.
        unsafe { std::env::set_var(Config::REPOS_DIR_ENV, "/tmp/test-repos") };
        let config = Config::load();
        // SAFETY: same justification.
        unsafe { std::env::remove_var(Config::REPOS_DIR_ENV) };
        assert_eq!(config.repos_dir, PathBuf::from("/tmp/test-repos"));
    }
}
