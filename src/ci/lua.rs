//! Lua bridge for `ci.fnl`: the registration-time module exposed via
//! `(require :quire.ci)` and the per-execution runtime handle passed
//! into each job's `run-fn`.
//!
//! All mlua/Fennel interaction lives here. The pipeline module calls
//! [`register`] to evaluate a script and collect the registered jobs;
//! the run module installs a [`Runtime`] and threads its handle into
//! each `run-fn` at execute time.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{IntoLua, Lua, LuaSerdeExt};

use miette::NamedSource;

use super::pipeline::{DefinitionError, Diagnostic, Job, PipelineError};
use crate::Result;
use crate::fennel::Fennel;
use crate::secret::SecretString;

/// Output of [`register`]: jobs and image successfully registered
/// from the script. Definition-time errors are returned via the `Err`
/// arm, not collected here.
#[derive(Debug)]
pub(super) struct Registrations {
    pub(super) jobs: Vec<Job>,
    pub(super) image: Option<String>,
}

/// Evaluate `source` with the registration module bound and collect
/// what got registered.
///
/// Pre-graph rules run inside the callback, so a single bad job does
/// not abort the rest of the script — but if any rule fired, the
/// whole batch is returned as a `PipelineError` instead of partial
/// registrations.
pub(super) fn register(fennel: &Fennel, source: &str, name: &str) -> Result<Registrations> {
    let jobs: Rc<RefCell<Vec<Job>>> = Rc::new(RefCell::new(Vec::new()));
    let image = Rc::new(RefCell::new(None));
    let src = Rc::new(source.to_string());

    let errors = Rc::new(RefCell::new(Vec::new()));

    fennel.eval_raw(source, name, |lua| {
        lua.register_module(
            "quire.ci",
            Registration {
                jobs: jobs.clone(),
                errors: errors.clone(),
                image: image.clone(),
                source: src.clone(),
            },
        )
    })?;

    // Remove the Registration app data so `ci.image`/`ci.job` calls at
    // runtime (inside run-fns) hit "registration not installed" instead of
    // silently pushing into the already-consumed sinks.
    fennel.lua().remove_app_data::<Registration>();

    let errors = errors.take();
    if !errors.is_empty() {
        return Err(PipelineError {
            src: NamedSource::new(name, source.to_string()),
            diagnostics: errors.into_iter().map(Diagnostic::Definition).collect(),
        }
        .into());
    }

    let image_name = image.borrow().as_ref().map(|i| i.name.clone());
    Ok(Registrations {
        jobs: jobs.take(),
        image: image_name,
    })
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
    jobs: Rc<RefCell<Vec<Job>>>,
    errors: Rc<RefCell<Vec<DefinitionError>>>,
    image: Rc<RefCell<Option<ImageRegistration>>>,
    source: Rc<String>,
}

impl IntoLua for Registration {
    fn into_lua(self, lua: &Lua) -> mlua::Result<mlua::Value> {
        lua.set_app_data(self);
        let table = lua.create_table()?;
        table.set("job", lua.create_function(register_job)?)?;
        table.set("image", lua.create_function(register_image)?)?;
        table.into_lua(lua)
    }
}

/// A pending image registration extracted from the Lua callback.
struct ImageRegistration {
    name: String,
    _line: u32,
}

/// Body of `(ci.image name)`. Records the image on the first call;
/// pushes a `DuplicateImage` error on subsequent calls.
fn register_image(lua: &Lua, (name,): (String,)) -> mlua::Result<()> {
    let r = lua
        .app_data_ref::<Registration>()
        .ok_or_else(|| mlua::Error::external("quire.ci registration not installed on Lua VM"))?;
    let line = lua
        .inspect_stack(1, |d| d.current_line())
        .flatten()
        .map(|l| l as u32)
        .unwrap_or(0);
    let mut img = r.image.borrow_mut();
    match &*img {
        Some(_) => {
            let span = super::pipeline::span_for_line(&r.source, line);
            r.errors
                .borrow_mut()
                .push(DefinitionError::DuplicateImage { span });
        }
        None => {
            *img = Some(ImageRegistration { name, _line: line });
        }
    }
    Ok(())
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
    match Job::new(id, inputs, run_fn, line, &r.source) {
        Ok(job) => r.jobs.borrow_mut().push(job),
        Err(e) => r.errors.borrow_mut().push(e),
    }
    Ok(())
}

/// Per-execution runtime: owns the Lua VM, holds the secrets exposed
/// to the job, the per-job `(jobs name)` views, the current-job
/// cursor, and the per-job captured `sh` outputs.
///
/// `inputs` is keyed by the calling job; each inner map covers
/// exactly the names that job may read. Reachability is implicit in
/// the structure, so `(jobs name)` is a flat lookup. The inner value
/// is `None` for reachable names without recorded outputs (future
/// job-to-job outputs drop in without changing the lookup contract);
/// names absent from the inner map are unreachable and produce a Lua
/// error.
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
pub(super) struct Runtime {
    pipeline: super::pipeline::Pipeline,
    secrets: HashMap<String, SecretString>,
    inputs: HashMap<String, HashMap<String, Option<mlua::Value>>>,
    current_job: RefCell<Option<String>>,
    outputs: RefCell<HashMap<String, Vec<ShOutput>>>,
}

impl Runtime {
    /// Consume `pipeline` and build a runtime ready to execute it.
    ///
    /// Takes ownership of the pipeline (including its Lua VM). `meta`
    /// provides the push data for `:quire/push` source outputs.
    ///
    /// Panics if any of the Lua table operations below fail. They run
    /// against a freshly initialized VM with `String`/`&str` keys and
    /// values, so the only realistic failure mode is allocation
    /// failure — abort is the right answer there. The matching
    /// `RuntimeHandle::into_lua` call at the executor's call site uses
    /// the same boundary.
    pub(super) fn new(
        pipeline: super::pipeline::Pipeline,
        secrets: HashMap<String, SecretString>,
        meta: &super::run::RunMeta,
        git_dir: &std::path::Path,
    ) -> Self {
        let transitive = pipeline.transitive_inputs();
        let lua = pipeline.fennel().lua();

        // Build the push outputs as a Lua table.
        let push = lua.create_table().expect("create push table");
        push.set("sha", meta.sha.as_str()).expect("set sha");
        push.set("ref", meta.r#ref.as_str()).expect("set ref");
        push.set("pushed-at", meta.pushed_at.to_string().as_str())
            .expect("set pushed-at");
        // `git-dir` is environmental rather than a fact about the push;
        // it may belong on an ambient context alongside `sh`/`secret`
        // instead of on this table.
        push.set("git-dir", git_dir.to_string_lossy().as_ref())
            .expect("set git-dir");
        let push_value = push.into_lua(lua).expect("push table to value");

        // Build per-job input views from transitive reachability.
        let mut inputs = HashMap::new();
        for (job_id, reachable) in &transitive {
            let mut view = HashMap::new();
            for name in reachable {
                let value = if name == "quire/push" {
                    Some(push_value.clone())
                } else {
                    None
                };
                view.insert(name.clone(), value);
            }
            inputs.insert(job_id.clone(), view);
        }

        Self {
            pipeline,
            secrets,
            inputs,
            current_job: RefCell::new(None),
            outputs: RefCell::new(HashMap::new()),
        }
    }

    /// Borrow the underlying Lua VM.
    pub(super) fn lua(&self) -> &Lua {
        self.pipeline.fennel().lua()
    }

    /// The topo-sorted job IDs in execution order.
    pub(super) fn topo_order(&self) -> Vec<&str> {
        self.pipeline.topo_order()
    }

    /// Look up a job by id.
    pub(super) fn job(&self, id: &str) -> Option<&super::pipeline::Job> {
        self.pipeline.job(id)
    }

    /// Mark `id` as the currently executing job. `(sh …)` invocations
    /// from this job's `run_fn` will record output under `id`, and
    /// `(jobs …)` lookups will resolve against `id`'s view.
    ///
    /// Panics if `id` has no inputs view — every job built by
    /// `Runtime::new` gets one, so a missing view means the executor
    /// is calling `enter_job` with an id that wasn't in the pipeline.
    pub(super) fn enter_job(&self, id: &str) {
        assert!(
            self.inputs.contains_key(id),
            "enter_job called with unknown job id '{id}'"
        );
        *self.current_job.borrow_mut() = Some(id.to_string());
    }

    /// Clear the current-job cursor. Subsequent `(sh …)` calls (if
    /// any) won't be attributed to a job until `enter_job` is called again.
    pub(super) fn leave_job(&self) {
        *self.current_job.borrow_mut() = None;
    }

    /// Drain all recorded outputs, returning them keyed by job id.
    pub(super) fn take_outputs(&self) -> HashMap<String, Vec<ShOutput>> {
        std::mem::take(&mut *self.outputs.borrow_mut())
    }
}

#[cfg(test)]
impl Runtime {
    /// Minimal constructor for tests — no source outputs, just
    /// secrets and the pipeline's VM.
    fn for_test(
        pipeline: super::pipeline::Pipeline,
        secrets: HashMap<String, SecretString>,
    ) -> Self {
        Self {
            pipeline,
            secrets,
            inputs: HashMap::new(),
            current_job: RefCell::new(None),
            outputs: RefCell::new(HashMap::new()),
        }
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

/// Body of `(jobs name)`. Returns the outputs the calling job's
/// view has for `name` as a Lua value. Reachable names without
/// recorded outputs come back as `nil`. Errors if `name` is outside
/// the calling job's view, if the calling job tries to read its own
/// outputs, or if the runtime isn't installed.
fn lookup_input(lua: &Lua, name: String) -> mlua::Result<mlua::Value> {
    let rt = lua
        .app_data_ref::<Rc<Runtime>>()
        .ok_or_else(|| mlua::Error::external("runtime not installed on Lua VM"))?;
    let calling = rt.current_job.borrow();
    let calling = calling
        .as_ref()
        .ok_or_else(|| mlua::Error::external("(jobs ...) called outside a job's run-fn"))?;
    // Runtime::new builds a view for every job and enter_job is the only
    // setter for current_job, so a missing view is a programming error,
    // not a user-reachable condition.
    let view = rt
        .inputs
        .get(calling)
        .unwrap_or_else(|| unreachable!("no inputs view for calling job '{calling}'"));
    match view.get(&name) {
        Some(Some(value)) => Ok(value.clone()),
        Some(None) => Ok(mlua::Value::Nil),
        None if name == *calling => Err(mlua::Error::external(format!(
            "Job '{calling}' cannot read its own outputs"
        ))),
        None => Err(mlua::Error::external(format!(
            "Job '{calling}' cannot read outputs from '{name}' — not in transitive inputs"
        ))),
    }
}

/// Body of `(secret name)`. Errors as a Lua error if the runtime
/// isn't installed, the name is undeclared, or the file form fails to
/// read.
//
// Errors here cross the mlua boundary via `Error::external`, which
// erases them to `Box<dyn Error + Send + Sync>`. The `std::error::Error`
// source chain is preserved, but miette `Diagnostic` metadata
// (codes, labels, source spans) does not survive the round trip —
// the resulting `mlua::Error` becomes the `#[source]` of
// `Error::JobFailed` at the executor, which only renders the chain
// as plain `Display`. Don't reach for richer error types here
// expecting them to render: rephrase the Display string to carry
// what the user needs to see.
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
///
/// `Argv` splits the program from its arguments at construction so
/// `From<Cmd> for Command` can't be handed an empty argv. The
/// non-empty invariant is enforced in [`mlua::FromLua`] before this
/// type is ever built.
enum Cmd {
    Shell(String),
    Argv { program: String, args: Vec<String> },
}

impl std::fmt::Display for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Cmd::Shell(s) => write!(f, "{s}"),
            Cmd::Argv { program, args } => {
                write!(f, "[\"{program}\"")?;
                for arg in args {
                    write!(f, ", \"{arg}\"")?;
                }
                write!(f, "]")
            }
        }
    }
}

impl From<Cmd> for std::process::Command {
    fn from(cmd: Cmd) -> Self {
        match cmd {
            Cmd::Shell(s) => {
                let mut c = std::process::Command::new("sh");
                c.arg("-c").arg(s);
                c
            }
            Cmd::Argv { program, args } => {
                let mut c = std::process::Command::new(program);
                c.args(args);
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
    // The streaming rewrite should write to the per-job log file
    // (`jobs/<id>/log.yml`) as output arrives instead of batching
    // everything into `write_all_logs` at the end — see `Run::execute`.
    // Also revisit the `from_utf8_lossy` calls below — non-UTF-8 bytes
    // are silently replaced with U+FFFD and `:stdout` / `:stderr` end
    // up as mojibake with no signal that anything was lost.
    fn run(self, opts: ShOpts) -> std::io::Result<ShOutput> {
        let cmd_str = format!("{self}");
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
            cmd: cmd_str,
        })
    }
}

impl mlua::FromLua for Cmd {
    fn from_lua(value: mlua::Value, lua: &Lua) -> mlua::Result<Self> {
        match &value {
            mlua::Value::String(_) => Ok(Cmd::Shell(lua.from_value(value)?)),
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
            mlua::Value::Table(_) => {
                let argv: Vec<String> = lua.from_value(value)?;
                let mut iter = argv.into_iter();
                let program = iter.next().expect("raw_len > 0 ensures argv is non-empty");
                Ok(Cmd::Argv {
                    program,
                    args: iter.collect(),
                })
            }
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
    /// The command that was run, formatted for display.
    #[serde(default)]
    pub cmd: String,
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
    use super::*;

    /// Consume the pipeline for its VM, build a minimal runtime,
    /// and return the runtime and first job's run_fn.
    fn rt(source: &str, secrets: HashMap<String, SecretString>) -> (Rc<Runtime>, mlua::Function) {
        let pipeline =
            super::super::pipeline::compile(source, "ci.fnl").expect("compile should succeed");
        let run_fn = pipeline.jobs()[0].run_fn.clone();
        let runtime = Rc::new(Runtime::for_test(pipeline, secrets));
        let _ = RuntimeHandle(runtime.clone())
            .into_lua(runtime.lua())
            .expect("install runtime");
        (runtime, run_fn)
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
        let (runtime, run_fn) = rt(source, secrets);
        let handle = RuntimeHandle(runtime.clone())
            .into_lua(runtime.lua())
            .expect("install runtime");
        let token: String = run_fn
            .call(handle)
            .expect("run_fn should return the secret value");
        assert_eq!(token, "ghp_test_value");
    }

    #[test]
    fn secret_errors_for_unknown_name() {
        let source = r#"(local ci (require :quire.ci))
(ci.job :grab [:quire/push] (fn [{: secret}] (secret :missing)))"#;
        let (runtime, run_fn) = rt(source, HashMap::new());
        let handle = RuntimeHandle(runtime.clone())
            .into_lua(runtime.lua())
            .expect("install runtime");
        let err = run_fn.call::<mlua::Value>(handle).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown secret") && msg.contains("missing"),
            "expected unknown-secret error mentioning the name, got: {msg}"
        );
    }

    /// Build a pipeline whose single job's run-fn invokes `(sh …)`,
    /// invoke it with the runtime handle, and decode the resulting Lua
    /// table as ShOutput.
    fn run_sh_via_job(source: &str) -> ShOutput {
        let (runtime, run_fn) = rt(source, HashMap::new());
        let handle = RuntimeHandle(runtime.clone())
            .into_lua(runtime.lua())
            .expect("install runtime");
        let value: mlua::Value = run_fn.call(handle).expect("sh call should return a value");
        runtime.lua().from_value(value).expect("decode ShOutput")
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
        let (runtime, run_fn) = rt(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [{: sh}] (sh "echo hi" {:cwdir "/tmp"})))"#,
            HashMap::new(),
        );
        let handle = RuntimeHandle(runtime.clone())
            .into_lua(runtime.lua())
            .expect("install runtime");
        let err = run_fn.call::<mlua::Value>(handle).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field") && msg.contains("cwdir"),
            "expected unknown-field error mentioning the typo, got: {msg}"
        );
    }

    #[test]
    fn sh_rejects_non_sequence_table_as_cmd() {
        let (runtime, run_fn) = rt(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [{: sh}] (sh {:env {:FOO "bar"}})))"#,
            HashMap::new(),
        );
        let handle = RuntimeHandle(runtime.clone())
            .into_lua(runtime.lua())
            .expect("install runtime");
        let err = run_fn.call::<mlua::Value>(handle).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("sequence"),
            "expected sequence-shape error, got: {msg}"
        );
    }

    #[test]
    fn sh_rejects_empty_argv() {
        let (runtime, run_fn) = rt(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [{: sh}] (sh [])))"#,
            HashMap::new(),
        );
        let handle = RuntimeHandle(runtime.clone())
            .into_lua(runtime.lua())
            .expect("install runtime");
        let err = run_fn.call::<mlua::Value>(handle).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("empty"),
            "expected empty-argv error, got: {msg}"
        );
    }

    #[test]
    fn sh_rejects_number_as_cmd() {
        let (runtime, run_fn) = rt(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [{: sh}] (sh 42)))"#,
            HashMap::new(),
        );
        let handle = RuntimeHandle(runtime.clone())
            .into_lua(runtime.lua())
            .expect("install runtime");
        let err = run_fn.call::<mlua::Value>(handle).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("string or sequence"),
            "expected type error, got: {msg}"
        );
    }
}
