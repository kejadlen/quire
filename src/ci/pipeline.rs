//! CI job graph: evaluation of `ci.fnl` and validation rules.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use miette::{NamedSource, SourceSpan};
use mlua::{Lua, LuaSerdeExt};

use crate::Result;
use crate::fennel::Fennel;
use crate::secret::SecretString;

/// A registered job extracted from ci.fnl.
///
/// Constructed via `Job::new`, which enforces the per-job validation
/// rules (reserved-slash, empty-inputs). Holding a `Job` is proof that
/// those rules are satisfied; the post-graph rules (cycles, reachability)
/// are checked later by `validate_post_graph`.
#[derive(Debug)]
pub struct Job {
    pub id: String,
    pub inputs: Vec<String>,
    /// Span covering the `(ci.job …)` call site. Used as the label
    /// location for both per-job and post-graph diagnostics.
    pub(crate) span: SourceSpan,
    /// The job's run function from the Lua VM.
    /// Currently exercised only from tests until the runtime executor lands.
    #[allow(dead_code)]
    pub(crate) run_fn: mlua::Function,
}

impl Job {
    /// Build a `Job` from the raw `(ci.job …)` arguments, applying the
    /// per-job validation rules. `line` is the 1-indexed source line of
    /// the call site; `source` is the full Fennel source string used to
    /// compute the diagnostic span.
    fn new(
        id: String,
        inputs: Vec<String>,
        run_fn: mlua::Function,
        line: u32,
        source: &str,
    ) -> std::result::Result<Self, ValidationError> {
        let span = span_for_line(source, line);

        if id.contains('/') {
            return Err(ValidationError::ReservedSlash { job_id: id, span });
        }

        if inputs.is_empty() {
            return Err(ValidationError::EmptyInputs { job_id: id, span });
        }

        Ok(Self {
            id,
            inputs,
            span,
            run_fn,
        })
    }
}

/// A validated CI pipeline — a job graph that has passed all
/// structural rules.
///
/// Obtain via `pipeline::load`, which parses the Fennel source and
/// validates the result. Holding a `Pipeline` is proof that the graph
/// is sound.
pub struct Pipeline {
    jobs: Vec<Job>,
}

impl Pipeline {
    pub fn jobs(&self) -> &[Job] {
        &self.jobs
    }
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

/// Pull the `CiModule` off the Lua VM's app data. Errors with a
/// reasonable message if `install` was never called — should be
/// impossible in practice but worth surfacing if it ever happens.
fn module(lua: &Lua) -> mlua::Result<mlua::AppDataRef<'_, CiModule>> {
    lua.app_data_ref::<CiModule>()
        .ok_or_else(|| mlua::Error::external("quire.ci module not installed on Lua VM"))
}

/// Body of `(ci.job id inputs run-fn)`. Captures the call-site line
/// from the Lua debug stack so per-job validation errors carry a span
/// pointing back at the user's source.
fn register_job(
    lua: &Lua,
    (id, inputs, run_fn): (String, Vec<String>, mlua::Function),
) -> mlua::Result<()> {
    let m = module(lua)?;
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
    let m = module(lua)?;
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
    fn run(self, opts: ShOpts) -> std::io::Result<Output> {
        let mut command: std::process::Command = self.into();
        for (k, v) in opts.env {
            command.env(k, v);
        }
        if let Some(cwd) = opts.cwd {
            command.current_dir(cwd);
        }
        let output = command.output()?;
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
                Err(mlua::Error::FromLuaConversionError {
                    from: "table",
                    to: "Cmd".into(),
                    message: Some("ci.sh: argv list is empty".into()),
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

/// Parse and validate a ci.fnl source string into a `Pipeline`.
///
/// Injects `quire.ci` into `package.loaded` so scripts can
/// `(require :quire.ci)`, evaluates the source to register jobs, runs
/// the per-job pre-graph rules during registration, and then runs
/// the post-graph rules over the surviving jobs. Any errors found are
/// gathered into a single `LoadError` carrying the source for miette
/// to render with inline labels.
pub(crate) fn load(
    fennel: &Fennel,
    source: &str,
    filename: &str,
    display: &str,
    secrets: HashMap<String, SecretString>,
) -> Result<Pipeline> {
    let results = parse(fennel, source, filename, display, secrets)?;

    let mut errors = Vec::new();
    let mut jobs = Vec::new();
    for r in results {
        match r {
            Ok(j) => jobs.push(j),
            Err(e) => errors.push(e),
        }
    }

    if let Err(post) = validate_post_graph(&jobs) {
        errors.extend(post);
    }

    if errors.is_empty() {
        Ok(Pipeline { jobs })
    } else {
        Err(LoadError {
            src: NamedSource::new(display, source.to_string()),
            errors,
        }
        .into())
    }
}

/// Evaluate `source` with the `quire.ci` module bound and collect the
/// registration results — one `Result` per `(ci.job …)` call. Pre-graph
/// rules run inside the callback, so a single bad job does not abort
/// the rest of the script.
fn parse(
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

/// Compute a span covering the given 1-indexed line in `source`.
/// Returns an empty span at offset 0 when the line is unknown.
fn span_for_line(source: &str, line: u32) -> SourceSpan {
    if line == 0 {
        return SourceSpan::from((0, 0));
    }
    let target = line as usize;
    let mut current = 1usize;
    for (i, ch) in source.char_indices() {
        if current == target {
            let end = source[i..]
                .find('\n')
                .map(|n| i + n)
                .unwrap_or(source.len());
            return SourceSpan::from((i, end - i));
        }
        if ch == '\n' {
            current += 1;
        }
    }
    SourceSpan::from((source.len(), 0))
}

/// A validation error found in the job graph.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ValidationError {
    #[error("Cycle detected among jobs: {}", cycle_jobs.join(", "))]
    Cycle {
        cycle_jobs: Vec<String>,
        #[label(collection, "in cycle")]
        spans: Vec<SourceSpan>,
    },

    #[error(
        "Job '{job_id}' has empty inputs. Pass [:quire/push] (or another input) so it has something to fire it."
    )]
    EmptyInputs {
        job_id: String,
        #[label("declared here")]
        span: SourceSpan,
    },

    #[error("Job '{job_id}' is not reachable from any source ref (e.g. :quire/push).")]
    Unreachable {
        job_id: String,
        #[label("declared here")]
        span: SourceSpan,
    },

    #[error("Job id '{job_id}' contains '/', which is reserved for the 'quire/' source namespace.")]
    ReservedSlash {
        job_id: String,
        #[label("declared here")]
        span: SourceSpan,
    },
}

/// All validation errors produced while loading a ci.fnl, paired with
/// the source so miette can render inline labels for each per-job
/// error.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("CI validation failed")]
pub struct LoadError {
    // Named `src` rather than `source` so thiserror doesn't auto-treat
    // it as the error chain.
    #[source_code]
    pub src: NamedSource<String>,

    #[related]
    pub errors: Vec<ValidationError>,
}

/// Run the post-graph validation rules — cycle detection and source
/// reachability — over the surviving jobs from parsing.
///
/// Per-job pre-graph rules (slash-in-id, empty inputs) run inside the
/// `(ci.job …)` callback during `parse`, so they are not re-checked here.
fn validate_post_graph(jobs: &[Job]) -> std::result::Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    // Rule 1: acyclic.
    //
    // Build a directed graph where edges point from dependency to
    // dependent. Source refs (e.g. "quire/push") are not nodes.
    let mut graph: petgraph::Graph<&str, ()> = petgraph::Graph::new();
    let mut node_map: std::collections::HashMap<&str, petgraph::graph::NodeIndex> =
        std::collections::HashMap::new();

    for job in jobs {
        let idx = graph.add_node(job.id.as_str());
        node_map.insert(job.id.as_str(), idx);
    }

    for job in jobs {
        let dependent = node_map[job.id.as_str()];
        for input in &job.inputs {
            if let Some(&dependency) = node_map.get(input.as_str()) {
                graph.add_edge(dependency, dependent, ());
            }
        }
    }

    // Each non-trivial strongly connected component is a distinct cycle.
    // A single-node SCC is only a cycle if it has a self-edge.
    for scc in petgraph::algo::tarjan_scc(&graph) {
        let is_cycle = scc.len() > 1 || (scc.len() == 1 && graph.contains_edge(scc[0], scc[0]));
        if !is_cycle {
            continue;
        }
        let mut members: Vec<&Job> = scc
            .iter()
            .filter_map(|&idx| jobs.iter().find(|j| j.id == graph[idx]))
            .collect();
        members.sort_by(|a, b| a.id.cmp(&b.id));
        let cycle_jobs = members.iter().map(|j| j.id.clone()).collect();
        let spans = members.iter().map(|j| j.span).collect();
        errors.push(ValidationError::Cycle { cycle_jobs, spans });
    }

    // Rule 3: reachability — every job's transitive inputs must include a source ref.
    let is_source = |name: &str| name.starts_with("quire/");

    for job in jobs {
        let mut visited = std::collections::HashSet::new();
        let mut stack: Vec<&str> = job.inputs.iter().map(|s| s.as_str()).collect();
        let mut found_source = false;

        while let Some(name) = stack.pop() {
            if !visited.insert(name) {
                continue;
            }
            if is_source(name) {
                found_source = true;
                break;
            }
            if let Some(upstream) = jobs.iter().find(|j| j.id == name) {
                for input in &upstream.inputs {
                    stack.push(input.as_str());
                }
            }
        }

        if !found_source {
            errors.push(ValidationError::Unreachable {
                job_id: job.id.clone(),
                span: job.span,
            });
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fennel() -> Fennel {
        Fennel::new().expect("Fennel::new() should succeed")
    }

    #[test]
    fn load_registers_a_job() {
        let f = fennel();
        let source = r#"(local ci (require :quire.ci))
(ci.job :test [:quire/push] (fn [_] nil))"#;
        let pipeline = load(&f, source, "ci.fnl", "ci.fnl", HashMap::new()).expect("load should succeed");
        let jobs = pipeline.jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "test");
        assert_eq!(jobs[0].inputs, vec!["quire/push"]);
    }

    #[test]
    fn load_registers_multiple_jobs() {
        let f = fennel();
        let source = r#"
(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [_] nil))
(ci.job :test [:build] (fn [_] nil))
"#;
        let pipeline = load(&f, source, "ci.fnl", "ci.fnl", HashMap::new()).expect("load should succeed");
        let jobs = pipeline.jobs();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].id, "build");
        assert_eq!(jobs[0].inputs, vec!["quire/push"]);
        assert_eq!(jobs[1].id, "test");
        assert_eq!(jobs[1].inputs, vec!["build"]);
    }

    #[test]
    fn load_captures_source_line() {
        let f = fennel();
        let source = "(local ci (require :quire.ci))
(ci.job :first [:quire/push] (fn [_] nil))
(ci.job :second [:quire/push] (fn [_] nil))


(ci.job :sixth [:quire/push] (fn [_] nil))";
        let pipeline = load(&f, source, "ci.fnl", "ci.fnl", HashMap::new()).expect("load should succeed");
        let lines: Vec<usize> = pipeline
            .jobs()
            .iter()
            .map(|j| 1 + source[..j.span.offset()].matches('\n').count())
            .collect();
        assert_eq!(lines, vec![2, 3, 6]);
    }

    #[test]
    fn load_errors_on_bad_fennel() {
        let f = fennel();
        let result = load(&f, "{:bad {:}", "ci.fnl", "ci.fnl", HashMap::new());
        assert!(result.is_err(), "malformed Fennel should fail");
    }

    /// Parse a Fennel source into per-job results. Pre-graph rules
    /// run during parsing, so each entry is `Ok(Job)` or
    /// `Err(ValidationError)`.
    fn parse_results(source: &str) -> Vec<std::result::Result<Job, ValidationError>> {
        let f = fennel();
        parse(&f, source, "ci.fnl", "ci.fnl", HashMap::new()).expect("parse should succeed")
    }

    /// Discard parse errors and return only the successfully registered
    /// jobs — for tests that exercise post-graph rules.
    fn parsed_jobs(source: &str) -> Vec<Job> {
        parse_results(source)
            .into_iter()
            .filter_map(|r| r.ok())
            .collect()
    }

    #[test]
    fn validate_accepts_valid_config() {
        let jobs = parsed_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [_] nil))
(ci.job :test [:build :quire/push] (fn [_] nil))
"#,
        );
        assert!(validate_post_graph(&jobs).is_ok());
    }

    #[test]
    fn validate_rejects_cycle() {
        let jobs = parsed_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :a [:b] (fn [_] nil))
(ci.job :b [:a] (fn [_] nil))
"#,
        );
        let errs = validate_post_graph(&jobs).unwrap_err();
        assert!(
            errs.iter().any(|e| matches!(e, ValidationError::Cycle { cycle_jobs, .. } if cycle_jobs.contains(&"a".to_string()) && cycle_jobs.contains(&"b".to_string()))),
            "should report a cycle involving a and b: {errs:?}"
        );
    }

    #[test]
    fn validate_cycle_only_reports_cycle_members() {
        let jobs = parsed_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :a [:b :quire/push] (fn [_] nil))
(ci.job :b [:a :quire/push] (fn [_] nil))
(ci.job :clean [:quire/push] (fn [_] nil))
"#,
        );
        let errs = validate_post_graph(&jobs).unwrap_err();
        let cycle_errs: Vec<&Vec<String>> = errs
            .iter()
            .filter_map(|e| match e {
                ValidationError::Cycle { cycle_jobs, .. } => Some(cycle_jobs),
                _ => None,
            })
            .collect();
        assert_eq!(
            cycle_errs.len(),
            1,
            "expected exactly one cycle error: {errs:?}"
        );
        assert_eq!(cycle_errs[0], &vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn validate_reports_disjoint_cycles_separately() {
        let jobs = parsed_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :a [:b :quire/push] (fn [_] nil))
(ci.job :b [:a :quire/push] (fn [_] nil))
(ci.job :c [:d :quire/push] (fn [_] nil))
(ci.job :d [:c :quire/push] (fn [_] nil))
"#,
        );
        let errs = validate_post_graph(&jobs).unwrap_err();
        let cycle_count = errs
            .iter()
            .filter(|e| matches!(e, ValidationError::Cycle { .. }))
            .count();
        assert_eq!(cycle_count, 2, "expected two cycle errors: {errs:?}");
    }

    #[test]
    fn parse_rejects_empty_inputs() {
        let results = parse_results(
            r#"(local ci (require :quire.ci))
(ci.job :setup [] (fn [_] nil))"#,
        );
        assert!(
            results.iter().any(
                |r| matches!(r, Err(ValidationError::EmptyInputs { job_id, .. }) if job_id == "setup")
            ),
            "should report empty inputs for 'setup': {results:?}"
        );
    }

    #[test]
    fn parse_rejects_slash_in_job_id() {
        let results = parse_results(
            r#"(local ci (require :quire.ci))
(ci.job :foo/bar [:quire/push] (fn [_] nil))"#,
        );
        assert!(
            results.iter().any(
                |r| matches!(r, Err(ValidationError::ReservedSlash { job_id, .. }) if job_id == "foo/bar")
            ),
            "should report slash in job id: {results:?}"
        );
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
        let pipeline = load(&f, source, "ci.fnl", "ci.fnl", secrets)
            .expect("load should succeed");
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
    /// the same VM via lua.from_value. Owned data, so the Fennel VM can
    /// drop without a use-after-free.
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

    #[test]
    fn validate_rejects_unreachable_jobs() {
        // An orphan with non-empty self-input passes pre-graph rules
        // and reaches the post-graph reachability check.
        let jobs = parsed_jobs(
            r#"(local ci (require :quire.ci))
(ci.job :orphan [:orphan] (fn [_] nil))"#,
        );
        let errs = validate_post_graph(&jobs).unwrap_err();
        assert!(
            errs.iter().any(
                |e| matches!(e, ValidationError::Unreachable { job_id, .. } if job_id == "orphan")
            ),
            "should report unreachable job 'orphan': {errs:?}"
        );
    }
}
