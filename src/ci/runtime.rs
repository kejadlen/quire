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
use super::run::{DockerLifecycle, RunMeta};
use crate::secret::SecretString;

use super::redact::{SecretRegistry, redact};
/// Per-sh timing: (index, started_at, finished_at).
pub(super) type ShTimings = Vec<(usize, Timestamp, Timestamp)>;

/// The runtime-side carrier for the chosen [`Executor`](super::run::Executor).
/// `Host` runs `sh` directly on the host. `Docker` owns a
/// [`DockerLifecycle`] whose Drop tears down the per-run container;
/// `Runtime::sh` reads the variant's payload to wrap each command in
/// `docker exec`.
pub(super) enum ExecutorRuntime {
    Host,
    Docker(DockerLifecycle),
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
pub(super) struct Runtime {
    pipeline: Pipeline,
    /// Unified secret store: holds declared secrets and their revealed
    /// values for both lookup and redaction. No Debug impl on the
    /// registry; Runtime must not derive Debug either.
    pub(super) registry: RefCell<SecretRegistry>,
    pub(super) inputs: HashMap<String, HashMap<String, Option<mlua::Value>>>,
    pub(super) current_job: RefCell<Option<String>>,
    pub(super) outputs: RefCell<HashMap<String, Vec<ShOutput>>>,
    /// Per-sh timing records: job_id → (sh_index, started_at, finished_at).
    /// Parallel to `outputs`; each entry at the same index corresponds.
    pub(super) sh_timings: RefCell<HashMap<String, ShTimings>>,
    /// Per-job sh call counter for assigning sequential indices.
    sh_counter: RefCell<HashMap<String, usize>>,
    /// The materialized workspace for this run. Every `(sh …)` call
    /// runs here.
    workspace: std::path::PathBuf,
    /// The chosen executor. `Host` is a no-op; `Docker` owns the
    /// per-run container's lifecycle (teardown on Drop).
    executor: ExecutorRuntime,
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
        pipeline: Pipeline,
        secrets: HashMap<String, SecretString>,
        meta: &RunMeta,
        git_dir: &std::path::Path,
        workspace: std::path::PathBuf,
        executor: ExecutorRuntime,
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
            registry: RefCell::new(SecretRegistry::new(secrets)),
            current_job: RefCell::new(None),
            outputs: RefCell::new(HashMap::new()),
            sh_timings: RefCell::new(HashMap::new()),
            sh_counter: RefCell::new(HashMap::new()),
            workspace,
            executor,
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
    pub(super) fn job(&self, id: &str) -> Option<&Job> {
        self.pipeline.job(id)
    }

    /// Borrow the run's materialized workspace path.
    pub(super) fn workspace(&self) -> &std::path::Path {
        &self.workspace
    }

    /// In docker mode, return the `(container_id, work_dir)` used to
    /// route `(sh …)` through `docker exec`. Host mode returns `None`.
    pub(super) fn docker_target(&self) -> Option<(&str, &str)> {
        match &self.executor {
            ExecutorRuntime::Host => None,
            ExecutorRuntime::Docker(l) => Some((&l.session.container_id, &l.work_dir)),
        }
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

    /// Drain all recorded sh timings, returning them keyed by job id.
    pub(super) fn take_sh_timings(&self) -> HashMap<String, ShTimings> {
        std::mem::take(&mut *self.sh_timings.borrow_mut())
    }

    /// Resolve a declared secret by name, caching it for redaction.
    /// Errors if the name isn't declared or the secret's source
    /// can't be read.
    pub(super) fn secret(&self, name: &str) -> super::error::Result<String> {
        self.registry.borrow_mut().resolve(name)
    }

    /// Run `cmd` with `opts` and record its output against the
    /// current job (if one is active). Non-zero exits come back in
    /// `:exit`, not as `Err`.
    ///
    /// In docker mode the command is wrapped in `docker exec` against
    /// the per-run container; the bind-mounted workspace is the
    /// container's working directory, and `opts.env` is forwarded as
    /// `-e KEY=VAL` flags.
    pub(super) fn sh(&self, cmd: Cmd, opts: ShOpts) -> super::error::Result<ShOutput> {
        let started_at = Timestamp::now();
        let output = match self.docker_target() {
            None => {
                let program = cmd.program().to_string();
                cmd.run(opts, &self.workspace).map_err(|e| {
                    super::error::Error::CommandSpawnFailed {
                        program,
                        cwd: self.workspace.clone(),
                        source: e,
                    }
                })?
            }
            Some((container_id, work_dir)) => {
                let wrapped = Cmd::wrap_in_docker_exec(cmd, container_id, work_dir, &opts);
                let program = wrapped.program().to_string();
                // env was embedded into the docker exec argv; clear it
                // so it isn't also set on the host `docker` process.
                wrapped
                    .run(ShOpts::default(), &self.workspace)
                    .map_err(|e| super::error::Error::CommandSpawnFailed {
                        program,
                        cwd: self.workspace.clone(),
                        source: e,
                    })?
            }
        };
        let finished_at = Timestamp::now();
        if let Some(job) = self.current_job.borrow().as_ref() {
            let n = {
                let mut counter = self.sh_counter.borrow_mut();
                let entry = counter.entry(job.clone()).or_insert(1);
                let n = *entry;
                *entry += 1;
                n
            };
            self.outputs
                .borrow_mut()
                .entry(job.clone())
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
                .entry(job.clone())
                .or_default()
                .push((n, started_at, finished_at));
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
            registry: RefCell::new(SecretRegistry::new(secrets)),
            inputs: HashMap::new(),
            current_job: RefCell::new(None),
            outputs: RefCell::new(HashMap::new()),
            sh_timings: RefCell::new(HashMap::new()),
            sh_counter: RefCell::new(HashMap::new()),
            workspace: std::env::current_dir().expect("cwd"),
            executor: ExecutorRuntime::Host,
        }
    }
}

/// `IntoLua` carrier for an `Rc<Runtime>`. Stows the Rc on the VM as
/// app data and returns the handle table — `{sh, secret, jobs}`.
pub(super) struct RuntimeHandle(pub Rc<Runtime>);

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
pub(super) enum Cmd {
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
    /// Build a new `Cmd` that invokes `docker exec` against the given
    /// container. Embeds `cwd` as `--workdir` and `opts.env` as
    /// repeated `-e KEY=VALUE` flags, so the caller's `ShOpts` after
    /// wrapping should be empty.
    pub(super) fn wrap_in_docker_exec(
        inner: Cmd,
        container_id: &str,
        work_dir: &str,
        opts: &ShOpts,
    ) -> Cmd {
        // `--interactive` keeps stdin attached. No `--tty` so stdout
        // and stderr stay as separate streams.
        let mut args: Vec<String> = vec![
            "exec".to_string(),
            "--interactive".to_string(),
            "--workdir".to_string(),
            work_dir.to_string(),
        ];
        for (k, v) in &opts.env {
            args.push("--env".to_string());
            args.push(format!("{k}={v}"));
        }
        args.push(container_id.to_string());

        match inner {
            Cmd::Argv {
                program,
                args: inner_args,
            } => {
                args.push(program);
                args.extend(inner_args);
            }
            Cmd::Shell(s) => {
                args.push("sh".to_string());
                args.push("-c".to_string());
                args.push(s);
            }
        }
        Cmd::Argv {
            program: "docker".to_string(),
            args,
        }
    }

    pub(super) fn run(self, opts: ShOpts, cwd: &std::path::Path) -> std::io::Result<ShOutput> {
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
pub(super) struct ShOpts {
    pub(super) env: HashMap<String, String>,
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
    fn cmd_wrap_in_docker_exec_argv_form() {
        let inner = Cmd::Argv {
            program: "echo".to_string(),
            args: vec!["hi".to_string()],
        };
        let mut opts = ShOpts::default();
        opts.env.insert("FOO".to_string(), "bar".to_string());

        let wrapped = Cmd::wrap_in_docker_exec(inner, "abc123", "/work", &opts);
        let Cmd::Argv { program, args } = wrapped else {
            panic!("expected argv form");
        };
        assert_eq!(program, "docker");
        assert_eq!(
            args,
            vec![
                "exec",
                "--interactive",
                "--workdir",
                "/work",
                "--env",
                "FOO=bar",
                "abc123",
                "echo",
                "hi",
            ]
        );
    }

    #[test]
    fn cmd_wrap_in_docker_exec_shell_form() {
        let inner = Cmd::Shell("echo hi | tr a-z A-Z".to_string());
        let opts = ShOpts::default();

        let wrapped = Cmd::wrap_in_docker_exec(inner, "abc123", "/work", &opts);
        let Cmd::Argv { program, args } = wrapped else {
            panic!("expected argv form");
        };
        assert_eq!(program, "docker");
        assert_eq!(
            args,
            vec![
                "exec",
                "--interactive",
                "--workdir",
                "/work",
                "abc123",
                "sh",
                "-c",
                "echo hi | tr a-z A-Z",
            ]
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
