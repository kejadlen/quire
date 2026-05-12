//! Per-execution runtime: the state passed to each job's `run-fn`.
//!
//! Owns the Lua VM (via the consumed `Pipeline`), the secrets the job
//! may resolve, the per-job `(jobs name)` views, the current-job
//! cursor, and the captured `sh` outputs. The handle threaded into
//! each `run-fn` exposes three primitives — `sh`, `secret`, `jobs` —
//! implemented by the free functions below.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use jiff::Timestamp;
use mlua::{IntoLua, Lua, LuaSerdeExt};

use super::pipeline::{Job, Pipeline};
use super::run::RunMeta;
use crate::secret::{self, SecretRegistry, SecretString, redact};

/// Errors produced by [`Runtime`] methods and the `RunFn::Rust`
/// callbacks that hold them. A small sum carved out of the
/// orchestrator's kitchen-sink error so the runtime layer doesn't
/// drag rusqlite/yaml/etc. along with it.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum RuntimeError {
    #[error(transparent)]
    Secret(#[from] secret::Error),

    #[error(transparent)]
    Lua(Box<mlua::Error>),

    #[error("command spawn failed: {program} in {cwd}")]
    CommandSpawnFailed {
        program: String,
        cwd: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("git error: {0}")]
    Git(String),

    #[error("failed to write CRI log at {path}")]
    LogWriteFailed {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl From<mlua::Error> for RuntimeError {
    fn from(err: mlua::Error) -> Self {
        Self::Lua(Box::new(err))
    }
}

pub type RuntimeResult<T> = std::result::Result<T, RuntimeError>;

/// Per-sh timing: (started_at, finished_at). Sequential by sh-call
/// order within a job; consumers derive the 1-based sh index from the
/// position in the vector.
pub type ShTimings = Vec<(Timestamp, Timestamp)>;

/// Lifecycle events fired by [`Runtime`] for an installed observer.
/// Carries borrowed strings so the hot path doesn't allocate.
///
/// Currently only sh process lifecycle is observable; the variant
/// names leave room for other event sources (secret access, jobs
/// lookups, etc.) without renaming the enum.
#[derive(Debug)]
pub enum RuntimeEvent<'a> {
    ShStarted { cmd: &'a str },
    ShFinished { exit: i32 },
}

/// A callback that observes [`RuntimeEvent`]s.
pub type RuntimeCallback = Box<dyn FnMut(RuntimeEvent<'_>)>;

/// The default callback installed on a fresh [`Runtime`]: drops every
/// event. Replaced via [`Runtime::set_event_callback`].
fn noop_event_callback() -> RuntimeCallback {
    Box::new(|_| {})
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
/// All three handle primitives — `(sh …)`, `(secret …)`, and
/// `(jobs …)` — require a runtime to be installed on the VM. Calls
/// from a VM without one error.
pub struct Runtime {
    pipeline: Pipeline,
    /// Unified secret store: holds declared secrets and their revealed
    /// values for both lookup and redaction. No Debug impl on the
    /// registry; Runtime must not derive Debug either.
    pub registry: RefCell<SecretRegistry>,
    pub inputs: HashMap<String, HashMap<String, Option<mlua::Value>>>,
    pub current_job: RefCell<Option<String>>,
    pub outputs: RefCell<HashMap<String, Vec<ShOutput>>>,
    /// Per-sh timing records: job_id → (started_at, finished_at).
    /// Parallel to `outputs`; consumers derive the 1-based sh index
    /// from the position in the vector.
    pub sh_timings: RefCell<HashMap<String, ShTimings>>,
    /// Observer notified of [`RuntimeEvent`]s. Defaults to a no-op;
    /// callers install a real one via [`Runtime::set_event_callback`].
    event_callback: RefCell<RuntimeCallback>,
    /// Directory under which [`Runtime::sh`] writes per-sh CRI log
    /// files at `<log_dir>/jobs/<job_id>/sh-<n>.log`. Set at
    /// construction time; callers manage the directory's lifetime.
    log_dir: std::path::PathBuf,
    /// The materialized workspace for this run. Every `(sh …)` call
    /// runs here.
    workspace: std::path::PathBuf,
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
    pub fn new(
        pipeline: Pipeline,
        secrets: HashMap<String, SecretString>,
        meta: &RunMeta,
        git_dir: &std::path::Path,
        workspace: std::path::PathBuf,
        log_dir: std::path::PathBuf,
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
            inputs,
            registry: RefCell::new(SecretRegistry::from(secrets)),
            current_job: RefCell::new(None),
            outputs: RefCell::new(HashMap::new()),
            sh_timings: RefCell::new(HashMap::new()),
            event_callback: RefCell::new(noop_event_callback()),
            log_dir,
            workspace,
        }
    }

    /// Borrow the underlying Lua VM.
    pub fn lua(&self) -> &Lua {
        self.pipeline.fennel().lua()
    }

    /// The topo-sorted job IDs in execution order.
    pub fn topo_order(&self) -> Vec<&str> {
        self.pipeline.topo_order()
    }

    /// Look up a job by id.
    pub fn job(&self, id: &str) -> Option<&Job> {
        self.pipeline.job(id)
    }

    /// Mark `id` as the currently executing job. `(sh …)` invocations
    /// from this job's `run_fn` will record output under `id`, and
    /// `(jobs …)` lookups will resolve against `id`'s view.
    ///
    /// Panics if `id` has no inputs view — every job built by
    /// `Runtime::new` gets one, so a missing view means the executor
    /// is calling `enter_job` with an id that wasn't in the pipeline.
    pub fn enter_job(&self, id: &str) {
        assert!(
            self.inputs.contains_key(id),
            "enter_job called with unknown job id '{id}'"
        );
        *self.current_job.borrow_mut() = Some(id.to_string());
    }

    /// Clear the current-job cursor. Subsequent `(sh …)` calls (if
    /// any) won't be attributed to a job until `enter_job` is called again.
    pub fn leave_job(&self) {
        *self.current_job.borrow_mut() = None;
    }

    /// Drain all recorded outputs, returning them keyed by job id.
    pub fn take_outputs(&self) -> HashMap<String, Vec<ShOutput>> {
        std::mem::take(&mut *self.outputs.borrow_mut())
    }

    /// Drain all recorded sh timings, returning them keyed by job id.
    pub fn take_sh_timings(&self) -> HashMap<String, ShTimings> {
        std::mem::take(&mut *self.sh_timings.borrow_mut())
    }

    /// Install an observer for runtime lifecycle events. Currently the
    /// only events fired are [`RuntimeEvent::ShStarted`] (before each
    /// sh process spawns) and [`RuntimeEvent::ShFinished`] (after exit),
    /// only when an `enter_job`/`leave_job` window is open — sh calls
    /// outside a job are not observed, mirroring the recording behavior.
    ///
    /// Replaces the previously installed callback (the default is a
    /// no-op). The callback must not call back into `Runtime::sh`
    /// (re-entrant borrow on the callback slot) or into
    /// `take_outputs` / `take_sh_timings` mid-run.
    pub fn set_event_callback(&self, callback: RuntimeCallback) {
        *self.event_callback.borrow_mut() = callback;
    }

    /// Resolve a declared secret by name, caching it for redaction.
    /// Errors if the name isn't declared or the secret's source
    /// can't be read.
    ///
    /// The returned `String` is the plain, revealed value — never
    /// trace or log it directly. See [`SecretRegistry::resolve`] for
    /// the full caveat.
    ///
    /// [`SecretRegistry::resolve`]: quire_core::secret::SecretRegistry::resolve
    pub fn secret(&self, name: &str) -> RuntimeResult<String> {
        self.registry.borrow_mut().resolve(name).map_err(Into::into)
    }

    /// Run `cmd` with `opts` and record its output against the
    /// current job (if one is active). Non-zero exits come back in
    /// `:exit`, not as `Err`.
    ///
    /// Fires `RuntimeEvent::ShStarted` before the spawn and
    /// `RuntimeEvent::ShFinished` after exit when a job is current.
    /// Callbacks installed via [`Self::set_event_callback`] must not
    /// re-enter `sh`.
    pub fn sh(&self, cmd: Cmd, opts: ShOpts) -> RuntimeResult<ShOutput> {
        let started_at = Timestamp::now();
        let program = cmd.program().to_string();
        let current_job = self.current_job.borrow().clone();

        if current_job.is_some() {
            let cmd_display = redact(&cmd.to_string(), &self.registry.borrow());
            (self.event_callback.borrow_mut())(RuntimeEvent::ShStarted { cmd: &cmd_display });
        }

        let output =
            cmd.run(opts, &self.workspace)
                .map_err(|e| RuntimeError::CommandSpawnFailed {
                    program,
                    cwd: self.workspace.clone(),
                    source: e,
                })?;
        let finished_at = Timestamp::now();

        if let Some(job_id) = current_job {
            (self.event_callback.borrow_mut())(RuntimeEvent::ShFinished { exit: output.exit });

            let recorded = {
                let reg = self.registry.borrow();
                ShOutput {
                    exit: output.exit,
                    stdout: redact(&output.stdout, &reg),
                    stderr: redact(&output.stderr, &reg),
                    cmd: redact(&output.cmd, &reg),
                }
            };

            let job_dir = self.log_dir.join("jobs").join(&job_id);
            fs_err::create_dir_all(&job_dir).map_err(|source| RuntimeError::LogWriteFailed {
                path: job_dir.clone(),
                source,
            })?;
            let n = self.outputs.borrow().get(&job_id).map_or(0, Vec::len) + 1;
            let log_path = job_dir.join(format!("sh-{n}.log"));
            super::logs::write_cri_log(&log_path, &recorded, &started_at.to_string()).map_err(
                |source| RuntimeError::LogWriteFailed {
                    path: log_path,
                    source,
                },
            )?;

            self.outputs
                .borrow_mut()
                .entry(job_id.clone())
                .or_default()
                .push(recorded);
            self.sh_timings
                .borrow_mut()
                .entry(job_id)
                .or_default()
                .push((started_at, finished_at));
        }

        Ok(output)
    }
}

#[cfg(test)]
impl Runtime {
    /// Test-only accessor for the runtime's log directory.
    pub(crate) fn log_dir(&self) -> &std::path::Path {
        &self.log_dir
    }

    /// Minimal constructor for tests — no source outputs, just
    /// secrets and the pipeline's VM. Workspace defaults to cwd; logs
    /// land under a fresh tempdir each call (leaked into the system
    /// temp area, since tests don't share a TempDir handle).
    fn for_test(pipeline: Pipeline, secrets: HashMap<String, SecretString>) -> Self {
        let log_dir = tempfile::tempdir()
            .expect("tempdir for runtime logs")
            .keep();
        Self {
            pipeline,
            registry: RefCell::new(SecretRegistry::from(secrets)),
            inputs: HashMap::new(),
            current_job: RefCell::new(None),
            outputs: RefCell::new(HashMap::new()),
            sh_timings: RefCell::new(HashMap::new()),
            event_callback: RefCell::new(noop_event_callback()),
            log_dir,
            workspace: std::env::current_dir().expect("cwd"),
        }
    }
}

/// Install the runtime primitives on the Lua VM.
///
/// Stows the `Rc<Runtime>` as app data and populates the stub seeded
/// at `package.loaded["quire.runtime"]` by
/// [`crate::fennel::Fennel::new`] with `sh`, `secret`, and `jobs`
/// closures over the active runtime. An `__index` metatable raises a
/// clear error for any other key. The active job slot is set/cleared
/// by the executor around each run-fn invocation via
/// [`Runtime::enter_job`] / [`Runtime::leave_job`].
///
/// The stub is mutated in place rather than replaced so that
/// references captured during registration — e.g.
/// `(require :quire.runtime)` directly, or
/// `(local {: runtime} (require :quire.ci))` — see the populated
/// table at call time. There is no `runtime` global; user code must
/// reach the primitives via one of the require paths.
pub struct RuntimeHandle(pub Rc<Runtime>);

impl RuntimeHandle {
    /// Install the runtime on the Lua VM. Call once before executing
    /// any run-fns; pair with [`RuntimeHandle::uninstall`] when the
    /// run is done.
    pub fn install(self, lua: &Lua) -> mlua::Result<()> {
        fn runtime(lua: &Lua) -> mlua::Result<mlua::AppDataRef<'_, Rc<Runtime>>> {
            lua.app_data_ref::<Rc<Runtime>>()
                .ok_or_else(|| mlua::Error::external("runtime not installed on Lua VM"))
        }

        lua.set_app_data(self.0);
        let rt: mlua::Table = runtime_stub(lua)?;

        rt.set(
            "sh",
            lua.create_function(|lua, (cmd, opts): (Cmd, Option<ShOpts>)| {
                let rt = runtime(lua)?;
                let output = rt
                    .sh(cmd, opts.unwrap_or_default())
                    .map_err(mlua::Error::external)?;
                lua.to_value(&output)
            })?,
        )?;

        rt.set(
            "secret",
            lua.create_function(|lua, name: String| {
                let rt = runtime(lua)?;
                rt.secret(&name).map_err(mlua::Error::external)
            })?,
        )?;

        rt.set(
            "jobs",
            lua.create_function(|lua, name: String| {
                let rt = runtime(lua)?;
                let calling = rt.current_job.borrow();
                let calling = calling.as_ref().ok_or_else(|| {
                    mlua::Error::external(
                        "runtime accessed outside a job — primitives are only available while a run-fn is executing",
                    )
                })?;
                // Runtime::new builds a view for every job and
                // enter_job is the only setter for current_job, so a
                // missing view is a programming error, not a
                // user-reachable condition.
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
            })?,
        )?;

        // Catch typos: any key other than sh/secret/jobs raises
        // a clear error instead of returning nil.
        let mt = lua.create_table()?;
        mt.set(
            "__index",
            lua.create_function(
                |_lua, (_table, key): (mlua::Table, String)| -> mlua::Result<mlua::Value> {
                    Err(mlua::Error::external(format!(
                        "unknown runtime primitive '{key}' — expected sh, secret, or jobs"
                    )))
                },
            )?,
        )?;
        rt.set_metatable(Some(mt))?;
        Ok(())
    }

    /// Tear down the ambient runtime: clear `sh`/`secret`/`jobs` from
    /// the runtime table, drop the metatable, and remove the
    /// `Rc<Runtime>` app data. Idempotent — calling twice is a no-op.
    ///
    /// In practice the Lua VM is dropped right after a run, so this
    /// is hygiene rather than necessity; pair it with `install` so
    /// the install/uninstall lifecycle is explicit at the call site.
    pub fn uninstall(lua: &Lua) -> mlua::Result<()> {
        let rt: mlua::Table = runtime_stub(lua)?;
        rt.set("sh", mlua::Value::Nil)?;
        rt.set("secret", mlua::Value::Nil)?;
        rt.set("jobs", mlua::Value::Nil)?;
        rt.set_metatable(None)?;
        lua.remove_app_data::<Rc<Runtime>>();
        Ok(())
    }
}

/// The runtime stub at `package.loaded["quire.ci"].runtime`. Seeded
/// as a placeholder by `Fennel::new`, threaded through
/// `registration::register`, mutated by `install`, cleared by
/// `uninstall`.
fn runtime_stub(lua: &Lua) -> mlua::Result<mlua::Table> {
    let package: mlua::Table = lua.globals().get("package")?;
    let loaded: mlua::Table = package.get("loaded")?;
    let ci: mlua::Table = loaded.get("quire.ci")?;
    ci.get::<mlua::Table>("runtime")
}

/// The two valid shapes of `cmd` for `(sh cmd …)`. A bare string
/// runs under `sh -c`; a sequence runs as argv with no shell.
///
/// `Argv` splits the program from its arguments at construction so
/// `From<Cmd> for Command` can't be handed an empty argv. The
/// non-empty invariant is enforced in [`mlua::FromLua`] before this
/// type is ever built.
pub enum Cmd {
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
    pub fn program(&self) -> &str {
        match self {
            Cmd::Shell(_) => "sh",
            Cmd::Argv { program, .. } => program,
        }
    }

    /// Spawn this command with the given options, blocking until exit,
    /// and capture the result. Inherits the runner's env with
    /// `opts.env` merged on top. `cwd` becomes the child's working
    /// directory unconditionally.
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
    pub fn run(self, opts: ShOpts, cwd: &std::path::Path) -> std::io::Result<ShOutput> {
        let cmd_str = format!("{self}");
        let mut command: std::process::Command = self.into();
        for (k, v) in opts.env {
            command.env(k, v);
        }
        command.current_dir(cwd);
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
#[derive(Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShOpts {
    pub env: HashMap<String, String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::pipeline::{RunFn, compile};

    /// Consume the pipeline for its VM, build a minimal runtime,
    /// and return the runtime and first job's Lua run_fn. Tests in
    /// this module exercise the `RunFn::Lua` path; if the first job
    /// turns out to be a `Rust` variant the test setup is wrong.
    fn rt(source: &str, secrets: HashMap<String, SecretString>) -> (Rc<Runtime>, mlua::Function) {
        let pipeline = compile(source, "ci.fnl").expect("compile should succeed");
        let run_fn = match pipeline.jobs()[0].run_fn.clone() {
            RunFn::Lua(f) => f,
            RunFn::Rust(_) => panic!("expected RunFn::Lua for test setup"),
        };
        let runtime = Rc::new(Runtime::for_test(pipeline, secrets));
        RuntimeHandle(runtime.clone())
            .install(runtime.lua())
            .expect("install runtime");
        (runtime, run_fn)
    }

    #[test]
    fn secret_returns_resolved_value() {
        let mut secrets = HashMap::new();
        secrets.insert(
            "github_token".to_string(),
            SecretString::from("ghp_test_value"),
        );
        let source = r#"(local {: job : runtime} (require :quire.ci))
(job :grab [:quire/push] (fn [] (runtime.secret :github_token)))"#;
        let (_runtime, run_fn) = rt(source, secrets);
        let token: String = run_fn
            .call::<String>(())
            .expect("run_fn should return the secret value");
        assert_eq!(token, "ghp_test_value");
    }

    #[test]
    fn runtime_destructured_from_quire_ci_resolves_after_install() {
        // (local {: runtime} (require :quire.ci)) must bind to the
        // same table that `RuntimeHandle::install` mutates in place,
        // so `runtime.secret` works inside a run-fn.
        let mut secrets = HashMap::new();
        secrets.insert("k".to_string(), SecretString::from("v"));
        let source = r#"(local {: job : runtime} (require :quire.ci))
(job :grab [:quire/push] (fn [] (runtime.secret :k)))"#;
        let (_runtime, run_fn) = rt(source, secrets);
        let token: String = run_fn.call::<String>(()).expect("run_fn");
        assert_eq!(token, "v");
    }

    #[test]
    fn uninstall_clears_runtime_table_and_app_data() {
        let source = r#"(local {: job : runtime} (require :quire.ci))
(job :grab [:quire/push] (fn [] (runtime.secret :anything)))"#;
        let (runtime, _run_fn) = rt(source, HashMap::new());
        let lua = runtime.lua();
        RuntimeHandle::uninstall(lua).expect("uninstall");

        let rt: mlua::Table = runtime_stub(lua).expect("runtime stub");
        assert!(matches!(
            rt.get::<mlua::Value>("sh").expect("sh"),
            mlua::Value::Nil
        ));
        assert!(matches!(
            rt.get::<mlua::Value>("secret").expect("secret"),
            mlua::Value::Nil
        ));
        assert!(matches!(
            rt.get::<mlua::Value>("jobs").expect("jobs"),
            mlua::Value::Nil
        ));
        assert!(rt.metatable().is_none(), "metatable should be cleared");
        assert!(
            lua.app_data_ref::<Rc<Runtime>>().is_none(),
            "app data should be removed"
        );

        // Idempotent.
        RuntimeHandle::uninstall(lua).expect("uninstall twice");
    }

    #[test]
    fn secret_errors_for_unknown_name() {
        let source = r#"(local {: job : runtime} (require :quire.ci))
(job :grab [:quire/push] (fn [] (runtime.secret :missing)))"#;
        let (_runtime, run_fn) = rt(source, HashMap::new());
        let err = run_fn.call::<mlua::Value>(()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown secret") && msg.contains("missing"),
            "expected unknown-secret error mentioning the name, got: {msg}"
        );
    }

    #[test]
    fn event_callback_fires_sh_started_then_sh_finished() {
        let received: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let received_clone = received.clone();

        let (runtime, run_fn) = rt(
            r#"(local {: job : runtime} (require :quire.ci))
(job :go [:quire/push] (fn [] (runtime.sh ["echo" "hi"])))"#,
            HashMap::new(),
        );
        runtime.set_event_callback(Box::new(move |event| match event {
            RuntimeEvent::ShStarted { cmd } => {
                received_clone.borrow_mut().push(format!("started:{cmd}"));
            }
            RuntimeEvent::ShFinished { exit } => {
                received_clone.borrow_mut().push(format!("finished:{exit}"));
            }
        }));
        *runtime.current_job.borrow_mut() = Some("go".to_string());

        let _: mlua::Value = run_fn.call(()).expect("sh call");

        let calls = received.borrow();
        assert_eq!(calls.len(), 2, "expected 2 events, got: {calls:?}");
        assert!(
            calls[0].starts_with("started:") && calls[0].contains("echo"),
            "started event shape: {}",
            calls[0]
        );
        assert_eq!(calls[1], "finished:0");
    }

    #[test]
    fn sh_writes_cri_log_inline() {
        let (runtime, run_fn) = rt(
            r#"(local {: job : runtime} (require :quire.ci))
(job :go [:quire/push] (fn [] (runtime.sh ["echo" "hi"])))"#,
            HashMap::new(),
        );
        let log_dir = runtime.log_dir().to_path_buf();
        *runtime.current_job.borrow_mut() = Some("go".to_string());

        let _: mlua::Value = run_fn.call(()).expect("sh call");

        let log_path = log_dir.join("jobs").join("go").join("sh-1.log");
        assert!(log_path.exists(), "expected sh-1.log at {log_path:?}");
        let contents = std::fs::read_to_string(&log_path).expect("read log");
        assert!(
            contents.contains("stdout F hi"),
            "expected stdout line in log, got: {contents:?}"
        );
    }

    #[test]
    fn event_callback_not_fired_without_current_job() {
        let count = Rc::new(RefCell::new(0u32));
        let count_clone = count.clone();

        let (runtime, run_fn) = rt(
            r#"(local {: job : runtime} (require :quire.ci))
(job :go [:quire/push] (fn [] (runtime.sh ["echo" "hi"])))"#,
            HashMap::new(),
        );
        runtime.set_event_callback(Box::new(move |_event| {
            *count_clone.borrow_mut() += 1;
        }));
        // No enter_job — current_job stays None.

        let _: mlua::Value = run_fn.call(()).expect("sh call");

        assert_eq!(*count.borrow(), 0, "callback should not fire outside a job");
    }

    /// Build a pipeline whose single job's run-fn invokes `(sh …)`,
    /// invoke it with the ambient runtime, and decode the resulting Lua
    /// table as ShOutput.
    fn run_sh_via_job(source: &str) -> ShOutput {
        let (runtime, run_fn) = rt(source, HashMap::new());
        let value: mlua::Value = run_fn.call(()).expect("sh call should return a value");
        runtime.lua().from_value(value).expect("decode ShOutput")
    }

    #[test]
    fn sh_redacts_secret_in_recorded_output() {
        let mut secrets = HashMap::new();
        secrets.insert(
            "github_token".to_string(),
            SecretString::from("ghp_long_secret_value"),
        );
        let source = r#"(local {: job : runtime} (require :quire.ci))
(job :go [:quire/push]
  (fn []
    (let [tok (runtime.secret :github_token)]
      (runtime.sh ["echo" tok]))))"#;
        let (runtime, run_fn) = rt(source, secrets);

        // Mark a current job so sh records into outputs. for_test seeds
        // an empty inputs map, so enter_job would panic; bypass the
        // assertion by writing the field directly.
        *runtime.current_job.borrow_mut() = Some("go".to_string());

        let value: mlua::Value = run_fn.call(()).expect("sh call");
        let returned: ShOutput = runtime.lua().from_value(value).expect("decode");

        // The Lua caller still sees the raw value — echo printed it.
        assert!(returned.stdout.contains("ghp_long_secret_value"));

        // The recorded copy is redacted.
        let outputs = runtime.take_outputs();
        let recorded = outputs
            .get("go")
            .and_then(|v| v.first())
            .expect("recorded sh output for 'go'");
        assert!(
            !recorded.stdout.contains("ghp_long_secret_value"),
            "recorded stdout must not contain raw secret: {}",
            recorded.stdout
        );
        assert!(
            recorded.stdout.contains("{{ github_token }}"),
            "recorded stdout must contain redaction marker: {}",
            recorded.stdout
        );
        assert!(
            !recorded.cmd.contains("ghp_long_secret_value"),
            "recorded cmd must not contain raw secret: {}",
            recorded.cmd
        );
    }

    #[test]
    fn sh_runs_argv_and_captures_stdout() {
        let r = run_sh_via_job(
            r#"(local {: job : runtime} (require :quire.ci))
(job :go [:quire/push] (fn [] (runtime.sh ["echo" "hello"])))"#,
        );
        assert_eq!(r.exit, 0);
        assert_eq!(r.stdout, "hello\n");
        assert!(r.stderr.is_empty());
    }

    #[test]
    fn sh_runs_string_under_shell() {
        let r = run_sh_via_job(
            r#"(local {: job : runtime} (require :quire.ci))
(job :go [:quire/push] (fn [] (runtime.sh "echo hello | tr a-z A-Z")))"#,
        );
        assert_eq!(r.exit, 0);
        assert_eq!(r.stdout, "HELLO\n");
    }

    #[test]
    fn sh_reports_nonzero_exit_without_erroring() {
        let r = run_sh_via_job(
            r#"(local {: job : runtime} (require :quire.ci))
(job :go [:quire/push] (fn [] (runtime.sh "exit 7")))"#,
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
            r#"(local {: job : runtime} (require :quire.ci))
(job :go [:quire/push]
  (fn []
    (runtime.sh "echo $CI_SH_INHERITED_TEST $CI_SH_OVERRIDE_TEST"
        {:env {:CI_SH_OVERRIDE_TEST "from-opts"}})))"#,
        );
        assert_eq!(r.exit, 0);
        assert_eq!(r.stdout, "from-parent from-opts\n");
    }

    #[test]
    fn sh_rejects_unknown_opt_key() {
        let (_runtime, run_fn) = rt(
            r#"(local {: job : runtime} (require :quire.ci))
(job :go [:quire/push] (fn [] (runtime.sh "echo hi" {:cwdir "/tmp"})))"#,
            HashMap::new(),
        );
        let err = run_fn.call::<mlua::Value>(()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field") && msg.contains("cwdir"),
            "expected unknown-field error mentioning the unknown key, got: {msg}"
        );
    }

    #[test]
    fn sh_rejects_non_sequence_table_as_cmd() {
        let (_runtime, run_fn) = rt(
            r#"(local {: job : runtime} (require :quire.ci))
(job :go [:quire/push] (fn [] (runtime.sh {:env {:FOO "bar"}})))"#,
            HashMap::new(),
        );
        let err = run_fn.call::<mlua::Value>(()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("sequence"),
            "expected sequence-shape error, got: {msg}"
        );
    }

    #[test]
    fn sh_rejects_empty_argv() {
        let (_runtime, run_fn) = rt(
            r#"(local {: job : runtime} (require :quire.ci))
(job :go [:quire/push] (fn [] (runtime.sh [])))"#,
            HashMap::new(),
        );
        let err = run_fn.call::<mlua::Value>(()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("empty"),
            "expected empty-argv error, got: {msg}"
        );
    }

    #[test]
    fn sh_rejects_number_as_cmd() {
        let (_runtime, run_fn) = rt(
            r#"(local {: job : runtime} (require :quire.ci))
(job :go [:quire/push] (fn [] (runtime.sh 42)))"#,
            HashMap::new(),
        );
        let err = run_fn.call::<mlua::Value>(()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("string or sequence"),
            "expected type error, got: {msg}"
        );
    }

    // --- quire.stdlib mirror tests ---

    /// Run `git` once with the standard test env. Asserts success and
    /// returns stdout. Used by the mirror fixture below to set up the
    /// source and target bare repos.
    fn git(args: &[&str], cwd: &std::path::Path) -> String {
        let env_vars: [(&str, &str); 6] = [
            ("GIT_AUTHOR_NAME", "test"),
            ("GIT_AUTHOR_EMAIL", "test@test"),
            ("GIT_COMMITTER_NAME", "test"),
            ("GIT_COMMITTER_EMAIL", "test@test"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ];
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .envs(env_vars)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf8")
    }

    /// Build a source bare repo with one commit and an empty target
    /// bare repo in the same tempdir. Returns (tempdir, source bare,
    /// target bare, head sha).
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
        git(&["init", "-b", "main"], &work);
        git(&["commit", "--allow-empty", "-m", "initial"], &work);
        let sha = git(&["rev-parse", "HEAD"], &work).trim().to_string();
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

    #[test]
    fn stdlib_mirror_tags_and_pushes() {
        let (_dir, bare, target, sha) = bare_repo_with_target();

        let mut secrets = HashMap::new();
        secrets.insert(
            "github_token".to_string(),
            SecretString::from("Authorization: Bearer test-token"),
        );

        let source = format!(
            r#"(local {{: job : runtime}} (require :quire.ci))
(local {{: mirror}} (require :quire.stdlib))
(job :go [:quire/push]
  (fn []
    (let [auth (runtime.secret :github_token)]
      (mirror {{:url "{url}"
               :auth-header auth
               :sha "{sha}"
               :tag "v1"
               :git-dir "{git_dir}"}}))))"#,
            url = format!("file://{}", target.display()),
            sha = sha,
            git_dir = bare.display(),
        );

        let (_runtime, run_fn) = rt(&source, secrets);
        let _: mlua::Value = run_fn.call(()).expect("mirror should succeed");

        // Tag landed in the target repo, pointing at the head SHA.
        let resolved = git(&["rev-parse", "refs/tags/v1"], &target);
        assert_eq!(resolved.trim(), sha);
    }

    #[test]
    fn stdlib_mirror_errors_on_missing_required_opt() {
        let source = r#"(local {: job} (require :quire.ci))
(local {: mirror} (require :quire.stdlib))
(job :go [:quire/push]
  (fn []
    (mirror {:auth-header "x" :sha "x" :tag "v1" :git-dir "/tmp"})))"#;
        let (_runtime, run_fn) = rt(source, HashMap::new());
        let err = run_fn.call::<mlua::Value>(()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Missing argument url"),
            "expected missing-:url error, got: {msg}"
        );
    }
}
