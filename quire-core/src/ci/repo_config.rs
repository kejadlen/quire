//! Per-repo CI configuration parsed from `.quire/config.fnl`.
//!
//! Loaded at run time (inside `quire-ci`) from the materialized
//! workspace. Missing file → all defaults.

/// Per-repo CI configuration.
#[derive(serde::Deserialize, Debug, Default, Clone)]
#[serde(default, rename_all = "kebab-case")]
pub struct RepoConfig {
    pub github: GithubRepoConfig,
}

/// Per-repo GitHub configuration.
#[derive(serde::Deserialize, Debug, Default, Clone)]
#[serde(default, rename_all = "kebab-case")]
pub struct GithubRepoConfig {
    /// Remote URL to mirror every pushed ref to.
    /// E.g. `"https://github.com/user/repo.git"`.
    pub mirror: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(source: &str) -> RepoConfig {
        crate::fennel::Fennel::new()
            .expect("Fennel::new")
            .load_string(source, "config.fnl")
            .expect("load_string")
    }

    #[test]
    fn defaults_when_empty_table() {
        let cfg = load("{}");
        assert!(cfg.github.mirror.is_none());
    }

    #[test]
    fn parses_mirror_url() {
        let cfg = load(r#"{:github {:mirror "https://github.com/user/repo.git"}}"#);
        assert_eq!(
            cfg.github.mirror.as_deref(),
            Some("https://github.com/user/repo.git")
        );
    }
}
