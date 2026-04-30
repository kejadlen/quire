//! Lua bridge for `ci.fnl`: the `quire.ci` module exposed to Fennel
//! scripts and the runtime primitives (`job`, `secret`, `sh`).
//!
//! All mlua/Fennel interaction lives here. The pipeline module calls
//! [`parse`] to evaluate a script and collect the registered jobs;
//! everything else (the `quire.ci` table, the primitive bodies, the
//! Lua-side data shapes) is internal.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Lua, LuaSerdeExt};

use super::pipeline::{Job, ValidationError};
use crate::Result;
use crate::fennel::Fennel;
use crate::secret::SecretString;

/// Evaluate `source` with the `quire.ci` module bound and collect the
/// registration results — one `Result` per `(ci.job …)` call. Pre-graph
/// rules run inside the callback, so a single bad job does not abort
/// the rest of the script.
pub(super) fn parse(
    fennel: &Fennel,
    source: &str,
    filename: &str,
    display: &str,
    secrets: HashMap<String, SecretString>,
) -> Result<Vec<std::result::Result<Job, ValidationError>>> {
    let jobs = Rc::new(RefCell::new(Vec::new()));
    let src = Rc::new(source.to_string());
    let secrets = Rc::new(secrets);

    fennel.eval_raw(source, filename, display, |lua| {
        let module = CiModule {
            jobs: jobs.clone(),
            source: src.clone(),
            secrets: secrets.clone(),
        }
        .install(lua)?;
        let loaded: mlua::Table = lua.globals().get::<mlua::Table>("package")?.get("loaded")?;
        loaded.set("quire.ci", module)?;
        Ok(())
    })?;

    Ok(jobs.take())
}

/// The `quire.ci` module exposed to Fennel scripts via `require`.
///
/// `install` stows the module on the Lua VM via `set_app_data`, then
/// builds a plain table whose entries are bare functions that look the
/// module back up at call time. Both `(ci.job …)` field access and
/// `(local {: job : secret} (require :quire.ci))` destructuring work.
///
/// ```fennel
/// (local ci (require :quire.ci))
/// (ci.job :build [:quire/push] (fn [_] nil))
/// (ci.secret :github_token)
/// ```
struct CiModule {
    jobs: Rc<RefCell<Vec<std::result::Result<Job, ValidationError>>>>,
    source: Rc<String>,
    secrets: Rc<HashMap<String, SecretString>>,
}

impl CiModule {
    /// Install the module on `lua` as app data and return the
    /// `quire.ci` table. The registered functions error at call time
    /// if the module isn't installed first.
    fn install(self, lua: &Lua) -> mlua::Result<mlua::Table> {
        lua.set_app_data(self);
        let table = lua.create_table()?;
        table.set("job", lua.create_function(register_job)?)?;
        table.set("secret", lua.create_function(lookup_secret)?)?;
        table.set("sh", lua.create_function(run_sh)?)?;
        Ok(table)
    }
}

/// Body of `(ci.job id inputs run-fn)`. Captures the call-site line
/// from the Lua debug stack so per-job validation errors carry a span
/// pointing back at the user's source.
fn register_job(
    lua: &Lua,
    (id, inputs, run_fn): (String, Vec<String>, mlua::Function),
) -> mlua::Result<()> {
    let m = lua
        .app_data_ref::<CiModule>()
        .ok_or_else(|| mlua::Error::external("quire.ci module not installed on Lua VM"))?;
    let line = lua
        .inspect_stack(1, |d| d.current_line())
        .flatten()
        .map(|l| l as u32)
        .unwrap_or(0);
    m.jobs
        .borrow_mut()
        .push(Job::new(id, inputs, run_fn, line, &m.source));
    Ok(())
}

/// Body of `(ci.secret name)`. Errors as a Lua error if the name is
/// undeclared or the file form fails to read.
fn lookup_secret(lua: &Lua, name: String) -> mlua::Result<String> {
    let m = lua
        .app_data_ref::<CiModule>()
        .ok_or_else(|| mlua::Error::external("quire.ci module not installed on Lua VM"))?;
    let secret = m
        .secrets
        .get(&name)
        .ok_or_else(|| mlua::Error::external(crate::Error::UnknownSecret(name)))?;
    secret
        .reveal()
        .map(|s| s.to_string())
        .map_err(mlua::Error::external)
}

/// The two valid shapes of `cmd` for `(ci.sh cmd …)`. A bare string
/// runs under `sh -c`; a sequence runs as argv with no shell.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum Cmd {
    Shell(String),
    Argv(Vec<String>),
}

impl From<Cmd> for std::process::Command {
    fn from(cmd: Cmd) -> Self {
        match cmd {
            Cmd::Shell(s) => {
                let mut c = std::process::Command::new("sh");
                c.arg("-c").arg(s);
                c
            }
            Cmd::Argv(argv) => {
                let mut c = std::process::Command::new(&argv[0]);
                c.args(&argv[1..]);
                c
            }
        }
    }
}

impl Cmd {
    /// Spawn this command with the given options, blocking until exit,
    /// and capture the result. Inherits the runner's env with
    /// `opts.env` merged on top.
    //
    // TODO: stream stdout/stderr live instead of buffering. `output()`
    // captures the full child output in memory and only returns at exit,
    // so long-running or chatty jobs show nothing until they finish.
    fn run(self, opts: ShOpts) -> std::io::Result<Output> {
        let mut command: std::process::Command = self.into();
        for (k, v) in opts.env {
            command.env(k, v);
        }
        if let Some(cwd) = opts.cwd {
            command.current_dir(cwd);
        }
        let output = command.output()?;
        // Signal-killed processes have no exit code; collapse them to -1
        // for now. Surfacing the signal as a separate field is future work.
        Ok(Output {
            exit: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

impl mlua::FromLua for Cmd {
    fn from_lua(value: mlua::Value, lua: &Lua) -> mlua::Result<Self> {
        // Pre-check the Lua type so wrong-shape inputs get specific
        // FromLuaConversionError messages — serde's untagged dispatch
        // would otherwise just say "data did not match any variant".
        match &value {
            mlua::Value::String(_) => lua.from_value(value),
            mlua::Value::Table(t) if t.raw_len() == 0 => {
                // `raw_len() == 0` covers both an empty sequence (`[]`)
                // and a string-keyed table (`{:env {...}}`) passed in
                // place of an argv list. One message handles both.
                Err(mlua::Error::FromLuaConversionError {
                    from: "table",
                    to: "Cmd".into(),
                    message: Some(
                        "ci.sh: cmd must be a non-empty sequence of strings or a shell string"
                            .into(),
                    ),
                })
            }
            mlua::Value::Table(_) => lua.from_value(value),
            other => Err(mlua::Error::FromLuaConversionError {
                from: other.type_name(),
                to: "Cmd".into(),
                message: Some("ci.sh: cmd must be a string or sequence of strings".into()),
            }),
        }
    }
}

/// The optional `opts` table for `(ci.sh cmd opts?)`. Unknown keys
/// fail closed so typos surface rather than being silently ignored.
#[derive(Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ShOpts {
    env: HashMap<String, String>,
    cwd: Option<String>,
}

impl mlua::FromLua for ShOpts {
    fn from_lua(value: mlua::Value, lua: &Lua) -> mlua::Result<Self> {
        lua.from_value(value)
    }
}

/// The captured outcome of running a process — what `(ci.sh …)`
/// returns. Crosses the boundary as plain serde data: `lua.to_value`
/// on the way out, `lua.from_value` on the way in.
///
/// A non-zero exit is reported in `:exit`, not raised as a Lua error —
/// matches the shape `(container …)` will eventually use so callers can
/// branch on it.
#[derive(serde::Serialize, serde::Deserialize)]
struct Output {
    exit: i32,
    stdout: String,
    stderr: String,
}

/// Body of `(ci.sh cmd opts?)`. Glue between the Lua call and
/// `Cmd::run` — defaults the opts and converts both directions.
fn run_sh(lua: &Lua, (cmd, opts): (Cmd, Option<ShOpts>)) -> mlua::Result<mlua::Value> {
    let output = cmd
        .run(opts.unwrap_or_default())
        .map_err(mlua::Error::external)?;
    lua.to_value(&output)
}

#[cfg(test)]
mod tests {
    use super::super::pipeline::load;
    use super::*;

    fn fennel() -> Fennel {
        Fennel::new().expect("Fennel::new() should succeed")
    }

    #[test]
    fn ci_secret_returns_resolved_value() {
        let f = fennel();
        let mut secrets = HashMap::new();
        secrets.insert(
            "github_token".to_string(),
            SecretString::from_plain("ghp_test_value"),
        );
        let source = r#"(local ci (require :quire.ci))
(ci.job :grab [:quire/push] (fn [_] (ci.secret :github_token)))"#;
        let pipeline =
            load(&f, source, "ci.fnl", "ci.fnl", secrets).expect("load should succeed");
        let token: String = pipeline.jobs()[0]
            .run_fn
            .call(())
            .expect("run_fn should return the secret value");
        assert_eq!(token, "ghp_test_value");
    }

    #[test]
    fn ci_secret_errors_for_unknown_name() {
        let f = fennel();
        let source = r#"(local ci (require :quire.ci))
(ci.job :grab [:quire/push] (fn [_] (ci.secret :missing)))"#;
        let pipeline = load(&f, source, "ci.fnl", "ci.fnl", HashMap::new())
            .expect("load should succeed");
        let err = pipeline.jobs()[0]
            .run_fn
            .call::<mlua::Value>(())
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown secret") && msg.contains("missing"),
            "expected unknown-secret error mentioning the name, got: {msg}"
        );
    }

    /// Build a pipeline whose single job's run-fn invokes `(ci.sh …)`,
    /// invoke it, and decode the resulting Lua table as Output through
    /// the same VM via `lua.from_value`. Owned data, so the Fennel VM
    /// can drop without a use-after-free.
    fn run_sh_via_job(source: &str) -> Output {
        let f = fennel();
        let pipeline =
            load(&f, source, "ci.fnl", "ci.fnl", HashMap::new()).expect("load should succeed");
        let value: mlua::Value = pipeline.jobs()[0]
            .run_fn
            .call(())
            .expect("ci.sh call should return a value");
        f.lua().from_value(value).expect("decode Output")
    }

    #[test]
    fn ci_sh_runs_argv_and_captures_stdout() {
        let r = run_sh_via_job(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [_] (ci.sh ["echo" "hello"])))"#,
        );
        assert_eq!(r.exit, 0);
        assert_eq!(r.stdout, "hello\n");
        assert!(r.stderr.is_empty());
    }

    #[test]
    fn ci_sh_runs_string_under_shell() {
        let r = run_sh_via_job(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [_] (ci.sh "echo hello | tr a-z A-Z")))"#,
        );
        assert_eq!(r.exit, 0);
        assert_eq!(r.stdout, "HELLO\n");
    }

    #[test]
    fn ci_sh_reports_nonzero_exit_without_erroring() {
        let r = run_sh_via_job(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [_] (ci.sh "exit 7")))"#,
        );
        assert_eq!(r.exit, 7);
    }

    #[test]
    fn ci_sh_merges_env_into_inherited() {
        // SAFETY: setting an env var in a single-threaded test process.
        unsafe {
            std::env::set_var("CI_SH_INHERITED_TEST", "from-parent");
        }
        let r = run_sh_via_job(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push]
  (fn [_]
    (ci.sh "echo $CI_SH_INHERITED_TEST $CI_SH_OVERRIDE_TEST"
           {:env {:CI_SH_OVERRIDE_TEST "from-opts"}})))"#,
        );
        assert_eq!(r.exit, 0);
        assert_eq!(r.stdout, "from-parent from-opts\n");
    }

    #[test]
    fn ci_sh_honors_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Resolve symlinks (macOS /tmp → /private/tmp) so the assertion holds.
        let canonical = fs_err::canonicalize(dir.path()).expect("canonicalize");
        let source = format!(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [_] (ci.sh "pwd" {{:cwd "{}"}})))"#,
            canonical.display()
        );
        let r = run_sh_via_job(&source);
        assert_eq!(r.exit, 0);
        assert_eq!(r.stdout.trim(), canonical.to_string_lossy());
    }

    #[test]
    fn ci_sh_rejects_unknown_opt_key() {
        let f = fennel();
        let pipeline = load(
            &f,
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [_] (ci.sh "echo hi" {:cwdir "/tmp"})))"#,
            "ci.fnl",
            "ci.fnl",
            HashMap::new(),
        )
        .expect("load should succeed");
        let err = pipeline.jobs()[0]
            .run_fn
            .call::<mlua::Value>(())
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field") && msg.contains("cwdir"),
            "expected unknown-field error mentioning the typo, got: {msg}"
        );
    }

    #[test]
    fn ci_sh_rejects_non_sequence_table_as_cmd() {
        let f = fennel();
        let pipeline = load(
            &f,
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [_] (ci.sh {:env {:FOO "bar"}})))"#,
            "ci.fnl",
            "ci.fnl",
            HashMap::new(),
        )
        .expect("load should succeed");
        let err = pipeline.jobs()[0]
            .run_fn
            .call::<mlua::Value>(())
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("sequence"),
            "expected sequence-shape error, got: {msg}"
        );
    }

    #[test]
    fn ci_sh_rejects_empty_argv() {
        let f = fennel();
        let pipeline = load(
            &f,
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [_] (ci.sh [])))"#,
            "ci.fnl",
            "ci.fnl",
            HashMap::new(),
        )
        .expect("load should succeed");
        let err = pipeline.jobs()[0]
            .run_fn
            .call::<mlua::Value>(())
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("empty"),
            "expected empty-argv error, got: {msg}"
        );
    }
}
