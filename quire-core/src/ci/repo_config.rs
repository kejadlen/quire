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
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(default, rename_all = "kebab-case")]
pub struct GithubRepoConfig {
    /// Remote URL to mirror to on every push to `:branch`.
    /// E.g. `"https://github.com/user/repo.git"`.
    /// When set, quire injects a `quire/mirror` built-in job into
    /// every pipeline for this repo.
    pub mirror: Option<String>,
    /// Ref that triggers the mirror (default: `refs/heads/main`).
    /// Pushes to any other ref are ignored by the mirror job.
    pub branch: String,
}

impl Default for GithubRepoConfig {
    fn default() -> Self {
        Self {
            mirror: None,
            branch: "refs/heads/main".to_string(),
        }
    }
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
        assert_eq!(cfg.github.branch, "refs/heads/main");
    }

    #[test]
    fn parses_mirror_url() {
        let cfg = load(r#"{:github {:mirror "https://github.com/user/repo.git"}}"#);
        assert_eq!(
            cfg.github.mirror.as_deref(),
            Some("https://github.com/user/repo.git")
        );
    }

    #[test]
    fn parses_custom_branch() {
        let cfg = load(
            r#"{:github {:mirror "https://github.com/u/r.git" :branch "refs/heads/release"}}"#,
        );
        assert_eq!(cfg.github.branch, "refs/heads/release");
    }

    #[test]
    fn default_branch_when_only_mirror_set() {
        let cfg = load(r#"{:github {:mirror "https://github.com/u/r.git"}}"#);
        assert_eq!(cfg.github.branch, "refs/heads/main");
    }
}
