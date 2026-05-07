//! `(ci.mirror url opts)`: registers the singleton `quire/mirror`
//! job, whose Rust run-fn tags the pushed commit and `git push`es
//! the configured refs (or the trigger ref) plus the tag.
//!
//! `:refs` serves double duty: it gates whether the mirror job runs
//! at all (trigger filter) and controls what gets pushed (push
//! filter). If the trigger ref is not in `:refs`, the job is a
//! no-op — no tag is created, no push is attempted. When `:refs` is
//! empty (the default), the trigger ref is used for the push and
//! the mirror always runs.
//!
//! Lives at the ci-feature layer rather than under `lua/` because
//! mirror is a CI capability that happens to be exposed via the
//! Lua DSL — most of its body is git plumbing, and it produces a
//! `RunFn::Rust` rather than running through the Lua callback path.

use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Lua, LuaSerdeExt};

use super::error::{Error, Result};
use super::pipeline::{self, DefinitionError, Job, RunFn};
use super::registration::Registration;
use super::runtime::{Cmd, Runtime, ShOpts};

/// Closure state for the `quire/mirror` job's run-fn: everything the
/// tag-and-push needs at execute time, captured once at registration.
pub(super) struct MirrorJob {
    url: String,
    secret: String,
    /// Refs to push to the remote. Empty means "push whatever ref
    /// triggered the run."
    refs: Vec<String>,
    /// Tag callback. Called at execute time with the push table to
    /// produce the tag name; the helper then tags `push.sha` and
    /// pushes that tag alongside the refs.
    tag: mlua::Function,
}

impl MirrorJob {
    /// Run the tag-and-push against the bare git dir from the runtime's
    /// `quire/push` data. Side effects only — outputs are recorded
    /// against the calling job via the sh-capture channel. Returns
    /// `Ok(())` whether or not the remote push succeeded; non-zero
    /// `git push` exit lands in the run log alongside any other shell
    /// output. Returns `Err` only for setup failures (unknown secret,
    /// failed tag, base64 spawn).
    fn execute(&self, rt: &Runtime) -> Result<()> {
        let calling = rt.current_job.borrow();
        let calling = calling
            .as_ref()
            .expect("mirror run-fn invoked without an active job");

        // Pull push data from this job's inputs view. Reachability is
        // a structural fact established at registration; the unwraps
        // are program invariants, not user-reachable conditions.
        let view = rt
            .inputs
            .get(calling)
            .unwrap_or_else(|| unreachable!("no inputs view for calling job '{calling}'"));
        let push_table = view
            .get("quire/push")
            .and_then(|v| v.as_ref())
            .and_then(|v| v.as_table())
            .expect("quire/push table absent from quire/mirror inputs view");
        let sha: String = push_table.get("sha")?;
        let pushed_ref: String = push_table.get("ref")?;

        // Gate: if :refs is set, only run when the trigger ref matches.
        if !self.refs.is_empty() && !self.refs.contains(&pushed_ref) {
            tracing::info!(
                ref_name = %pushed_ref,
                "skipping mirror — trigger ref not in :refs"
            );
            return Ok(());
        };
        let git_dir: String = push_table.get("git-dir")?;

        let secret = rt.secret(&self.secret)?;

        let git_opts = ShOpts {
            env: HashMap::from([("GIT_DIR".to_string(), git_dir)]),
        };

        // Tag step.
        let tag_name: String = self.tag.call(push_table.clone())?;
        let tag_result = rt.sh(
            Cmd::Argv {
                program: "git".to_string(),
                args: vec!["tag".to_string(), tag_name.clone(), sha],
            },
            git_opts.clone(),
        )?;
        if tag_result.exit != 0 {
            return Err(Error::Git(format!(
                "git tag failed: {}",
                tag_result.stderr.trim()
            )));
        }

        // Build the auth header. printf-into-base64 keeps the secret
        // out of the argv (visible in `ps`); piping via $T is the
        // smallest stdin-free alternative.
        //
        // Run via `Cmd::run` rather than `rt.sh` — we don't want the
        // encoded token landing in recorded outputs.
        let token_pair = format!("x-access-token:{secret}");
        let encoded_output = Cmd::Shell("printf '%s' \"$T\" | base64 --wrap=0".to_string()).run(
            ShOpts {
                env: HashMap::from([("T".to_string(), token_pair)]),
            },
            rt.workspace(),
        )?;
        let auth_header = format!("Authorization: Basic {}", encoded_output.stdout.trim());

        // Push the configured refs (or the trigger ref, if none) plus the tag.
        let mut push_args = vec![
            "-c".to_string(),
            format!("http.extraHeader={auth_header}"),
            "push".to_string(),
            "--porcelain".to_string(),
            self.url.clone(),
        ];
        if self.refs.is_empty() {
            push_args.push(pushed_ref);
        } else {
            push_args.extend(self.refs.iter().cloned());
        }
        push_args.push(format!("refs/tags/{tag_name}"));
        rt.sh(
            Cmd::Argv {
                program: "git".to_string(),
                args: push_args,
            },
            git_opts,
        )?;

        Ok(())
    }

    /// Parse `(ci.mirror url opts)` into a `MirrorJob` and the
    /// `:after` list. `:after` only affects sequencing (extra inputs
    /// on the registered job), so it stays out of the closure state.
    ///
    /// `:tag` is extracted manually since `mlua::Function` isn't
    /// serde-deserializable; the rest go through `lua.from_value`
    /// with `deny_unknown_fields` so typos surface as registration
    /// errors.
    ///
    /// Errors are returned as `mlua::Error::external` so callers can
    /// render them via `Display` into a
    /// `DefinitionError::InvalidMirrorCall` at the call site.
    fn parse(lua: &Lua, url: String, opts: mlua::Table) -> mlua::Result<(Self, Vec<String>)> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            secret: String,
            #[serde(default)]
            refs: Vec<String>,
            #[serde(default)]
            after: Vec<String>,
        }

        // Pull :tag separately — it's a Lua function, not deserializable.
        let tag: mlua::Function = match opts.get::<mlua::Value>("tag")? {
            mlua::Value::Function(f) => f,
            mlua::Value::Nil => {
                return Err(mlua::Error::external(
                    ":tag is required (a function returning the tag name)",
                ));
            }
            other => {
                return Err(mlua::Error::external(format!(
                    ":tag must be a function, got {}",
                    other.type_name()
                )));
            }
        };

        // Build a copy of the opts table without :tag so
        // `deny_unknown_fields` doesn't trip on it.
        let stripped = lua.create_table()?;
        for pair in opts.pairs::<String, mlua::Value>() {
            let (k, v) = pair?;
            if k != "tag" {
                stripped.set(k, v)?;
            }
        }

        let fields: Fields = lua.from_value(mlua::Value::Table(stripped))?;

        Ok((
            Self {
                url,
                secret: fields.secret,
                refs: fields.refs,
                tag,
            },
            fields.after,
        ))
    }

    /// Body of `(ci.mirror url opts)`. Parses opts and registers an
    /// internal job at `quire/mirror` whose run-fn performs the
    /// tag-and-push at execute time. Singleton-ness is enforced by
    /// generic id uniqueness in `Registration::add_job` — a second
    /// `(ci.mirror …)` collides on the `quire/mirror` id.
    pub(super) fn register(lua: &Lua, (url, opts): (String, mlua::Table)) -> mlua::Result<()> {
        let r = lua.app_data_ref::<Registration>().ok_or_else(|| {
            mlua::Error::external("quire.ci registration not installed on Lua VM")
        })?;
        let line = lua
            .inspect_stack(1, |d| d.current_line())
            .flatten()
            .map(|l| l as u32)
            .unwrap_or(0);

        let (job, after) = match Self::parse(lua, url, opts) {
            Ok(parsed) => parsed,
            Err(e) => {
                let span = pipeline::span_for_line(&r.source, line);
                r.errors
                    .borrow_mut()
                    .push(DefinitionError::InvalidMirrorCall {
                        message: e.to_string(),
                        span,
                    });
                return Ok(());
            }
        };

        let run_fn = RunFn::Rust(Rc::new(move |rt: &Runtime| job.execute(rt)));

        // Inputs: always quire/push first (the push data source), then
        // any extra dependencies from :after for sequencing.
        let mut inputs = vec!["quire/push".to_string()];
        inputs.extend(after);

        match Job::new("quire/mirror".to_string(), inputs, run_fn, line, &r.source) {
            Ok(job) => r.add_job(job, line),
            Err(e) => r.errors.borrow_mut().push(e),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::IntoLua;

    use crate::ci::pipeline::{Diagnostic, RustRunFn, compile};
    use crate::ci::run::RunMeta;
    use crate::ci::runtime::{ExecutorRuntime, RuntimeHandle};
    use crate::secret::SecretString;

    /// Set up a bare git repo with one commit. Returns the tempdir,
    /// the bare repo path, and the head SHA.
    fn bare_repo() -> (tempfile::TempDir, std::path::PathBuf, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let work = dir.path().join("work");
        let bare = dir.path().join("repo.git");

        fs_err::create_dir_all(&work).expect("mkdir work");
        let env_vars: [(&str, &str); 6] = [
            ("GIT_AUTHOR_NAME", "test"),
            ("GIT_AUTHOR_EMAIL", "test@test"),
            ("GIT_COMMITTER_NAME", "test"),
            ("GIT_COMMITTER_EMAIL", "test@test"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ];
        let output = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&work)
            .envs(env_vars)
            .output()
            .expect("git init");
        assert!(output.status.success());

        let output = std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "initial"])
            .current_dir(&work)
            .envs(env_vars)
            .output()
            .expect("git commit");
        assert!(output.status.success());

        let output = std::process::Command::new("git")
            .args([
                "clone",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ])
            .current_dir(dir.path())
            .output()
            .expect("git clone --bare");
        assert!(output.status.success());

        let sha_output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&bare)
            .output()
            .expect("git rev-parse");
        let sha = String::from_utf8(sha_output.stdout)
            .expect("utf8")
            .trim()
            .to_string();

        (dir, bare, sha)
    }

    /// Pull the mirror job out of a compiled pipeline. Panics if no
    /// `quire/mirror` job is present.
    fn mirror_job_inputs(source: &str) -> Vec<String> {
        let pipeline = compile(source, "ci.fnl").expect("compile should succeed");
        pipeline
            .jobs()
            .iter()
            .find(|j| j.id == "quire/mirror")
            .expect("mirror job should be registered")
            .inputs
            .clone()
    }

    #[test]
    fn mirror_registers_quire_mirror_job_with_push_input() {
        let inputs = mirror_job_inputs(
            r#"(local ci (require :quire.ci))
(ci.mirror "https://github.com/example/repo.git"
  {:secret :github_token :tag (fn [_] "v1")})"#,
        );
        assert_eq!(inputs, vec!["quire/push".to_string()]);
    }

    #[test]
    fn mirror_after_appends_to_inputs() {
        let source = r#"(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [_] nil))
(ci.mirror "https://github.com/example/repo.git"
  {:secret :github_token :tag (fn [_] "v1") :after [:build]})"#;
        let inputs = mirror_job_inputs(source);
        assert_eq!(inputs, vec!["quire/push".to_string(), "build".to_string()]);
    }

    #[test]
    fn mirror_duplicate_call_errors_via_id_collision() {
        let source = r#"(local ci (require :quire.ci))
(ci.mirror "https://github.com/example/repo.git" {:secret :a :tag (fn [_] "v1")})
(ci.mirror "https://github.com/example/other.git" {:secret :b :tag (fn [_] "v1")})"#;
        let Err(err) = compile(source, "ci.fnl") else {
            panic!("expected error");
        };
        let crate::ci::error::Error::Pipeline(pe) = err else {
            panic!("expected PipelineError, got {err:?}");
        };
        assert!(
            pe.diagnostics.iter().any(|d| matches!(
                d,
                Diagnostic::Definition(DefinitionError::DuplicateJob { job_id, .. })
                    if job_id == "quire/mirror"
            )),
            "expected DuplicateJob('quire/mirror') in: {:?}",
            pe.diagnostics
        );
    }

    /// Compile, expect a single `InvalidMirrorCall` diagnostic, return its message.
    fn invalid_mirror_message(source: &str) -> String {
        let Err(err) = compile(source, "ci.fnl") else {
            panic!("expected error");
        };
        let crate::ci::error::Error::Pipeline(pe) = err else {
            panic!("expected PipelineError, got {err:?}");
        };
        pe.diagnostics
            .iter()
            .find_map(|d| match d {
                Diagnostic::Definition(DefinitionError::InvalidMirrorCall { message, .. }) => {
                    Some(message.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected InvalidMirrorCall in: {:?}", pe.diagnostics))
    }

    #[test]
    fn mirror_unknown_opt_key_errors() {
        let msg = invalid_mirror_message(
            r#"(local ci (require :quire.ci))
(ci.mirror "https://github.com/example/repo.git"
  {:secret :a :tag (fn [_] "v1") :tagPrefix "v"})"#,
        );
        assert!(
            msg.contains("tagPrefix") && msg.contains("unknown field"),
            "expected unknown-field error mentioning the typo, got: {msg}"
        );
    }

    #[test]
    fn mirror_requires_secret() {
        let msg = invalid_mirror_message(
            r#"(local ci (require :quire.ci))
(ci.mirror "https://github.com/example/repo.git" {:tag (fn [_] "v1")})"#,
        );
        assert!(
            msg.contains("missing field") && msg.contains("secret"),
            "expected missing-secret error, got: {msg}"
        );
    }

    #[test]
    fn mirror_requires_tag() {
        let msg = invalid_mirror_message(
            r#"(local ci (require :quire.ci))
(ci.mirror "https://github.com/example/repo.git" {:secret :a})"#,
        );
        assert!(
            msg.contains(":tag is required"),
            "expected missing-tag error, got: {msg}"
        );
    }

    #[test]
    fn mirror_tag_must_be_function() {
        let msg = invalid_mirror_message(
            r#"(local ci (require :quire.ci))
(ci.mirror "https://github.com/example/repo.git" {:secret :a :tag "v1"})"#,
        );
        assert!(
            msg.contains("must be a function"),
            "expected tag-shape error, got: {msg}"
        );
    }

    /// Compile a mirror source and return the registered Rust
    /// run-fn ready to be invoked with a runtime that has
    /// `:quire/push` populated.
    fn mirror_run_fn(
        source: &str,
        secrets: HashMap<String, SecretString>,
        meta: &RunMeta,
        git_dir: &std::path::Path,
    ) -> (Rc<Runtime>, RustRunFn) {
        let pipeline = compile(source, "ci.fnl").expect("compile should succeed");
        let run_fn = match pipeline
            .jobs()
            .iter()
            .find(|j| j.id == "quire/mirror")
            .expect("mirror job should exist")
            .run_fn
            .clone()
        {
            RunFn::Rust(f) => f,
            RunFn::Lua(_) => panic!("mirror should register a RunFn::Rust"),
        };
        let runtime = Rc::new(Runtime::new(
            pipeline,
            secrets,
            meta,
            git_dir,
            std::env::current_dir().expect("cwd"),
            ExecutorRuntime::Host,
        ));
        let _ = RuntimeHandle(runtime.clone())
            .into_lua(runtime.lua())
            .expect("install runtime");
        (runtime, run_fn)
    }

    #[test]
    fn mirror_executes_tag_callback_and_pushes() {
        let (_dir, bare, sha) = bare_repo();
        let pushed_at: jiff::Timestamp = "2026-05-01T12:00:00Z".parse().unwrap();
        let meta = RunMeta {
            sha: sha.clone(),
            r#ref: "refs/heads/main".to_string(),
            pushed_at,
        };

        let mut secrets = HashMap::new();
        secrets.insert(
            "github_token".to_string(),
            SecretString::from_plain("fake_token"),
        );

        let source = r#"(local ci (require :quire.ci))
(ci.mirror "https://github.com/example/repo.git"
  {:secret :github_token
   :tag (fn [push] (.. "release-" (string.sub push.sha 1 8)))})"#;
        let (runtime, run_fn) = mirror_run_fn(source, secrets, &meta, &bare);

        runtime.enter_job("quire/mirror");
        run_fn(&runtime).expect("mirror should succeed");
        runtime.leave_job();

        // Tag was created in the bare repo with the callback's name.
        let expected_tag = format!("release-{}", &sha[..8]);
        let tag_output = std::process::Command::new("git")
            .args(["tag", "-l"])
            .current_dir(&bare)
            .output()
            .expect("git tag -l");
        let tags = String::from_utf8(tag_output.stdout).expect("utf8");
        assert!(
            tags.contains(&expected_tag),
            "tag should exist in bare repo: {tags}"
        );

        // Outputs were recorded for the tag step and the push step
        // (push to a fake URL fails non-zero, not via Err).
        let outputs = runtime.take_outputs();
        let recorded = outputs
            .get("quire/mirror")
            .expect("mirror outputs recorded");
        assert_eq!(recorded.len(), 2, "expected tag + push outputs");
        let push = recorded.last().unwrap();
        assert_ne!(push.exit, 0, "push to fake URL should fail");
    }

    #[test]
    fn mirror_pushes_listed_refs_when_trigger_ref_matches() {
        let (_dir, bare, sha) = bare_repo();
        let pushed_at: jiff::Timestamp = "2026-05-01T12:00:00Z".parse().unwrap();
        let meta = RunMeta {
            sha,
            r#ref: "refs/heads/main".to_string(),
            pushed_at,
        };

        let mut secrets = HashMap::new();
        secrets.insert(
            "github_token".to_string(),
            SecretString::from_plain("fake_token"),
        );

        // :refs is set and the trigger ref matches, so the mirror
        // should push the listed refs verbatim.
        let source = r#"(local ci (require :quire.ci))
(ci.mirror "https://github.com/example/repo.git"
  {:secret :github_token
   :tag (fn [_] "v1")
   :refs ["refs/heads/main" "refs/heads/release"]})"#;
        let (runtime, run_fn) = mirror_run_fn(source, secrets, &meta, &bare);

        runtime.enter_job("quire/mirror");
        run_fn(&runtime).expect("mirror should succeed");
        runtime.leave_job();

        let outputs = runtime.take_outputs();
        let recorded = outputs.get("quire/mirror").expect("recorded");
        // Tag step records first; push step second.
        let push = recorded.last().expect("push output");
        let cmd = &push.cmd;
        assert!(
            cmd.contains("refs/heads/main") && cmd.contains("refs/heads/release"),
            "push cmd should list configured refs, got: {cmd}"
        );
    }

    #[test]
    fn mirror_skips_when_trigger_ref_not_in_refs() {
        let (_dir, bare, _sha) = bare_repo();
        let pushed_at: jiff::Timestamp = "2026-05-01T12:00:00Z".parse().unwrap();
        let meta = RunMeta {
            sha: "abc123".to_string(),
            r#ref: "refs/heads/feature".to_string(),
            pushed_at,
        };

        let mut secrets = HashMap::new();
        secrets.insert(
            "github_token".to_string(),
            SecretString::from_plain("fake_token"),
        );

        // Trigger ref is feature, but :refs only lists main — mirror
        // should be a no-op.
        let source = r#"(local ci (require :quire.ci))
(ci.mirror "https://github.com/example/repo.git"
  {:secret :github_token
   :tag (fn [_] "v1")
   :refs ["refs/heads/main"]})"#;
        let (runtime, run_fn) = mirror_run_fn(source, secrets, &meta, &bare);

        runtime.enter_job("quire/mirror");
        run_fn(&runtime).expect("mirror should succeed (no-op)");
        runtime.leave_job();

        let outputs = runtime.take_outputs();
        assert_eq!(
            outputs.get("quire/mirror").map(|v| v.len()).unwrap_or(0),
            0,
            "no outputs should be recorded for a skipped mirror"
        );

        // No tag should have been created.
        let tag_output = std::process::Command::new("git")
            .args(["tag", "-l"])
            .current_dir(&bare)
            .output()
            .expect("git tag -l");
        let tags = String::from_utf8(tag_output.stdout).expect("utf8");
        assert!(tags.trim().is_empty(), "no tags should exist: {tags}");
    }

    #[test]
    fn mirror_errors_for_unknown_secret_at_runtime() {
        let (_dir, bare, sha) = bare_repo();
        let pushed_at: jiff::Timestamp = "2026-05-01T12:00:00Z".parse().unwrap();
        let meta = RunMeta {
            sha,
            r#ref: "refs/heads/main".to_string(),
            pushed_at,
        };

        let source = r#"(local ci (require :quire.ci))
(ci.mirror "https://github.com/example/repo.git"
  {:secret :missing :tag (fn [_] "v1")})"#;
        let (runtime, run_fn) = mirror_run_fn(source, HashMap::new(), &meta, &bare);

        runtime.enter_job("quire/mirror");
        let err = run_fn(&runtime).expect_err("should fail for missing secret");
        runtime.leave_job();

        assert!(
            matches!(err, Error::UnknownSecret(ref name) if name == "missing"),
            "expected UnknownSecret(\"missing\"), got: {err:?}"
        );
    }
}
