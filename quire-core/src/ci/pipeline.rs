//! CI job graph: validation rules and the [`compile`] entry point
//! that turns a `ci.fnl` source string into a [`Pipeline`].
//!
//! Lua/Fennel evaluation lives in the sibling [`super::registration`]
//! module; this module owns the domain types and the structural rules.

use std::collections::{HashMap, HashSet};

use miette::{NamedSource, SourceSpan};
use petgraph::Graph;
use petgraph::graph::NodeIndex;
use petgraph::visit::{Bfs, Reversed};

use super::registration::{self, Registrations};
use crate::fennel::{Fennel, FennelError};

/// A registration-time error caught while individual `(ci.job …)` and
/// `(ci.image …)` calls are being processed.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum DefinitionError {
    #[error(
        "Job '{job_id}' has empty inputs. Pass [:quire/push] (or another trigger) so it has something to fire it."
    )]
    EmptyInputs {
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

    #[error("Pipeline image declared more than once.")]
    DuplicateImage {
        #[label("duplicate image declaration")]
        span: SourceSpan,
    },

    #[error("Job '{job_id}' is registered more than once.")]
    DuplicateJob {
        job_id: String,
        #[label("duplicate registration")]
        span: SourceSpan,
    },
}

/// A post-graph structural error found after all jobs have been
/// registered and the dependency graph is built.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum StructureError {
    #[error("Cycle detected among jobs: {}", cycle_jobs.join(", "))]
    Cycle {
        cycle_jobs: Vec<String>,
        #[label(collection, "in cycle")]
        spans: Vec<SourceSpan>,
    },

    #[error("Job '{job_id}' is not reachable from any trigger (e.g. :quire/push).")]
    Unreachable {
        job_id: String,
        #[label("declared here")]
        span: SourceSpan,
    },
}

/// A single diagnostic from pipeline compilation. Wraps the two
/// error categories — definition-time and structure-time — so miette
/// can iterate them via `#[related]`.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum Diagnostic {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Definition(#[from] DefinitionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Structure(#[from] StructureError),
}

/// Edges point from dependency to dependent. Node weights are the
/// `Job` values themselves; source refs (e.g. `quire/push`) are not
/// nodes in this graph.
type JobGraph = Graph<Job, ()>;

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
    /// Span covering the `(ci.job …)` call site. `None` for built-in
    /// source jobs (e.g. `quire/push`) registered by `compile` rather
    /// than user code — they have no call site to point at. Diagnostic
    /// labels just elide themselves for these.
    pub span: Option<SourceSpan>,
    /// What to run when the executor reaches this job.
    pub run_fn: RunFn,
}

/// A Rust-side run-fn: a closure invoked synchronously by the
/// executor with the runtime in scope.
pub type RustRunFn =
    std::rc::Rc<dyn Fn(&super::runtime::Runtime) -> super::runtime::RuntimeResult<()>>;

/// How a job runs at execute time.
///
/// `Lua` is the user case: a Fennel function the executor calls
/// through the Lua VM, passing the runtime handle table. `Rust` is
/// the built-in case: a closure that receives the runtime directly,
/// used by helpers that do their work in plain Rust without
/// round-tripping through Lua.
///
/// Both variants are `Clone` so the executor can take an owned copy
/// before invoking — `mlua::Function` is cheap to clone (a registry
/// handle); the `Rc` makes the `Rust` variant cheap too.
#[derive(Clone)]
pub enum RunFn {
    Lua(mlua::Function),
    #[allow(dead_code)] // Wired up by built-in helpers.
    Rust(RustRunFn),
}

impl std::fmt::Debug for RunFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunFn::Lua(_) => f.debug_tuple("Lua").field(&"<lua function>").finish(),
            RunFn::Rust(_) => f.debug_tuple("Rust").field(&"<rust closure>").finish(),
        }
    }
}

impl Job {
    /// Build a `Job`, applying the rules that apply to every job
    /// regardless of how it was registered. `line` is the 1-indexed
    /// source line of the call site; `source` is the full Fennel
    /// source string used to compute the diagnostic span.
    ///
    /// The `quire/`-namespace check is the caller's responsibility —
    /// user-facing `(ci.job …)` calls must reject slashes (see
    /// `register_job`), but internal helpers (e.g. `register_mirror`)
    /// legitimately register jobs at `quire/<name>` and skip that
    /// rule.
    ///
    /// Visible to the sibling `registration` module which constructs
    /// jobs from the registration callbacks.
    pub fn new(
        id: String,
        inputs: Vec<String>,
        run_fn: RunFn,
        line: u32,
        source: &str,
    ) -> std::result::Result<Self, DefinitionError> {
        let span = span_for_line(source, line);

        if inputs.is_empty() {
            return Err(DefinitionError::EmptyInputs { job_id: id, span });
        }

        Ok(Self {
            id,
            inputs,
            span: Some(span),
            run_fn,
        })
    }
}

/// A validated CI pipeline — a job graph that has passed all
/// structural rules.
///
/// Obtain via [`compile`], which evaluates the Fennel source and
/// validates the result. Holding a `Pipeline` is proof that the graph
/// is sound.
///
/// Owns the Fennel/Lua VM so the registered `run_fn`s remain callable
/// after `compile` returns.
pub struct Pipeline {
    /// Jobs and dependencies in one structure: nodes own `Job` values,
    /// edges go from dependency to dependent. Replaces the old pair of
    /// `Vec<Job>` plus `Graph<usize, ()>` (node weights as vec indices).
    graph: JobGraph,
    /// Job id → node index, for O(1) lookup by id.
    by_id: HashMap<String, NodeIndex>,
    fennel: Fennel,
    /// Container image declared via `(ci.image "...")`, if any.
    image: Option<String>,
    /// The original Fennel source — kept so runtime Lua errors raised
    /// during job execution can be re-wrapped via
    /// [`FennelError::from_lua`] with the same source-code annotation
    /// that compile-time errors get.
    ///
    /// [`FennelError::from_lua`]: crate::fennel::FennelError::from_lua
    source: String,
    /// The source's display name (typically the .fnl path or a
    /// synthetic label like `HEAD:.quire/ci.fnl`).
    source_name: String,
}

impl Pipeline {
    /// Jobs in topological order — dependencies before dependents.
    /// The pipeline is validated as acyclic, so toposort never fails.
    /// This is the only order callers should iterate in; registration
    /// order isn't exposed because nothing relies on it.
    pub fn jobs(&self) -> Vec<&Job> {
        petgraph::algo::toposort(&self.graph, None)
            .expect("pipeline is validated as acyclic")
            .into_iter()
            .map(|idx| &self.graph[idx])
            .collect()
    }

    /// Number of registered jobs.
    pub fn job_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Look up a job by id.
    pub fn job(&self, id: &str) -> Option<&Job> {
        self.by_id.get(id).map(|&idx| &self.graph[idx])
    }

    /// Borrow the underlying Fennel/Lua VM.
    pub fn fennel(&self) -> &Fennel {
        &self.fennel
    }

    /// The container image declared via `(ci.image ...)`, if any.
    /// The executor resolves the final image at run time:
    /// declared image → `.quire/Dockerfile` → default.
    pub fn image(&self) -> Option<&str> {
        self.image.as_deref()
    }

    /// The original Fennel source. Held so the executor can attach
    /// source context to runtime Lua errors via
    /// [`FennelError::from_lua`].
    ///
    /// [`FennelError::from_lua`]: crate::fennel::FennelError::from_lua
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The source's display name (path or synthetic label).
    pub fn source_name(&self) -> &str {
        &self.source_name
    }

    /// For each job, the set of ancestor job ids reachable through the
    /// input graph. The job's own id is not included.
    ///
    /// Used by the executor to validate `(jobs name)` lookups: the
    /// calling job may only read outputs from names in its set. Built-in
    /// sources (like `quire/push`) are real graph nodes, so they appear
    /// in this set the same way user jobs do.
    pub fn transitive_inputs(&self) -> HashMap<String, HashSet<String>> {
        let reversed = Reversed(&self.graph);
        let mut result: HashMap<String, HashSet<String>> = HashMap::new();
        for (start_id, &start) in &self.by_id {
            let mut reachable = HashSet::new();
            let mut bfs = Bfs::new(reversed, start);
            while let Some(idx) = bfs.next(reversed) {
                if idx != start {
                    reachable.insert(self.graph[idx].id.clone());
                }
            }
            result.insert(start_id.clone(), reachable);
        }
        result
    }
}

impl Pipeline {
    /// Replace the first job's run-fn — for tests that need to
    /// exercise a `RunFn::Rust` execution path without building the
    /// full helper machinery (which doesn't exist yet).
    #[doc(hidden)]
    pub fn replace_first_run_fn(&mut self, run_fn: RunFn) {
        if let Some(job) = self.graph.node_weights_mut().next() {
            job.run_fn = run_fn;
        }
    }
}

/// Build the dependency graph by consuming `jobs` into graph nodes.
/// Inputs that don't match a known job id are treated as source refs
/// (e.g. `quire/push`) and don't get an edge — they're not nodes in
/// this graph.
fn build_graph(jobs: Vec<Job>) -> (JobGraph, HashMap<String, NodeIndex>) {
    let mut graph = JobGraph::new();
    let mut by_id = HashMap::with_capacity(jobs.len());
    for job in jobs {
        let id = job.id.clone();
        let idx = graph.add_node(job);
        by_id.insert(id, idx);
    }
    // `node_indices()` walks insertion order, giving deterministic edge
    // ordering. Snapshotting to a Vec releases the immutable graph
    // borrow before we mutate it via `add_edge`.
    let dependents: Vec<NodeIndex> = graph.node_indices().collect();
    for dependent in dependents {
        let inputs = graph[dependent].inputs.clone();
        for input in inputs {
            if let Some(&dependency) = by_id.get(&input) {
                graph.add_edge(dependency, dependent, ());
            }
        }
    }
    (graph, by_id)
}

/// Compute a span covering the given 1-indexed line in `source`.
/// Returns an empty span at offset 0 when the line is unknown.
pub fn span_for_line(source: &str, line: u32) -> SourceSpan {
    if line == 0 {
        return SourceSpan::from((0, 0)); // cov-excl-line
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
    SourceSpan::from((source.len(), 0)) // cov-excl-line
}

/// All diagnostics produced while compiling a ci.fnl, paired with
/// the source so miette can render inline labels for each diagnostic.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("ci.fnl has errors")]
pub struct PipelineError {
    // Named `src` rather than `source` so thiserror doesn't auto-treat
    // it as the error chain.
    #[source_code]
    pub src: NamedSource<String>,

    #[related]
    pub diagnostics: Vec<Diagnostic>,
}

/// Errors from [`compile`] — Fennel evaluation failures and pipeline-shape
/// failures unified at the compile boundary, so callers can match on
/// the compile result without reaching into the kitchen-sink
/// `ci::Error`.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum CompileError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Fennel(#[from] Box<FennelError>),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Pipeline(#[from] Box<PipelineError>),
}

impl From<FennelError> for CompileError {
    fn from(err: FennelError) -> Self {
        Self::Fennel(Box::new(err))
    }
}

impl From<PipelineError> for CompileError {
    fn from(err: PipelineError) -> Self {
        Self::Pipeline(Box::new(err))
    }
}

pub type CompileResult<T> = std::result::Result<T, CompileError>;

/// The built-in `quire/push` job: a real graph node that downstream
/// user jobs depend on by listing `:quire/push` in their inputs.
///
/// The run-fn is a no-op — the runtime pre-populates each downstream
/// job's input view with push data taken from the `RunMeta` it was
/// constructed with. The job exists in the graph so reachability,
/// validation, and `(jobs "quire/push")` lookups all use the same
/// node-based machinery as user jobs, with no "synthetic source ref"
/// concept to special-case.
fn builtin_push_job() -> Job {
    Job {
        id: "quire/push".to_string(),
        inputs: Vec::new(),
        span: None,
        run_fn: RunFn::Rust(std::rc::Rc::new(|_| Ok(()))),
    }
}

/// Compile a ci.fnl source string into a validated [`Pipeline`].
///
/// Two phases, fail-fast between them: [`registration::register`]
/// evaluates the script and reports any definition-time errors, then
/// [`validate_post_graph`] checks the dependency graph. Errors from a
/// phase are wrapped in a [`PipelineError`] for miette to render with
/// inline labels.
///
/// Compile failures are user-pipeline problems, not operational
/// errors — callers should surface them in the run UI rather than
/// emitting `tracing::error!` events. This function intentionally does
/// not log; doing so would route every malformed `ci.fnl` to Sentry.
pub fn compile(source: &str, name: &str) -> CompileResult<Pipeline> {
    let fennel = Fennel::new()?;
    let Registrations { mut jobs, image } = registration::register(&fennel, source, name)?;

    // Append the built-in push job so it's a real node in the
    // dependency graph. Inputs like `:quire/push` resolve to edges
    // against this node; nothing in the rest of the pipeline has to
    // special-case "is this a source ref" anymore. Position in the
    // Vec doesn't matter — the push job has no inputs, so topo order
    // already puts it ahead of any dependent.
    jobs.push(builtin_push_job());

    let (graph, by_id) = build_graph(jobs);

    if let Err(errors) = validate_post_graph(&graph) {
        return Err(PipelineError {
            src: NamedSource::new(name, source.to_string()),
            diagnostics: errors.into_iter().map(Diagnostic::Structure).collect(),
        }
        .into());
    }

    Ok(Pipeline {
        graph,
        by_id,
        fennel,
        image,
        source: source.to_string(),
        source_name: name.to_string(),
    })
}

/// Run the post-graph validation rules — cycle detection and source
/// reachability — over the surviving jobs from registration.
///
/// Per-job pre-graph rules (slash-in-id, empty inputs) run inside the
/// `(ci.job …)` callback during `registration::register`, so they are
/// not re-checked here.
fn validate_post_graph(graph: &JobGraph) -> std::result::Result<(), Vec<StructureError>> {
    let mut errors = Vec::new();
    let mut cycle_members: std::collections::HashSet<&str> = std::collections::HashSet::new();

    // Acyclic. Each non-trivial strongly connected component is a
    // distinct cycle. A single-node SCC is only a cycle if it has a
    // self-edge.
    for scc in petgraph::algo::tarjan_scc(graph) {
        let is_cycle = scc.len() > 1 || (scc.len() == 1 && graph.contains_edge(scc[0], scc[0]));
        if !is_cycle {
            continue;
        }
        let mut members: Vec<&Job> = scc.iter().map(|&idx| &graph[idx]).collect();
        members.sort_by(|a, b| a.id.cmp(&b.id));
        for j in &members {
            cycle_members.insert(j.id.as_str());
        }
        let cycle_jobs = members.iter().map(|j| j.id.clone()).collect();
        // Triggers can't be in cycles (no inputs → no outgoing edges),
        // so every member here has a span. filter_map is defensive.
        let spans = members.iter().filter_map(|j| j.span).collect();
        errors.push(StructureError::Cycle { cycle_jobs, spans });
    }

    // Reachability — every job must transitively walk back to a
    // trigger node (one with empty inputs, like `quire/push`).
    // Triggers themselves are trivially reachable.
    //
    // Walking the reversed graph (incoming edges) lets us find ancestors
    // without re-resolving string ids against the input vectors. An
    // unresolved input — a job that lists `:typo` where no `typo` job
    // exists — produces no edge in `build_graph`, so the BFS just
    // stops short and `found_trigger` stays false.
    for node in graph.node_indices() {
        let job = &graph[node];
        if cycle_members.contains(job.id.as_str()) {
            continue;
        }
        if job.inputs.is_empty() {
            // Trigger node: trivially reachable.
            continue;
        }
        let reversed = Reversed(graph);
        let mut bfs = Bfs::new(reversed, node);
        let mut found_trigger = false;
        while let Some(idx) = bfs.next(reversed) {
            if graph[idx].inputs.is_empty() {
                found_trigger = true;
                break;
            }
        }

        if !found_trigger {
            // We `continue`'d above on `job.inputs.is_empty()`, so this
            // branch only fires for user jobs — which always have a span.
            errors.push(StructureError::Unreachable {
                job_id: job.id.clone(),
                span: job.span.expect("user jobs always have a span"),
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

    /// Jobs registered by the ci.fnl, excluding the built-in
    /// `quire/push` trigger that `compile` prepends to every pipeline.
    fn user_jobs(pipeline: &Pipeline) -> Vec<&Job> {
        pipeline
            .jobs()
            .into_iter()
            .filter(|j| j.span.is_some())
            .collect()
    }

    #[test]
    fn compile_registers_a_job() {
        let source = r#"(local ci (require :quire.ci))
(ci.job :test [:quire/push] (fn [] nil))"#;
        let pipeline = compile(source, "ci.fnl").expect("compile should succeed");
        let jobs = user_jobs(&pipeline);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "test");
        assert_eq!(jobs[0].inputs, vec!["quire/push"]);
    }

    #[test]
    fn compile_registers_multiple_jobs() {
        let source = r#"
(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [] nil))
(ci.job :test [:build] (fn [] nil))
"#;
        let pipeline = compile(source, "ci.fnl").expect("compile should succeed");
        // Topological order among user jobs: build (depends only on
        // quire/push) before test.
        let jobs = user_jobs(&pipeline);
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].id, "build");
        assert_eq!(jobs[0].inputs, vec!["quire/push"]);
        assert_eq!(jobs[1].id, "test");
        assert_eq!(jobs[1].inputs, vec!["build"]);
    }

    #[test]
    fn compile_captures_source_line() {
        let source = "(local ci (require :quire.ci))
(ci.job :first [:quire/push] (fn [] nil))
(ci.job :second [:quire/push] (fn [] nil))


(ci.job :sixth [:quire/push] (fn [] nil))";
        let pipeline = compile(source, "ci.fnl").expect("compile should succeed");
        let mut lines: Vec<usize> = user_jobs(&pipeline)
            .iter()
            .map(|j| {
                let offset = j.span.expect("user jobs have spans").offset();
                1 + source[..offset].matches('\n').count()
            })
            .collect();
        // All three jobs depend only on quire/push, so topo order
        // among them isn't fixed — sort before comparing.
        lines.sort();
        assert_eq!(lines, vec![2, 3, 6]);
    }

    #[test]
    fn compile_errors_on_bad_fennel() {
        let result = compile("{:bad {:}", "ci.fnl");
        assert!(result.is_err(), "malformed Fennel should fail");
    }

    /// Register a Fennel source for tests that exercise post-graph
    /// rules. Panics if registration produced any errors. The local
    /// Fennel is dropped on return, but the returned `Job`s only need
    /// their non-VM fields here.
    fn registered_jobs(source: &str) -> Vec<Job> {
        let f = Fennel::new().expect("Fennel::new() should succeed");
        registration::register(&f, source, "ci.fnl")
            .expect("register should succeed")
            .jobs
    }

    /// Run registration on a source expected to fail and return the
    /// definition errors it produced.
    fn registration_errors(source: &str) -> Vec<DefinitionError> {
        let f = Fennel::new().expect("Fennel::new() should succeed");
        let err =
            registration::register(&f, source, "ci.fnl").expect_err("expected registration errors");
        let CompileError::Pipeline(pe) = err else {
            panic!("expected PipelineError, got {err:?}")
        };
        pe.diagnostics
            .into_iter()
            .map(|d| match d {
                Diagnostic::Definition(e) => e,
                Diagnostic::Structure(_) => panic!("expected only definition errors"),
            })
            .collect()
    }

    /// Run post-graph validation against `jobs`, building the dependency
    /// graph the same way `compile` does — including the built-in push
    /// job that `compile` appends so reachability has a trigger node
    /// to walk back to.
    fn validate(mut jobs: Vec<Job>) -> std::result::Result<(), Vec<StructureError>> {
        jobs.push(builtin_push_job());
        let (graph, _) = build_graph(jobs);
        validate_post_graph(&graph)
    }

    #[test]
    fn validate_accepts_valid_config() {
        let jobs = registered_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [] nil))
(ci.job :test [:build :quire/push] (fn [] nil))
"#,
        );
        assert!(validate(jobs).is_ok());
    }

    #[test]
    fn validate_rejects_cycle() {
        let jobs = registered_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :a [:b] (fn [] nil))
(ci.job :b [:a] (fn [] nil))
"#,
        );
        let errs = validate(jobs).unwrap_err();
        assert!(
            errs.iter().any(|e| matches!(e, StructureError::Cycle { cycle_jobs, .. } if cycle_jobs.contains(&"a".to_string()) && cycle_jobs.contains(&"b".to_string()))),
            "should report a cycle involving a and b: {errs:?}"
        );
    }

    #[test]
    fn validate_cycle_only_reports_cycle_members() {
        let jobs = registered_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :a [:b :quire/push] (fn [] nil))
(ci.job :b [:a :quire/push] (fn [] nil))
(ci.job :clean [:quire/push] (fn [] nil))
"#,
        );
        let errs = validate(jobs).unwrap_err();
        let cycle_errs: Vec<&Vec<String>> = errs
            .iter()
            .filter_map(|e| match e {
                StructureError::Cycle { cycle_jobs, .. } => Some(cycle_jobs),
                _ => None, // cov-excl-line
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
        let jobs = registered_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :a [:b :quire/push] (fn [] nil))
(ci.job :b [:a :quire/push] (fn [] nil))
(ci.job :c [:d :quire/push] (fn [] nil))
(ci.job :d [:c :quire/push] (fn [] nil))
"#,
        );
        let errs = validate(jobs).unwrap_err();
        let cycle_count = errs
            .iter()
            .filter(|e| matches!(e, StructureError::Cycle { .. }))
            .count();
        assert_eq!(cycle_count, 2, "expected two cycle errors: {errs:?}");
    }

    #[test]
    fn register_rejects_empty_inputs() {
        let errors = registration_errors(
            r#"(local ci (require :quire.ci))
(ci.job :setup [] (fn [] nil))"#,
        );
        assert!(
            errors.iter().any(
                |e| matches!(e, DefinitionError::EmptyInputs { job_id, .. } if job_id == "setup")
            ),
            "should report empty inputs for 'setup': {errors:?}"
        );
    }

    #[test]
    fn register_rejects_slash_in_job_id() {
        let errors = registration_errors(
            r#"(local ci (require :quire.ci))
(ci.job :foo/bar [:quire/push] (fn [] nil))"#,
        );
        assert!(
            errors.iter().any(
                |e| matches!(e, DefinitionError::ReservedSlash { job_id, .. } if job_id == "foo/bar")
            ),
            "should report slash in job id: {errors:?}"
        );
    }

    #[test]
    fn register_rejects_duplicate_job_id() {
        let errors = registration_errors(
            r#"(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [] nil))
(ci.job :build [:quire/push] (fn [] nil))"#,
        );
        assert!(
            errors.iter().any(
                |e| matches!(e, DefinitionError::DuplicateJob { job_id, .. } if job_id == "build")
            ),
            "should report duplicate job id 'build': {errors:?}"
        );
    }

    #[test]
    fn validate_does_not_double_report_cycle_as_unreachable() {
        // Jobs in a cycle are technically also unreachable from any
        // source ref, but reporting both is noise. Cycle alone is enough.
        let jobs = registered_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :a [:b] (fn [] nil))
(ci.job :b [:a] (fn [] nil))
"#,
        );
        let errs = validate(jobs).unwrap_err();
        let unreachable_count = errs
            .iter()
            .filter(|e| matches!(e, StructureError::Unreachable { .. }))
            .count();
        assert_eq!(
            unreachable_count, 0,
            "cycle members should not also be reported as unreachable: {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_unreachable_jobs() {
        // A job whose only input names a non-existent job passes
        // pre-graph rules (inputs is non-empty, id has no slash) and
        // reaches the post-graph reachability check.
        let jobs = registered_jobs(
            r#"(local ci (require :quire.ci))
(ci.job :orphan [:does-not-exist] (fn [] nil))"#,
        );
        let errs = validate(jobs).unwrap_err();
        assert!(
            errs.iter().any(
                |e| matches!(e, StructureError::Unreachable { job_id, .. } if job_id == "orphan")
            ),
            "should report unreachable job 'orphan': {errs:?}"
        );
    }

    #[test]
    fn reachability_handles_diamond_dependencies() {
        // Diamond: push -> a -> b -> d, push -> a -> c -> d.
        // `d` is reachable and `a` is visited multiple times
        // through different paths.
        let jobs = registered_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :a [:quire/push] (fn [] nil))
(ci.job :b [:a] (fn [] nil))
(ci.job :c [:a] (fn [] nil))
(ci.job :d [:b :c] (fn [] nil))"#,
        );
        assert!(validate(jobs).is_ok());
    }

    #[test]
    fn reachability_deduplicates_visited_inputs() {
        // `orphan` lists `:a` twice. Walking from orphan:
        // stack ["a", "a"], pop a (visit), push nothing (a isn't a job),
        // stack ["a"], pop a → already visited → continue.
        // The dedup fires because `a` isn't a job and isn't a source.
        let jobs = registered_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :orphan [:a :a] (fn [] nil))"#,
        );
        let errs = validate(jobs).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, StructureError::Unreachable { .. })),
            "expected unreachable: {errs:?}"
        );
    }

    #[test]
    fn transitive_inputs_collects_direct_and_indirect() {
        let pipeline = compile(
            r#"(local ci (require :quire.ci))
(ci.job :setup [:quire/push] (fn [] nil))
(ci.job :build [:setup] (fn [] nil))
(ci.job :test [:build :setup] (fn [] nil))"#,
            "ci.fnl",
        )
        .expect("compile should succeed");

        let map = pipeline.transitive_inputs();

        assert_eq!(
            map["setup"],
            ["quire/push"].iter().map(|s| s.to_string()).collect()
        );
        assert_eq!(
            map["build"],
            ["setup", "quire/push"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        );
        assert_eq!(
            map["test"],
            ["build", "setup", "quire/push"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        );
    }

    #[test]
    fn transitive_inputs_excludes_self() {
        let pipeline = compile(
            r#"(local ci (require :quire.ci))
(ci.job :only [:quire/push] (fn [] nil))"#,
            "ci.fnl",
        )
        .expect("compile should succeed");

        let map = pipeline.transitive_inputs();
        assert!(!map["only"].contains("only"), "self should not be in set");
    }

    #[test]
    fn compile_registers_pipeline_image() {
        let source = r#"(local ci (require :quire.ci))
(ci.image "alpine")
(ci.job :build [:quire/push] (fn [] nil))"#;
        let pipeline = compile(source, "ci.fnl").expect("compile should succeed");
        assert_eq!(pipeline.image(), Some("alpine"));
    }

    #[test]
    fn compile_succeeds_without_image() {
        let source = r#"(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [] nil))"#;
        let pipeline = compile(source, "ci.fnl").expect("compile should succeed");
        assert_eq!(pipeline.image(), None);
    }

    #[test]
    fn duplicate_image_variant_exists() {
        let err = DefinitionError::DuplicateImage {
            span: SourceSpan::from((0, 0)),
        };
        assert!(err.to_string().contains("image"));
    }

    #[test]
    fn compile_short_circuits_on_definition_errors() {
        // `setup` has empty inputs (a definition error). `orphan`
        // would be unreachable (a structure error) if compile reached
        // the post-graph phase — but it shouldn't, because definition
        // errors short-circuit before structure checks run.
        let result = compile(
            r#"(local ci (require :quire.ci))
(ci.job :setup [] (fn [] nil))
(ci.job :orphan [:does-not-exist] (fn [] nil))"#,
            "ci.fnl",
        );
        let Err(CompileError::Pipeline(pe)) = result else {
            panic!("expected PipelineError")
        };
        for d in &pe.diagnostics {
            assert!(
                matches!(d, Diagnostic::Definition(_)),
                "structure errors should not be reported when registration fails: {d:?}"
            );
        }
        assert!(
            pe.diagnostics.iter().any(|d| matches!(
                d,
                Diagnostic::Definition(DefinitionError::EmptyInputs { .. })
            )),
            "expected EmptyInputs in: {:?}",
            pe.diagnostics
        );
    }

    #[test]
    fn compile_errors_on_duplicate_image() {
        let source = r#"(local ci (require :quire.ci))
(ci.image "alpine")
(ci.image "ubuntu")
(ci.job :build [:quire/push] (fn [] nil))"#;
        let result = compile(source, "ci.fnl");
        assert!(result.is_err(), "duplicate image should fail");
        let Err(e) = result else { unreachable!() };
        let msg = e.to_string();
        assert!(
            msg.contains("ci.fnl has errors"),
            "expected pipeline error: {msg}"
        );
    }
}
