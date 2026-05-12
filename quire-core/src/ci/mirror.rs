//! `(ci.mirror url opts)`: registers the singleton `quire/mirror`
//! job, whose Lua run-fn delegates to `(require :quire.stdlib).mirror`
//! for the tag-and-push at execute time.
//!
//! This shim handles the `ci.mirror`-specific concerns: resolving
//! the `:secret` name into an auth header, invoking the `:tag`
//! callback, and gating on `:refs`. The actual git plumbing lives
//! in `stdlib.fnl`.

use std::rc::Rc;

use mlua::{Lua, LuaSerdeExt};

use super::pipeline::{self, DefinitionError, Job, RunFn};
use super::registration::Registration;

/// Body of `(ci.mirror url opts)`. Validates opts and registers an
/// internal job at `quire/mirror` whose Lua run-fn delegates to
/// `stdlib.mirror` at execute time. Singleton-ness is enforced by
/// generic id uniqueness in `Registration::add_job`.
pub fn register(lua: &Lua, (url, opts): (String, mlua::Table)) -> mlua::Result<()> {
    let r = lua
        .app_data_ref::<Registration>()
        .ok_or_else(|| mlua::Error::external("quire.ci registration not installed on Lua VM"))?;
    let line = lua
        .inspect_stack(1, |d| d.current_line())
        .flatten()
        .map(|l| l as u32)
        .unwrap_or(0);
    let span = || pipeline::span_for_line(&r.source, line);
    let invalid = |msg: String, s: _| DefinitionError::InvalidMirrorCall {
        message: msg,
        span: s,
    };

    // :tag — required function.
    let tag: mlua::Function = match opts.get::<mlua::Value>("tag")? {
        mlua::Value::Function(f) => f,
        mlua::Value::Nil => {
            r.errors.borrow_mut().push(invalid(
                ":tag is required (a function returning the tag name)".into(),
                span(),
            ));
            return Ok(());
        }
        other => {
            r.errors.borrow_mut().push(invalid(
                format!(":tag must be a function, got {}", other.type_name()),
                span(),
            ));
            return Ok(());
        }
    };

    // :secret — required string.
    let secret: String = match opts.get::<mlua::Value>("secret")? {
        mlua::Value::String(s) => s.to_str()?.to_string(),
        mlua::Value::Nil => {
            r.errors
                .borrow_mut()
                .push(invalid("missing field `secret`".into(), span()));
            return Ok(());
        }
        other => {
            r.errors.borrow_mut().push(invalid(
                format!(":secret must be a string, got {}", other.type_name()),
                span(),
            ));
            return Ok(());
        }
    };

    // :refs, :after — optional string lists.
    let refs: Vec<String> = opts.get::<Option<Vec<String>>>("refs")?.unwrap_or_default();
    let after: Vec<String> = opts
        .get::<Option<Vec<String>>>("after")?
        .unwrap_or_default();

    // Reject unknown keys.
    for pair in opts.pairs::<String, mlua::Value>() {
        let (k, _) = pair?;
        if !matches!(k.as_str(), "tag" | "secret" | "refs" | "after") {
            r.errors
                .borrow_mut()
                .push(invalid(format!("unknown field `{k}`"), span()));
            return Ok(());
        }
    }

    // Build the Lua run-fn. Closes over registration-time values;
    // at execute time the ambient runtime provides push data and
    // secret resolution.
    let url = Rc::new(url);
    let secret = Rc::new(secret);
    let refs = Rc::new(refs);

    let run_fn = lua.create_function(move |lua, ()| {
        let loaded: mlua::Table = lua
            .globals()
            .get::<mlua::Table>("package")?
            .get::<mlua::Table>("loaded")?;
        let mirror: mlua::Function = loaded.get::<mlua::Table>("quire.stdlib")?.get("mirror")?;

        let runtime: mlua::Table = loaded.get::<mlua::Table>("quire.ci")?.get("runtime")?;
        let push: mlua::Table = runtime.get::<mlua::Function>("jobs")?.call("quire/push")?;
        let pushed_ref: String = push.get("ref")?;

        // Gate: skip if :refs is set and trigger ref doesn't match.
        if !refs.is_empty() && !refs.iter().any(|r| r == &pushed_ref) {
            return Ok(mlua::Value::Nil);
        }

        let tag_name: String = tag.call(push.clone())?;
        let auth_header: String = runtime
            .get::<mlua::Function>("secret")?
            .call(secret.as_str())?;

        let push_refs = if refs.is_empty() {
            vec![pushed_ref]
        } else {
            refs.to_vec()
        };

        let mopts = lua.create_table()?;
        mopts.set("url", url.as_str())?;
        mopts.set("auth-header", auth_header)?;
        mopts.set("sha", push.get::<String>("sha")?)?;
        mopts.set("tag", tag_name)?;
        mopts.set("git-dir", push.get::<String>("git-dir")?)?;
        mopts.set("refs", lua.to_value(&push_refs)?)?;

        mirror.call(mopts)
    })?;

    let mut inputs = vec!["quire/push".to_string()];
    inputs.extend(after);
    match Job::new(
        "quire/mirror".into(),
        inputs,
        RunFn::Lua(run_fn),
        line,
        &r.source,
    ) {
        Ok(job) => r.add_job(job, line),
        Err(e) => r.errors.borrow_mut().push(e),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use crate::ci::pipeline::{Diagnostic, compile};
    use crate::ci::run::RunMeta;
    use crate::ci::runtime::RuntimeHandle;
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

    /// Build a source bare repo with one commit and an empty target
    /// bare repo. Returns (tempdir, source bare, target bare, sha).
    fn bare_repo_with_target() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        String,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let work = dir.path().join("work");
        let bare = dir.path().join("repo.git");
        let target = dir.path().join("target.git");

        fs_err::create_dir_all(&work).expect("mkdir work");
        let env_vars: [(&str, &str); 6] = [
            ("GIT_AUTHOR_NAME", "test"),
            ("GIT_AUTHOR_EMAIL", "test@test"),
            ("GIT_COMMITTER_NAME", "test"),
            ("GIT_COMMITTER_EMAIL", "test@test"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ];
        let git = |args: &[&str], cwd: &std::path::Path| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .envs(env_vars)
                .output()
                .expect("git");
            assert!(out.status.success(), "git {:?} failed", args);
            out
        };
        git(&["init", "-b", "main"], &work);
        git(&["commit", "--allow-empty", "-m", "initial"], &work);
        let sha = String::from_utf8(git(&["rev-parse", "HEAD"], &work).stdout)
            .expect("utf8")
            .trim()
            .to_string();
        git(
            &[
                "clone",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
            dir.path(),
        );
        git(&["init", "--bare", target.to_str().unwrap()], dir.path());

        (dir, bare, target, sha)
    }

    /// Pull the mirror job's inputs from a compiled pipeline.
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
(ci.job :build [:quire/push] (fn [] nil))
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
        let crate::ci::pipeline::CompileError::Pipeline(pe) = err else {
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
        let crate::ci::pipeline::CompileError::Pipeline(pe) = err else {
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

    /// Compile a mirror source and return the runtime and the mirror
    /// job's Lua run-fn ready to be invoked with the ambient runtime
    /// installed.
    fn mirror_run_fn(
        source: &str,
        secrets: HashMap<String, SecretString>,
        meta: &RunMeta,
        git_dir: &std::path::Path,
    ) -> (Rc<crate::ci::runtime::Runtime>, mlua::Function) {
        use crate::ci::runtime::Runtime;
        let pipeline = compile(source, "ci.fnl").expect("compile should succeed");
        let run_fn = match pipeline
            .jobs()
            .iter()
            .find(|j| j.id == "quire/mirror")
            .expect("mirror job should exist")
            .run_fn
            .clone()
        {
            RunFn::Lua(f) => f,
            RunFn::Rust(_) => panic!("mirror should register a RunFn::Lua"),
        };
        let log_dir = tempfile::tempdir().expect("tempdir for mirror logs").keep();
        let runtime = Rc::new(Runtime::new(
            pipeline,
            secrets,
            meta,
            git_dir,
            std::env::current_dir().expect("cwd"),
            log_dir,
        ));
        RuntimeHandle(runtime.clone())
            .install(runtime.lua())
            .expect("install runtime");
        (runtime, run_fn)
    }

    #[test]
    fn mirror_executes_tag_callback_and_pushes() {
        let (_dir, bare, target, sha) = bare_repo_with_target();
        let pushed_at: jiff::Timestamp = "2026-05-01T12:00:00Z".parse().unwrap();
        let meta = RunMeta {
            sha: sha.clone(),
            r#ref: "refs/heads/main".to_string(),
            pushed_at,
        };

        let mut secrets = HashMap::new();
        secrets.insert(
            "github_token".to_string(),
            SecretString::from("Authorization: Bearer test-token"),
        );

        let source = format!(
            r#"(local ci (require :quire.ci))
(ci.mirror "{url}"
  {{:secret :github_token
   :tag (fn [push] (.. "release-" (string.sub push.sha 1 8)))}})"#,
            url = format!("file://{}", target.display()),
        );
        let (runtime, run_fn) = mirror_run_fn(&source, secrets, &meta, &bare);

        runtime.enter_job("quire/mirror");
        let _: mlua::Value = run_fn.call(()).expect("mirror should succeed");
        runtime.leave_job();

        // Tag landed in the target repo via the push.
        let expected_tag = format!("release-{}", &sha[..8]);
        let tag_output = std::process::Command::new("git")
            .args(["tag", "-l"])
            .current_dir(&target)
            .output()
            .expect("git tag -l");
        let tags = String::from_utf8(tag_output.stdout).expect("utf8");
        assert!(
            tags.contains(&expected_tag),
            "tag should exist in target repo: {tags}"
        );

        // Tag and push outputs were recorded.
        let outputs = runtime.take_outputs();
        let recorded = outputs
            .get("quire/mirror")
            .expect("mirror outputs recorded");
        assert_eq!(recorded.len(), 2, "expected tag + push outputs");
    }

    #[test]
    fn mirror_pushes_listed_refs_when_trigger_ref_matches() {
        let (_dir, bare, target, sha) = bare_repo_with_target();
        // Create a release branch so git can push it.
        std::process::Command::new("git")
            .args(["branch", "release"])
            .current_dir(&bare)
            .output()
            .expect("git branch release");

        let pushed_at: jiff::Timestamp = "2026-05-01T12:00:00Z".parse().unwrap();
        let meta = RunMeta {
            sha,
            r#ref: "refs/heads/main".to_string(),
            pushed_at,
        };

        let mut secrets = HashMap::new();
        secrets.insert(
            "github_token".to_string(),
            SecretString::from("Authorization: Bearer test-token"),
        );

        // :refs is set and the trigger ref matches, so the mirror
        // should push the listed refs verbatim.
        let source = format!(
            r#"(local ci (require :quire.ci))
(ci.mirror "{url}"
  {{:secret :github_token
   :tag (fn [_] "v1")
   :refs ["refs/heads/main" "refs/heads/release"]}})"#,
            url = format!("file://{}", target.display()),
        );
        let (runtime, run_fn) = mirror_run_fn(&source, secrets, &meta, &bare);

        runtime.enter_job("quire/mirror");
        let _: mlua::Value = run_fn.call(()).expect("mirror should succeed");
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
            SecretString::from("Authorization: Bearer test-token"),
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
        let _: mlua::Value = run_fn.call(()).expect("mirror should succeed (no-op)");
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
        let err = run_fn.call::<mlua::Value>(()).unwrap_err();
        runtime.leave_job();

        let msg = err.to_string();
        assert!(
            msg.contains("missing"),
            "expected UnknownSecret(\"missing\"), got: {msg}"
        );
    }
}
