//! Lua bridge for `ci.fnl`: the registration-time module exposed via
//! `(require :quire.ci)` and the per-execution runtime handle passed
//! into each job's `run-fn`.
//!
//! All mlua/Fennel interaction lives here. The pipeline module calls
//! [`parse`] to evaluate a script and collect the registered jobs;
//! the run module installs a [`Runtime`] and threads its handle into
//! each `run-fn` at execute time.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use mlua::{IntoLua, Lua, LuaSerdeExt};

use super::pipeline::{Job, ValidationError};
use crate::Result;
use crate::fennel::Fennel;
use crate::secret::SecretString;

/// Evaluate `source` with the registration module bound and collect
/// the registration results — one `Result` per `(ci.job …)` call.
/// Pre-graph rules run inside the callback, so a single bad job does
/// not abort the rest of the script.
pub(super) fn parse(
    fennel: &Fennel,
    source: &str,
    name: &str,
) -> Result<Vec<std::result::Result<Job, ValidationError>>> {
    let jobs = Rc::new(RefCell::new(Vec::new()));
    let src = Rc::new(source.to_string());

    fennel.eval_raw(source, name, |lua| {
        lua.register_module(
            "quire.ci",
            Registration {
                jobs: jobs.clone(),
                source: src.clone(),
            },
        )
    })?;

    Ok(jobs.take())
}

/// The registration-time module exposed to Fennel scripts via
/// `(require :quire.ci)`.
///
/// Converted into a Lua table via [`IntoLua`]: stows itself on the
/// VM as app data (so `register_job` can find the registration sink)
/// and returns a table whose only entry is `job`. Runtime primitives
/// (`sh`, `secret`) live on the per-execution [`Runtime`] handle, not
/// here.
///
/// ```fennel
/// (local ci (require :quire.ci))
/// (ci.job :build [:quire/push]
///   (fn [{: sh : secret}]
///     (sh ["echo" (secret :github_token)])))
/// ```
struct Registration {
    jobs: Rc<RefCell<Vec<std::result::Result<Job, ValidationError>>>>,
    source: Rc<String>,
}

impl IntoLua for Registration {
    fn into_lua(self, lua: &Lua) -> mlua::Result<mlua::Value> {
        lua.set_app_data(self);
        let table = lua.create_table()?;
        table.set("job", lua.create_function(register_job)?)?;
        table.into_lua(lua)
    }
}

/// Body of `(ci.job id inputs run-fn)`. Captures the call-site line
/// from the Lua debug stack so per-job validation errors carry a span
/// pointing back at the user's source.
fn register_job(
    lua: &Lua,
    (id, inputs, run_fn): (String, Vec<String>, mlua::Function),
) -> mlua::Result<()> {
    let r = lua
        .app_data_ref::<Registration>()
        .ok_or_else(|| mlua::Error::external("quire.ci registration not installed on Lua VM"))?;
    let line = lua
        .inspect_stack(1, |d| d.current_line())
        .flatten()
        .map(|l| l as u32)
        .unwrap_or(0);
    r.jobs
        .borrow_mut()
        .push(Job::new(id, inputs, run_fn, line, &r.source));
    Ok(())
}

/// Per-execution runtime: holds the secrets exposed to the job, the
/// inputs available via `(jobs name)`, the per-job transitive-input
/// reachability sets, the current-job cursor, and the per-job
/// captured `sh` outputs.
///
/// Wrap an `Rc<Runtime>` in [`RuntimeHandle`] and convert it via
/// [`IntoLua`] to install it on the Lua VM (sets app data, returns
/// the handle table passed into each `run_fn`). The newtype is
/// required because the orphan rule forbids `impl IntoLua` directly
/// on `Rc<Runtime>`.
///
/// Outside a run, no runtime is installed; in that case `(sh …)`
/// runs the command but doesn't record (the cursor lookup misses).
/// `(secret …)` and `(jobs …)` require a runtime — without one, calls
/// error.
#[derive(Debug)]
pub(super) struct Runtime {
    secrets: HashMap<String, SecretString>,
    /// Inputs readable via `(jobs name)`. Source outputs (e.g.
    /// `quire/push`) are populated by the executor before the loop;
    /// job-to-job outputs are not yet wired up.
    inputs: RefCell<HashMap<String, mlua::Value>>,
    /// For each job, the set of names it may legally read via
    /// `(jobs name)` — its transitive ancestors in the input graph,
    /// including source refs. Self is never present.
    transitive_inputs: HashMap<String, HashSet<String>>,
    current_job: RefCell<Option<String>>,
    outputs: RefCell<HashMap<String, Vec<ShOutput>>>,
}

impl Runtime {
    /// Build a fresh runtime. `inputs` is the map of source outputs
    /// already prepared as Lua values; `transitive_inputs` is the
    /// per-job reachability set from [`Pipeline::transitive_inputs`].
    pub(super) fn new(
        secrets: HashMap<String, SecretString>,
        inputs: HashMap<String, mlua::Value>,
        transitive_inputs: HashMap<String, HashSet<String>>,
    ) -> Self {
        Self {
            secrets,
            inputs: RefCell::new(inputs),
            transitive_inputs,
            current_job: RefCell::new(None),
            outputs: RefCell::new(HashMap::new()),
        }
    }

    /// Mark `id` as the currently executing job. `(sh …)` invocations
    /// from this job's `run_fn` will record output under `id`, and
    /// `(jobs …)` lookups will validate against `id`'s reachability set.
    pub(super) fn enter_job(&self, id: &str) {
        *self.current_job.borrow_mut() = Some(id.to_string());
    }

    /// Clear the current-job cursor. Subsequent `(sh …)` calls (if
    /// any) won't be attributed to a job until `enter_job` is called again.
    pub(super) fn leave_job(&self) {
        *self.current_job.borrow_mut() = None;
    }

    /// Snapshot the recorded outputs for `id`. Empty if the job
    /// produced none (or hasn't run).
    pub(super) fn outputs(&self, id: &str) -> Vec<ShOutput> {
        self.outputs.borrow().get(id).cloned().unwrap_or_default()
    }
}

/// `IntoLua` carrier for an `Rc<Runtime>`. Stows the Rc on the VM as
/// app data and returns the handle table — `{sh, secret, jobs}`.
pub(super) struct RuntimeHandle(pub Rc<Runtime>);

impl IntoLua for RuntimeHandle {
    fn into_lua(self, lua: &Lua) -> mlua::Result<mlua::Value> {
        lua.set_app_data(self.0);
        let table = lua.create_table()?;
        table.set("sh", lua.create_function(run_sh)?)?;
        table.set("secret", lua.create_function(lookup_secret)?)?;
        table.set("jobs", lua.create_function(lookup_input)?)?;
        table.into_lua(lua)
    }
}

/// Body of `(jobs name)`. Returns the outputs registered for `name`
/// if `name` is a transitive ancestor of the calling job (or a source
/// ref reachable through one). Errors loudly if `name` isn't reachable
/// or no runtime is installed.
fn lookup_input(lua: &Lua, name: String) -> mlua::Result<mlua::Value> {
    let rt = lua
        .app_data_ref::<Rc<Runtime>>()
        .ok_or_else(|| mlua::Error::external("runtime not installed on Lua VM"))?;
    let calling = rt.current_job.borrow();
    let calling = calling
        .as_ref()
        .ok_or_else(|| mlua::Error::external("(jobs ...) called outside a job's run-fn"))?;
    let reachable = rt.transitive_inputs.get(calling).ok_or_else(|| {
        mlua::Error::external(format!(
            "no transitive-input set for calling job '{calling}'"
        ))
    })?;
    if !reachable.contains(&name) {
        if name == *calling {
            return Err(mlua::Error::external(format!(
                "Job '{calling}' cannot read its own outputs"
            )));
        }
        return Err(mlua::Error::external(format!(
            "Job '{calling}' cannot read outputs from '{name}' — not in transitive inputs"
        )));
    }
    // Reachable but no outputs recorded yet: nil. Job-to-job outputs
    // aren't wired up, so this is the common case for non-source names.
    Ok(rt
        .inputs
        .borrow()
        .get(&name)
        .cloned()
        .unwrap_or(mlua::Value::Nil))
}

/// Body of `(secret name)`. Errors as a Lua error if the runtime
/// isn't installed, the name is undeclared, or the file form fails to
/// read.
fn lookup_secret(lua: &Lua, name: String) -> mlua::Result<String> {
    let rt = lua
        .app_data_ref::<Rc<Runtime>>()
        .ok_or_else(|| mlua::Error::external("runtime not installed on Lua VM"))?;
    let secret = rt
        .secrets
        .get(&name)
        .ok_or_else(|| mlua::Error::external(crate::Error::UnknownSecret(name)))?;
    secret
        .reveal()
        .map(|s| s.to_string())
        .map_err(mlua::Error::external)
}

/// The two valid shapes of `cmd` for `(sh cmd …)`. A bare string
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
    fn run(self, opts: ShOpts) -> std::io::Result<ShOutput> {
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
        Ok(ShOutput {
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
                        "sh: cmd must be a non-empty sequence of strings or a shell string".into(),
                    ),
                })
            }
            mlua::Value::Table(_) => lua.from_value(value),
            other => Err(mlua::Error::FromLuaConversionError {
                from: other.type_name(),
                to: "Cmd".into(),
                message: Some("sh: cmd must be a string or sequence of strings".into()),
            }),
        }
    }
}

/// The optional `opts` table for `(sh cmd opts?)`. Unknown keys fail
/// closed so typos surface rather than being silently ignored.
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

/// The captured outcome of running a process — what `(sh …)` returns.
/// Crosses the boundary as plain serde data: `lua.to_value` on the
/// way out, `lua.from_value` on the way in.
///
/// A non-zero exit is reported in `:exit`, not raised as a Lua error —
/// matches the shape `(container …)` will eventually use so callers can
/// branch on it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ShOutput {
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Body of `(sh cmd opts?)`. Glue between the Lua call and `Cmd::run`
/// — defaults the opts, runs the command, records output into the
/// active runtime (if any) under the current job, and converts the
/// result back to a Lua table.
fn run_sh(lua: &Lua, (cmd, opts): (Cmd, Option<ShOpts>)) -> mlua::Result<mlua::Value> {
    let output = cmd
        .run(opts.unwrap_or_default())
        .map_err(mlua::Error::external)?;

    if let Some(rt) = lua.app_data_ref::<Rc<Runtime>>()
        && let Some(job) = rt.current_job.borrow().as_ref()
    {
        rt.outputs
            .borrow_mut()
            .entry(job.clone())
            .or_default()
            .push(output.clone());
    }

    lua.to_value(&output)
}

#[cfg(test)]
mod tests {
    use super::super::pipeline::Pipeline;
    use super::*;

    /// Install a runtime with the given secrets on `pipeline`'s VM and
    /// return the runtime handle. Mirrors what `Run::execute` does so
    /// tests can drive a `run_fn` directly.
    fn rt(pipeline: &Pipeline, secrets: HashMap<String, SecretString>) -> mlua::Value {
        RuntimeHandle(Rc::new(Runtime::new(
            secrets,
            HashMap::new(),
            HashMap::new(),
        )))
        .into_lua(pipeline.fennel().lua())
        .expect("install runtime")
    }

    #[test]
    fn secret_returns_resolved_value() {
        let mut secrets = HashMap::new();
        secrets.insert(
            "github_token".to_string(),
            SecretString::from_plain("ghp_test_value"),
        );
        let source = r#"(local ci (require :quire.ci))
(ci.job :grab [:quire/push] (fn [{: secret}] (secret :github_token)))"#;
        let pipeline = Pipeline::load(source, "ci.fnl").expect("load should succeed");
        let token: String = pipeline.jobs()[0]
            .run_fn
            .call(rt(&pipeline, secrets))
            .expect("run_fn should return the secret value");
        assert_eq!(token, "ghp_test_value");
    }

    #[test]
    fn secret_errors_for_unknown_name() {
        let source = r#"(local ci (require :quire.ci))
(ci.job :grab [:quire/push] (fn [{: secret}] (secret :missing)))"#;
        let pipeline = Pipeline::load(source, "ci.fnl").expect("load should succeed");
        let err = pipeline.jobs()[0]
            .run_fn
            .call::<mlua::Value>(rt(&pipeline, HashMap::new()))
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown secret") && msg.contains("missing"),
            "expected unknown-secret error mentioning the name, got: {msg}"
        );
    }

    /// Build a pipeline whose single job's run-fn invokes `(sh …)`,
    /// invoke it with the runtime handle, and decode the resulting Lua
    /// table as ShOutput through the pipeline's VM via `lua.from_value`.
    fn run_sh_via_job(source: &str) -> ShOutput {
        let pipeline = Pipeline::load(source, "ci.fnl").expect("load should succeed");
        let value: mlua::Value = pipeline.jobs()[0]
            .run_fn
            .call(rt(&pipeline, HashMap::new()))
            .expect("sh call should return a value");
        pipeline
            .fennel()
            .lua()
            .from_value(value)
            .expect("decode ShOutput")
    }

    #[test]
    fn sh_runs_argv_and_captures_stdout() {
        let r = run_sh_via_job(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [{: sh}] (sh ["echo" "hello"])))"#,
        );
        assert_eq!(r.exit, 0);
        assert_eq!(r.stdout, "hello\n");
        assert!(r.stderr.is_empty());
    }

    #[test]
    fn sh_runs_string_under_shell() {
        let r = run_sh_via_job(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [{: sh}] (sh "echo hello | tr a-z A-Z")))"#,
        );
        assert_eq!(r.exit, 0);
        assert_eq!(r.stdout, "HELLO\n");
    }

    #[test]
    fn sh_reports_nonzero_exit_without_erroring() {
        let r = run_sh_via_job(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [{: sh}] (sh "exit 7")))"#,
        );
        assert_eq!(r.exit, 7);
    }

    #[test]
    fn sh_merges_env_into_inherited() {
        // SAFETY: setting an env var in a single-threaded test process.
        unsafe {
            std::env::set_var("CI_SH_INHERITED_TEST", "from-parent");
        }
        let r = run_sh_via_job(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push]
  (fn [{: sh}]
    (sh "echo $CI_SH_INHERITED_TEST $CI_SH_OVERRIDE_TEST"
        {:env {:CI_SH_OVERRIDE_TEST "from-opts"}})))"#,
        );
        assert_eq!(r.exit, 0);
        assert_eq!(r.stdout, "from-parent from-opts\n");
    }

    #[test]
    fn sh_honors_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Resolve symlinks (macOS /tmp → /private/tmp) so the assertion holds.
        let canonical = fs_err::canonicalize(dir.path()).expect("canonicalize");
        let source = format!(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [{{: sh}}] (sh "pwd" {{:cwd "{}"}})))"#,
            canonical.display()
        );
        let r = run_sh_via_job(&source);
        assert_eq!(r.exit, 0);
        assert_eq!(r.stdout.trim(), canonical.to_string_lossy());
    }

    #[test]
    fn sh_rejects_unknown_opt_key() {
        let pipeline = Pipeline::load(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [{: sh}] (sh "echo hi" {:cwdir "/tmp"})))"#,
            "ci.fnl",
        )
        .expect("load should succeed");
        let err = pipeline.jobs()[0]
            .run_fn
            .call::<mlua::Value>(rt(&pipeline, HashMap::new()))
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field") && msg.contains("cwdir"),
            "expected unknown-field error mentioning the typo, got: {msg}"
        );
    }

    #[test]
    fn sh_rejects_non_sequence_table_as_cmd() {
        let pipeline = Pipeline::load(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [{: sh}] (sh {:env {:FOO "bar"}})))"#,
            "ci.fnl",
        )
        .expect("load should succeed");
        let err = pipeline.jobs()[0]
            .run_fn
            .call::<mlua::Value>(rt(&pipeline, HashMap::new()))
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("sequence"),
            "expected sequence-shape error, got: {msg}"
        );
    }

    #[test]
    fn sh_rejects_empty_argv() {
        let pipeline = Pipeline::load(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [{: sh}] (sh [])))"#,
            "ci.fnl",
        )
        .expect("load should succeed");
        let err = pipeline.jobs()[0]
            .run_fn
            .call::<mlua::Value>(rt(&pipeline, HashMap::new()))
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("empty"),
            "expected empty-argv error, got: {msg}"
        );
    }
}
