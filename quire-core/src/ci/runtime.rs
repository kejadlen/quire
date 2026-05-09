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
}

impl From<mlua::Error> for RuntimeError {
    fn from(err: mlua::Error) -> Self {
        Self::Lua(Box::new(err))
    }
}

pub type RuntimeResult<T> = std::result::Result<T, RuntimeError>;

/// Per-sh timing: (index, started_at, finished_at).
pub type ShTimings = Vec<(usize, Timestamp, Timestamp)>;

/// Lifecycle events fired by [`Runtime::sh`] when a callback is
/// installed via [`Runtime::set_sh_callback`]. Carries borrowed strings
/// so the hot path doesn't allocate on the way through.
#[derive(Debug)]
pub enum ShEvent<'a> {
    Started {
        job_id: &'a str,
        n: usize,
        started_at: Timestamp,
        cmd: &'a str,
    },
    Finished {
        job_id: &'a str,
        n: usize,
        finished_at: Timestamp,
        exit: i32,
    },
}

/// A callback that observes [`Runtime::sh`] lifecycle events.
pub type ShCallback = Box<dyn FnMut(ShEvent<'_>)>;

/// The default callback installed on a fresh [`Runtime`]: drops every
/// event. Replaced via [`Runtime::set_sh_callback`].
fn noop_sh_callback() -> ShCallback {
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
    /// Per-sh timing records: job_id → (sh_index, started_at, finished_at).
    /// Parallel to `outputs`; each entry at the same index corresponds.
    pub sh_timings: RefCell<HashMap<String, ShTimings>>,
    /// Per-job sh call counter for assigning sequential indices.
    sh_counter: RefCell<HashMap<String, usize>>,
    /// Observer notified when an sh call starts and finishes. Defaults
    /// to a no-op; callers install a real one via
    /// [`Runtime::set_sh_callback`].
    sh_callback: RefCell<ShCallback>,
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
            sh_counter: RefCell::new(HashMap::new()),
            sh_callback: RefCell::new(noop_sh_callback()),
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

    /// Install an observer for sh lifecycle events. The callback fires
    /// once before each sh process spawns ([`ShEvent::Started`]) and
    /// once after it exits ([`ShEvent::Finished`]), but only when an
    /// `enter_job`/`leave_job` window is open — sh calls outside a job
    /// are not observed, mirroring the recording behavior.
    ///
    /// Replaces the previously installed callback (the default is a
    /// no-op). The callback must not call back into `Runtime::sh`
    /// (re-entrant borrow on the callback slot) or into
    /// `take_outputs` / `take_sh_timings` mid-run.
    pub fn set_sh_callback(&self, callback: ShCallback) {
        *self.sh_callback.borrow_mut() = callback;
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
    /// Fires `ShEvent::Started` before the spawn and
    /// `ShEvent::Finished` after exit when an sh callback has been
    /// installed via [`Self::set_sh_callback`] *and* a job is current.
    /// Callbacks must not re-enter `sh`.
    pub fn sh(&self, cmd: Cmd, opts: ShOpts) -> RuntimeResult<ShOutput> {
        let started_at = Timestamp::now();
        let program = cmd.program().to_string();

        // Reserve (job_id, n) up-front when a job is current; we need
        // `n` to label the Started callback. Increment runs before the
        // spawn so a panic in run() still leaves a consistent counter.
        let job_id_n: Option<(String, usize)> = self.current_job.borrow().as_ref().map(|job| {
            let mut counter = self.sh_counter.borrow_mut();
            let entry = counter.entry(job.clone()).or_insert(1);
            let n = *entry;
            *entry += 1;
            (job.clone(), n)
        });

        if let Some((job_id, n)) = &job_id_n {
            let cmd_display = redact(&cmd.to_string(), &self.registry.borrow());
            (self.sh_callback.borrow_mut())(ShEvent::Started {
                job_id,
                n: *n,
                started_at,
                cmd: &cmd_display,
            });
        }

        let output =
            cmd.run(opts, &self.workspace)
                .map_err(|e| RuntimeError::CommandSpawnFailed {
                    program,
                    cwd: self.workspace.clone(),
                    source: e,
                })?;
        let finished_at = Timestamp::now();

        if let Some((job_id, n)) = &job_id_n {
            (self.sh_callback.borrow_mut())(ShEvent::Finished {
                job_id,
                n: *n,
                finished_at,
                exit: output.exit,
            });

            self.outputs
                .borrow_mut()
                .entry(job_id.clone())
                .or_default()
                .push({
                    let reg = self.registry.borrow();
                    ShOutput {
                        exit: output.exit,
                        stdout: redact(&output.stdout, &reg),
                        stderr: redact(&output.stderr, &reg),
                        cmd: redact(&output.cmd, &reg),
                    }
                });
            self.sh_timings
                .borrow_mut()
                .entry(job_id.clone())
                .or_default()
                .push((*n, started_at, finished_at));
        }

        Ok(output)
    }
}

#[cfg(test)]
impl Runtime {
    /// Minimal constructor for tests — no source outputs, just
    /// secrets and the pipeline's VM. Defaults the workspace to the
    /// process CWD so tests that don't care about cwd keep working.
    fn for_test(pipeline: Pipeline, secrets: HashMap<String, SecretString>) -> Self {
        Self {
            pipeline,
            registry: RefCell::new(SecretRegistry::from(secrets)),
            inputs: HashMap::new(),
            current_job: RefCell::new(None),
            outputs: RefCell::new(HashMap::new()),
            sh_timings: RefCell::new(HashMap::new()),
            sh_counter: RefCell::new(HashMap::new()),
            sh_callback: RefCell::new(noop_sh_callback()),
            workspace: std::env::current_dir().expect("cwd"),
        }
    }
}

/// `IntoLua` carrier for an `Rc<Runtime>`. Stows the Rc on the VM as
/// app data and returns the handle table — `{sh, secret, jobs}`.
pub struct RuntimeHandle(pub Rc<Runtime>);

impl IntoLua for RuntimeHandle {
    // Errors raised by the closures below cross the mlua boundary via
    // `Error::external`, which erases them to
    // `Box<dyn Error + Send + Sync>`. The `std::error::Error` source
    // chain is preserved, but miette `Diagnostic` metadata (codes,
    // labels, source spans) does not survive the round trip — the
    // resulting `mlua::Error` becomes the `#[source]` of
    // `Error::JobFailed` at the executor, which only renders the chain
    // as plain `Display`. Don't reach for richer error types here
    // expecting them to render: rephrase the Display string to carry
    // what the user needs to see.
    fn into_lua(self, lua: &Lua) -> mlua::Result<mlua::Value> {
        // Pull the installed runtime out of `lua`'s app data, or
        // surface a Lua error. Every adapter below needs this.
        fn runtime(lua: &Lua) -> mlua::Result<mlua::AppDataRef<'_, Rc<Runtime>>> {
            lua.app_data_ref::<Rc<Runtime>>()
                .ok_or_else(|| mlua::Error::external("runtime not installed on Lua VM"))
        }

        lua.set_app_data(self.0);
        let table = lua.create_table()?;

        table.set(
            "sh",
            lua.create_function(|lua, (cmd, opts): (Cmd, Option<ShOpts>)| {
                let rt = runtime(lua)?;
                let output = rt
                    .sh(cmd, opts.unwrap_or_default())
                    .map_err(mlua::Error::external)?;
                lua.to_value(&output)
            })?,
        )?;

        table.set(
            "secret",
            lua.create_function(|lua, name: String| {
                let rt = runtime(lua)?;
                rt.secret(&name).map_err(mlua::Error::external)
            })?,
        )?;

        table.set(
            "jobs",
            lua.create_function(|lua, name: String| {
                let rt = runtime(lua)?;
                let calling = rt.current_job.borrow();
                let calling = calling.as_ref().ok_or_else(|| {
                    mlua::Error::external("(jobs ...) called outside a job's run-fn")
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

        table.into_lua(lua)
    }
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
            SecretString::from("ghp_test_value"),
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

    #[test]
    fn sh_callback_fires_started_then_finished() {
        let received: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let received_clone = received.clone();

        let (runtime, run_fn) = rt(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [{: sh}] (sh ["echo" "hi"])))"#,
            HashMap::new(),
        );
        runtime.set_sh_callback(Box::new(move |event| match event {
            ShEvent::Started { job_id, n, cmd, .. } => {
                received_clone
                    .borrow_mut()
                    .push(format!("started:{job_id}:{n}:{cmd}"));
            }
            ShEvent::Finished {
                job_id, n, exit, ..
            } => {
                received_clone
                    .borrow_mut()
                    .push(format!("finished:{job_id}:{n}:{exit}"));
            }
        }));
        *runtime.current_job.borrow_mut() = Some("go".to_string());

        let handle = RuntimeHandle(runtime.clone())
            .into_lua(runtime.lua())
            .expect("install runtime");
        let _: mlua::Value = run_fn.call(handle).expect("sh call");

        let calls = received.borrow();
        assert_eq!(calls.len(), 2, "expected 2 events, got: {calls:?}");
        assert!(
            calls[0].starts_with("started:go:1:"),
            "started event shape: {}",
            calls[0]
        );
        assert!(
            calls[0].contains("echo"),
            "started should carry cmd: {}",
            calls[0]
        );
        assert_eq!(calls[1], "finished:go:1:0");
    }

    #[test]
    fn sh_callback_not_fired_without_current_job() {
        let count = Rc::new(RefCell::new(0u32));
        let count_clone = count.clone();

        let (runtime, run_fn) = rt(
            r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push] (fn [{: sh}] (sh ["echo" "hi"])))"#,
            HashMap::new(),
        );
        runtime.set_sh_callback(Box::new(move |_event| {
            *count_clone.borrow_mut() += 1;
        }));
        // No enter_job — current_job stays None.

        let handle = RuntimeHandle(runtime.clone())
            .into_lua(runtime.lua())
            .expect("install runtime");
        let _: mlua::Value = run_fn.call(handle).expect("sh call");

        assert_eq!(*count.borrow(), 0, "callback should not fire outside a job");
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
    fn sh_redacts_secret_in_recorded_output() {
        let mut secrets = HashMap::new();
        secrets.insert(
            "github_token".to_string(),
            SecretString::from("ghp_long_secret_value"),
        );
        let source = r#"(local ci (require :quire.ci))
(ci.job :go [:quire/push]
  (fn [{: sh : secret}]
    (let [tok (secret :github_token)]
      (sh ["echo" tok]))))"#;
        let (runtime, run_fn) = rt(source, secrets);
        let handle = RuntimeHandle(runtime.clone())
            .into_lua(runtime.lua())
            .expect("install runtime");

        // Mark a current job so sh records into outputs. for_test seeds
        // an empty inputs map, so enter_job would panic; bypass the
        // assertion by writing the field directly.
        *runtime.current_job.borrow_mut() = Some("go".to_string());

        let value: mlua::Value = run_fn.call(handle).expect("sh call");
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
            "expected unknown-field error mentioning the unknown key, got: {msg}"
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
