//! Shell-out helpers for the docker subsystem.

use std::path::Path;

use crate::{Error, Result};

/// Returns true iff the docker daemon is reachable. Used to gate
/// integration tests; calls `docker info` and treats any failure
/// (binary missing, daemon down, permissions) as unavailable.
pub(crate) fn is_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build the per-run image. Runs `docker build --file <dockerfile>
/// --tag <tag> <context>`. Failures (including a missing Dockerfile)
/// surface as `Error::ImageBuildFailed` carrying docker's stderr.
pub(crate) fn docker_build(dockerfile: &Path, context: &Path, tag: &str) -> Result<()> {
    let output = std::process::Command::new("docker")
        .arg("build")
        .arg("--file")
        .arg(dockerfile)
        .arg("--tag")
        .arg(tag)
        .arg(context)
        .output()
        .map_err(|e| Error::ImageBuildFailed { source: e })?;
    if !output.status.success() {
        return Err(Error::ImageBuildFailed {
            source: std::io::Error::other(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires docker"]
    fn docker_build_succeeds_with_minimal_dockerfile() {
        if !is_available() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let context = dir.path();
        let dockerfile = context.join("Dockerfile");
        fs_err::write(&dockerfile, "FROM alpine:3.19\nRUN echo built\n")
            .expect("write Dockerfile");

        let tag = "quire-ci/test-task5:test";
        docker_build(&dockerfile, context, tag).expect("build should succeed");

        // Cleanup — best effort; ignore failures.
        let _ = std::process::Command::new("docker")
            .args(["image", "rm", tag])
            .output();
    }

    #[test]
    #[ignore = "requires docker"]
    fn docker_build_errors_on_bad_dockerfile() {
        if !is_available() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let context = dir.path();
        let dockerfile = context.join("Dockerfile");
        fs_err::write(&dockerfile, "GARBAGE\n").expect("write");

        let err = docker_build(&dockerfile, context, "quire-ci/test-task5-bad:test")
            .expect_err("should fail");
        assert!(matches!(err, Error::ImageBuildFailed { .. }));
    }
}
