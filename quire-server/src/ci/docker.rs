//! Shell-out helpers for the docker subsystem.

use std::path::Path;

use super::error::{Error, Result};

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
            source: std::io::Error::other(String::from_utf8_lossy(&output.stderr).into_owned()),
        });
    }
    Ok(())
}

/// A running container owned by a quire run. Started via [`start`];
/// stopped via [`Drop`]. The `--rm` flag on `docker run` removes the
/// container record once stop completes, so callers don't need to
/// manage `docker rm` separately.
///
/// `Drop` swallows errors from `docker stop` because `Drop` cannot
/// return `Result`. Failures are logged via `tracing::error!`; orphan
/// reconciliation handles anything that survives.
pub(crate) struct ContainerSession {
    pub(crate) container_id: String,
    pub(crate) container_started_at: jiff::Timestamp,
}

impl ContainerSession {
    /// Start a long-lived container from `image_tag` with `mount_source`
    /// bind-mounted at `mount_target` inside the container, with
    /// `mount_target` as the working directory. Captures the container
    /// ID. Failures surface as `Error::ContainerStartFailed`.
    pub(crate) fn start(image_tag: &str, mount_source: &Path, mount_target: &str) -> Result<Self> {
        let mount = format!(
            "type=bind,src={},dst={}",
            mount_source.to_string_lossy(),
            mount_target,
        );
        let output = std::process::Command::new("docker")
            .args(["run", "--detach", "--rm", "--mount"])
            .arg(&mount)
            // 15-minute ceiling. Real runs end when
            // `ContainerSession::drop` calls `docker stop`; this is
            // the safety net for cases where Drop never fires (orphaned
            // process, killed parent).
            .args(["--workdir", mount_target, image_tag, "sleep", "15m"])
            .output()
            .map_err(|e| Error::ContainerStartFailed { source: e })?;
        if !output.status.success() {
            return Err(Error::ContainerStartFailed {
                source: std::io::Error::other(String::from_utf8_lossy(&output.stderr).into_owned()),
            });
        }
        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(Self {
            container_id,
            container_started_at: jiff::Timestamp::now(),
        })
    }
}

impl Drop for ContainerSession {
    fn drop(&mut self) {
        let result = std::process::Command::new("docker")
            .args(["stop", "--time", "5"])
            .arg(&self.container_id)
            .output();
        match result {
            Ok(out) if out.status.success() => {}
            Ok(out) => tracing::error!(
                container_id = %self.container_id,
                stderr = %String::from_utf8_lossy(&out.stderr),
                "docker stop returned non-zero"
            ),
            Err(e) => tracing::error!(
                container_id = %self.container_id,
                error = %e,
                "docker stop failed"
            ),
        }
    }
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
        fs_err::write(&dockerfile, "FROM alpine:3.19\nRUN echo built\n").expect("write Dockerfile");

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

    #[test]
    #[ignore = "requires docker"]
    fn container_session_start_returns_id() {
        if !is_available() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");

        let session = ContainerSession::start("alpine:3.19", dir.path(), "/work")
            .expect("start should succeed");

        assert!(!session.container_id.is_empty());
        // Docker container IDs from `docker run` are 64-char SHA256 hex.
        assert_eq!(
            session.container_id.len(),
            64,
            "got: {}",
            session.container_id
        );
        // session drops here; docker stop runs.
    }

    #[test]
    #[ignore = "requires docker"]
    fn container_session_drop_stops_container() {
        if !is_available() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let id = {
            let session = ContainerSession::start("alpine:3.19", dir.path(), "/work")
                .expect("start should succeed");
            session.container_id.clone()
        }; // session drops here

        // Give the daemon a moment to settle the stop.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let out = std::process::Command::new("docker")
            .args(["ps", "-a", "--quiet", "--filter"])
            .arg(format!("id={id}"))
            .output()
            .expect("docker ps");
        assert!(
            String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            "container should be removed after drop, but ps shows: {}",
            String::from_utf8_lossy(&out.stdout),
        );
    }
}
