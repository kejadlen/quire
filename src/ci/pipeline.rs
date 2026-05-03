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
use crate::Result;
use crate::fennel::Fennel;

/// A registration-time error caught while individual `(ci.job …)` and
/// `(ci.image …)` calls are being processed.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum DefinitionError {
    #[error(
        "Job '{job_id}' has empty inputs. Pass [:quire/push] (or another input) so it has something to fire it."
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

    #[error("Pipeline mirror declared more than once.")]
    DuplicateMirror {
        #[label("duplicate mirror declaration")]
        span: SourceSpan,
    },

    #[error("ci.mirror: {message}")]
    InvalidMirrorCall {
        message: String,
        #[label("here")]
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

    #[error("Job '{job_id}' is not reachable from any source ref (e.g. :quire/push).")]
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

/// Edges point from dependency to dependent. Node weights are indices
/// into `Pipeline::jobs`; source refs (e.g. `quire/push`) are not nodes.
type JobGraph = Graph<usize, ()>;

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
    /// What to run when the executor reaches this job.
    pub(super) run_fn: RunFn,
}

/// A Rust-side run-fn: a closure invoked synchronously by the
/// executor with the runtime in scope.
pub(super) type RustRunFn = std::rc::Rc<dyn Fn(&super::runtime::Runtime) -> Result<()>>;

/// How a job runs at execute time.
///
/// `Lua` is the user case: a Fennel function the executor calls
/// through the Lua VM, passing the runtime handle table. `Rust` is
/// the built-in case: a closure that receives the runtime directly,
/// used by helpers (e.g. `(ci.mirror …)`) that do their work in
/// plain Rust without round-tripping through Lua.
///
/// Both variants are `Clone` so the executor can take an owned copy
/// before invoking — `mlua::Function` is cheap to clone (a registry
/// handle); the `Rc` makes the `Rust` variant cheap too.
#[derive(Clone)]
pub(super) enum RunFn {
    Lua(mlua::Function),
    #[allow(dead_code)] // Wired up by `(ci.mirror …)` and friends.
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
    pub(super) fn new(
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
            span,
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
    jobs: Vec<Job>,
    graph: JobGraph,
    /// Job id → node index in `graph`, for O(1) lookup.
    node_index: HashMap<String, NodeIndex>,
    fennel: Fennel,
    /// Container image declared via `(ci.image "...")`, if any.
    image: Option<String>,
}

impl Pipeline {
    pub fn jobs(&self) -> &[Job] {
        &self.jobs
    }

    /// Look up a job by id.
    pub fn job(&self, id: &str) -> Option<&Job> {
        self.node_index
            .get(id)
            .map(|&idx| &self.jobs[self.graph[idx]])
    }

    /// Borrow the underlying Fennel/Lua VM.
    pub(crate) fn fennel(&self) -> &Fennel {
        &self.fennel
    }

    /// The container image declared via `(ci.image ...)`, if any.
    /// The executor resolves the final image at run time:
    /// declared image → `.quire/Dockerfile` → default.
    pub fn image(&self) -> Option<&str> {
        self.image.as_deref()
    }

    /// Return job IDs in topological order — dependencies before
    /// dependents. The pipeline is already validated as acyclic, so
    /// this never fails.
    pub(crate) fn topo_order(&self) -> Vec<&str> {
        petgraph::algo::toposort(&self.graph, None)
            .expect("pipeline is validated as acyclic")
            .into_iter()
            .map(|idx| self.jobs[self.graph[idx]].id.as_str())
            .collect()
    }

    /// For each job, the set of input names — direct and transitive,
    /// including source refs — reachable through the input graph. The
    /// job's own id is not included.
    ///
    /// Used by the executor to validate `(jobs name)` lookups: the
    /// calling job may only read outputs from names in its set.
    ///
    /// Walks the existing dependency graph in reverse (ancestors of
    /// the job) via petgraph's BFS. Source refs aren't graph nodes,
    /// so they're scooped up from the inputs lists of every visited
    /// job.
    pub(crate) fn transitive_inputs(&self) -> HashMap<String, HashSet<String>> {
        let reversed = Reversed(&self.graph);
        let mut result: HashMap<String, HashSet<String>> = HashMap::new();
        for job in &self.jobs {
            let start = self.node_index[&job.id];
            let mut reachable = HashSet::new();
            let mut bfs = Bfs::new(reversed, start);
            while let Some(idx) = bfs.next(reversed) {
                let visited = &self.jobs[self.graph[idx]];
                if idx != start {
                    reachable.insert(visited.id.clone());
                }
                for input in &visited.inputs {
                    if !self.node_index.contains_key(input) {
                        reachable.insert(input.clone());
                    }
                }
            }
            result.insert(job.id.clone(), reachable);
        }
        result
    }
}

#[cfg(test)]
impl Pipeline {
    /// Replace the first job's run-fn — for tests that need to
    /// exercise a `RunFn::Rust` execution path without building the
    /// full helper machinery (which doesn't exist yet).
    pub(super) fn replace_first_run_fn(&mut self, run_fn: RunFn) {
        if let Some(job) = self.jobs.first_mut() {
            job.run_fn = run_fn;
        }
    }
}

/// Build the dependency graph for `jobs`. Inputs that don't match a
/// known job id are treated as source refs (e.g. `quire/push`) and
/// don't get an edge — they're not nodes in this graph.
fn build_graph(jobs: &[Job]) -> (JobGraph, HashMap<String, NodeIndex>) {
    let mut graph = JobGraph::new();
    let mut node_index = HashMap::new();
    for (i, job) in jobs.iter().enumerate() {
        let idx = graph.add_node(i);
        node_index.insert(job.id.clone(), idx);
    }
    for job in jobs {
        let dependent = node_index[&job.id];
        for input in &job.inputs {
            if let Some(&dependency) = node_index.get(input) {
                graph.add_edge(dependency, dependent, ());
            }
        }
    }
    (graph, node_index)
}

/// Compute a span covering the given 1-indexed line in `source`.
/// Returns an empty span at offset 0 when the line is unknown.
pub(super) fn span_for_line(source: &str, line: u32) -> SourceSpan {
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

/// Compile a ci.fnl source string into a validated [`Pipeline`].
///
/// Two phases, fail-fast between them: [`registration::register`]
/// evaluates the script and reports any definition-time errors, then
/// [`validate_post_graph`] checks the dependency graph. Errors from a
/// phase are wrapped in a [`PipelineError`] for miette to render with
/// inline labels.
pub(crate) fn compile(source: &str, name: &str) -> Result<Pipeline> {
    let fennel = Fennel::new()?;
    let Registrations { jobs, image } = registration::register(&fennel, source, name)?;

    let (graph, node_index) = build_graph(&jobs);

    if let Err(errors) = validate_post_graph(&jobs, &graph) {
        return Err(PipelineError {
            src: NamedSource::new(name, source.to_string()),
            diagnostics: errors.into_iter().map(Diagnostic::Structure).collect(),
        }
        .into());
    }

    Ok(Pipeline {
        jobs,
        graph,
        node_index,
        fennel,
        image,
    })
}

/// Run the post-graph validation rules — cycle detection and source
/// reachability — over the surviving jobs from registration.
///
/// Per-job pre-graph rules (slash-in-id, empty inputs) run inside the
/// `(ci.job …)` callback during `registration::register`, so they are
/// not re-checked here.
fn validate_post_graph(
    jobs: &[Job],
    graph: &JobGraph,
) -> std::result::Result<(), Vec<StructureError>> {
    let mut errors = Vec::new();
    let mut cycle_members: std::collections::HashSet<&str> = std::collections::HashSet::new();

    // Rule 1: acyclic. Each non-trivial strongly connected component
    // is a distinct cycle. A single-node SCC is only a cycle if it has
    // a self-edge.
    for scc in petgraph::algo::tarjan_scc(graph) {
        let is_cycle = scc.len() > 1 || (scc.len() == 1 && graph.contains_edge(scc[0], scc[0]));
        if !is_cycle {
            continue;
        }
        let mut members: Vec<&Job> = scc.iter().map(|&idx| &jobs[graph[idx]]).collect();
        members.sort_by(|a, b| a.id.cmp(&b.id));
        for j in &members {
            cycle_members.insert(j.id.as_str());
        }
        let cycle_jobs = members.iter().map(|j| j.id.clone()).collect();
        let spans = members.iter().map(|j| j.span).collect();
        errors.push(StructureError::Cycle { cycle_jobs, spans });
    }

    // Rule 3: reachability — every job's transitive inputs must include a source ref.
    //
    // TODO: replace the `quire/` prefix check with a whitelist of real
    // source refs (`quire/push`, etc.) once those are implemented, so
    // typos like `:quire/posh` don't silently make a job "reachable."
    let is_source = |name: &str| name.starts_with("quire/");

    for job in jobs {
        // Cycle members are already reported via Cycle; skip them here so
        // a single bad cycle doesn't generate N+1 errors.
        if cycle_members.contains(job.id.as_str()) {
            continue;
        }
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
            errors.push(StructureError::Unreachable {
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

    #[test]
    fn compile_registers_a_job() {
        let source = r#"(local ci (require :quire.ci))
(ci.job :test [:quire/push] (fn [_] nil))"#;
        let pipeline = compile(source, "ci.fnl").expect("compile should succeed");
        let jobs = pipeline.jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "test");
        assert_eq!(jobs[0].inputs, vec!["quire/push"]);
    }

    #[test]
    fn compile_registers_multiple_jobs() {
        let source = r#"
(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [_] nil))
(ci.job :test [:build] (fn [_] nil))
"#;
        let pipeline = compile(source, "ci.fnl").expect("compile should succeed");
        let jobs = pipeline.jobs();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].id, "build");
        assert_eq!(jobs[0].inputs, vec!["quire/push"]);
        assert_eq!(jobs[1].id, "test");
        assert_eq!(jobs[1].inputs, vec!["build"]);
    }

    #[test]
    fn compile_captures_source_line() {
        let source = "(local ci (require :quire.ci))
(ci.job :first [:quire/push] (fn [_] nil))
(ci.job :second [:quire/push] (fn [_] nil))


(ci.job :sixth [:quire/push] (fn [_] nil))";
        let pipeline = compile(source, "ci.fnl").expect("compile should succeed");
        let lines: Vec<usize> = pipeline
            .jobs()
            .iter()
            .map(|j| 1 + source[..j.span.offset()].matches('\n').count())
            .collect();
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
        let err = registration::register(&f, source, "ci.fnl").expect_err("expected registration errors");
        let crate::Error::Pipeline(pe) = err else {
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
    /// graph the same way `compile` does.
    fn validate(jobs: &[Job]) -> std::result::Result<(), Vec<StructureError>> {
        let (graph, _) = build_graph(jobs);
        validate_post_graph(jobs, &graph)
    }

    #[test]
    fn validate_accepts_valid_config() {
        let jobs = registered_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [_] nil))
(ci.job :test [:build :quire/push] (fn [_] nil))
"#,
        );
        assert!(validate(&jobs).is_ok());
    }

    #[test]
    fn validate_rejects_cycle() {
        let jobs = registered_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :a [:b] (fn [_] nil))
(ci.job :b [:a] (fn [_] nil))
"#,
        );
        let errs = validate(&jobs).unwrap_err();
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
(ci.job :a [:b :quire/push] (fn [_] nil))
(ci.job :b [:a :quire/push] (fn [_] nil))
(ci.job :clean [:quire/push] (fn [_] nil))
"#,
        );
        let errs = validate(&jobs).unwrap_err();
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
(ci.job :a [:b :quire/push] (fn [_] nil))
(ci.job :b [:a :quire/push] (fn [_] nil))
(ci.job :c [:d :quire/push] (fn [_] nil))
(ci.job :d [:c :quire/push] (fn [_] nil))
"#,
        );
        let errs = validate(&jobs).unwrap_err();
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
(ci.job :setup [] (fn [_] nil))"#,
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
(ci.job :foo/bar [:quire/push] (fn [_] nil))"#,
        );
        assert!(
            errors.iter().any(
                |e| matches!(e, DefinitionError::ReservedSlash { job_id, .. } if job_id == "foo/bar")
            ),
            "should report slash in job id: {errors:?}"
        );
    }

    #[test]
    fn validate_does_not_double_report_cycle_as_unreachable() {
        // Jobs in a cycle are technically also unreachable from any
        // source ref, but reporting both is noise. Cycle alone is enough.
        let jobs = registered_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :a [:b] (fn [_] nil))
(ci.job :b [:a] (fn [_] nil))
"#,
        );
        let errs = validate(&jobs).unwrap_err();
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
(ci.job :orphan [:does-not-exist] (fn [_] nil))"#,
        );
        let errs = validate(&jobs).unwrap_err();
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
(ci.job :a [:quire/push] (fn [_] nil))
(ci.job :b [:a] (fn [_] nil))
(ci.job :c [:a] (fn [_] nil))
(ci.job :d [:b :c] (fn [_] nil))"#,
        );
        assert!(validate(&jobs).is_ok());
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
(ci.job :orphan [:a :a] (fn [_] nil))"#,
        );
        let errs = validate(&jobs).unwrap_err();
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
(ci.job :setup [:quire/push] (fn [_] nil))
(ci.job :build [:setup] (fn [_] nil))
(ci.job :test [:build :setup] (fn [_] nil))"#,
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
(ci.job :only [:quire/push] (fn [_] nil))"#,
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
(ci.job :build [:quire/push] (fn [_] nil))"#;
        let pipeline = compile(source, "ci.fnl").expect("compile should succeed");
        assert_eq!(pipeline.image(), Some("alpine"));
    }

    #[test]
    fn compile_succeeds_without_image() {
        let source = r#"(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [_] nil))"#;
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
(ci.job :setup [] (fn [_] nil))
(ci.job :orphan [:does-not-exist] (fn [_] nil))"#,
            "ci.fnl",
        );
        let Err(crate::Error::Pipeline(pe)) = result else {
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
(ci.job :build [:quire/push] (fn [_] nil))"#;
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
