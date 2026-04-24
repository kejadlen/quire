use std::path::PathBuf;

/// Application configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Root directory containing bare Git repositories.
    pub repos_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            repos_dir: PathBuf::from("/var/quire/repos"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_repos_dir() {
        let config = Config::default();
        assert_eq!(config.repos_dir, PathBuf::from("/var/quire/repos"));
    }
}
