use miette::{Result, ensure};

/// Validate a repository name for creation.
///
/// Allows at most one level of grouping (e.g. `foo.git` or `work/foo.git`).
/// Rejects path traversal, missing `.git` suffix, empty segments, and
/// reserved path components.
pub fn validate_name(name: &str) -> Result<String> {
    ensure!(!name.is_empty(), "repository name cannot be empty");
    ensure!(!name.contains(".."), "invalid repository name: {name}");
    ensure!(
        name.ends_with(".git"),
        "repository name must end in .git: {name}"
    );
    ensure!(!name.contains("//"), "invalid repository name: {name}");

    let segments = name.split('/').collect::<Vec<_>>();
    ensure!(
        segments.len() <= 2,
        "repository name allows at most one level of grouping: {name}"
    );

    for seg in &segments {
        ensure!(!seg.is_empty(), "invalid repository name: {name}");
        ensure!(
            *seg != "." && *seg != ".." && *seg != ".git",
            "invalid repository name: {name}"
        );
    }

    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        assert_eq!(validate_name("foo.git").unwrap(), "foo.git");
        assert_eq!(validate_name("work/foo.git").unwrap(), "work/foo.git");
    }

    #[test]
    fn rejects_empty() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn rejects_traversal() {
        assert!(validate_name("../foo.git").is_err());
        assert!(validate_name("foo/../../bar.git").is_err());
        assert!(validate_name("./foo.git").is_err());
    }

    #[test]
    fn rejects_no_git_suffix() {
        assert!(validate_name("foo").is_err());
    }

    #[test]
    fn rejects_deep_nesting() {
        assert!(validate_name("a/b/c.git").is_err());
    }

    #[test]
    fn rejects_double_slash() {
        assert!(validate_name("foo//bar.git").is_err());
    }

    #[test]
    fn rejects_empty_segment() {
        assert!(validate_name("/foo.git").is_err());
        assert!(validate_name("foo//bar.git").is_err());
    }
}
