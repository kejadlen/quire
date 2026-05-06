//! Per-sh CRI log files for CI runs.
//!
//! Each `(sh ...)` call within a job produces a file at
//! `jobs/<job-id>/sh-<n>.log` in k8s CRI log format:
//!
//! ```text
//! <RFC3339 ts> <stream> <tag> <content>
//! ```
//!
//! Stream is `stdout` or `stderr`. Tag is `F` (full line).

use std::path::Path;

use super::runtime::ShOutput;

/// Write a sh output to a CRI log file.
///
/// Each line of stdout/stderr becomes one CRI-format line with the
/// given base timestamp, stream tag, and `F` (full) tag.
pub fn write_cri_log(path: &Path, output: &ShOutput, ts: &str) -> std::io::Result<()> {
    use std::io::Write;

    let mut f = std::fs::File::create(path)?;

    for line in output.stdout.lines() {
        writeln!(f, "{ts} stdout F {line}")?;
    }

    for line in output.stderr.lines() {
        writeln!(f, "{ts} stderr F {line}")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cri_log_splits_stdout_into_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sh-1.log");
        let output = ShOutput {
            exit: 0,
            stdout: "line one\nline two\n".to_string(),
            stderr: String::new(),
            cmd: "[\"echo\"]".to_string(),
        };

        write_cri_log(&path, &output, "2026-05-06T12:00:00Z").expect("write");

        let contents = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("stdout F line one"));
        assert!(lines[1].contains("stdout F line two"));
    }

    #[test]
    fn cri_log_handles_stderr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sh-1.log");
        let output = ShOutput {
            exit: 1,
            stdout: String::new(),
            stderr: "an error\n".to_string(),
            cmd: "[\"false\"]".to_string(),
        };

        write_cri_log(&path, &output, "2026-05-06T12:00:00Z").expect("write");

        let contents = std::fs::read_to_string(&path).expect("read");
        assert!(contents.contains("stderr F an error"));
    }

    #[test]
    fn cri_log_handles_empty_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sh-1.log");
        let output = ShOutput {
            exit: 0,
            stdout: String::new(),
            stderr: String::new(),
            cmd: "true".to_string(),
        };

        write_cri_log(&path, &output, "2026-05-06T12:00:00Z").expect("write");

        let contents = std::fs::read_to_string(&path).expect("read");
        assert!(contents.is_empty());
    }
}
