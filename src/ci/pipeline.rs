//! CI job graph: validation rules and the `load` entry point that
//! parses a `ci.fnl` source string into a `Pipeline`.
//!
//! Lua/Fennel evaluation lives in the sibling [`super::lua`] module;
//! this module owns the domain types and the structural rules.

use std::collections::{HashMap, HashSet};

use miette::{NamedSource, SourceSpan};
use petgraph::Graph;
use petgraph::graph::NodeIndex;
use petgraph::visit::{Bfs, Reversed};

use super::lua;
use crate::Result;
use crate::fennel::Fennel;

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
    ///
    /// Visible to the sibling `lua` module which constructs jobs from
    /// the registration callback.
    pub(super) fn new(
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
/// Obtain via [`Pipeline::load`], which parses the Fennel source and
/// validates the result. Holding a `Pipeline` is proof that the graph
/// is sound.
///
/// Owns the Fennel/Lua VM so the registered `run_fn`s remain callable
/// after `load` returns.
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

    /// Parse and validate a ci.fnl source string into a `Pipeline`.
    ///
    /// Delegates evaluation to [`lua::parse`] for the Fennel-side work,
    /// then runs the post-graph rules over the surviving jobs. Any errors
    /// found are gathered into a single `LoadError` carrying the source
    /// for miette to render with inline labels.
    pub(crate) fn load(source: &str, name: &str) -> Result<Pipeline> {
        let fennel = Fennel::new()?;
        let output = lua::parse(&fennel, source, name)?;

        let mut errors = Vec::new();
        let mut jobs = Vec::new();
        for r in output.jobs {
            match r {
                Ok(j) => jobs.push(j),
                Err(e) => errors.push(e),
            }
        }
        let image = output.image;

        let (graph, node_index) = build_graph(&jobs);

        if let Err(post) = validate_post_graph(&jobs, &graph) {
            errors.extend(post);
        }

        if errors.is_empty() {
            Ok(Self {
                jobs,
                graph,
                node_index,
                fennel,
                image,
            })
        } else {
            Err(LoadError {
                src: NamedSource::new(name, source.to_string()),
                errors,
            }
            .into())
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

    #[error("Pipeline image declared more than once.")]
    DuplicateImage {
        #[label("duplicate image declaration")]
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
/// `(ci.job …)` callback during `lua::parse`, so they are not re-checked
/// here.
fn validate_post_graph(
    jobs: &[Job],
    graph: &JobGraph,
) -> std::result::Result<(), Vec<ValidationError>> {
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
        errors.push(ValidationError::Cycle { cycle_jobs, spans });
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

    #[test]
    fn load_registers_a_job() {
        let source = r#"(local ci (require :quire.ci))
(ci.job :test [:quire/push] (fn [_] nil))"#;
        let pipeline = Pipeline::load(source, "ci.fnl").expect("load should succeed");
        let jobs = pipeline.jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "test");
        assert_eq!(jobs[0].inputs, vec!["quire/push"]);
    }

    #[test]
    fn load_registers_multiple_jobs() {
        let source = r#"
(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [_] nil))
(ci.job :test [:build] (fn [_] nil))
"#;
        let pipeline = Pipeline::load(source, "ci.fnl").expect("load should succeed");
        let jobs = pipeline.jobs();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].id, "build");
        assert_eq!(jobs[0].inputs, vec!["quire/push"]);
        assert_eq!(jobs[1].id, "test");
        assert_eq!(jobs[1].inputs, vec!["build"]);
    }

    #[test]
    fn load_captures_source_line() {
        let source = "(local ci (require :quire.ci))
(ci.job :first [:quire/push] (fn [_] nil))
(ci.job :second [:quire/push] (fn [_] nil))


(ci.job :sixth [:quire/push] (fn [_] nil))";
        let pipeline = Pipeline::load(source, "ci.fnl").expect("load should succeed");
        let lines: Vec<usize> = pipeline
            .jobs()
            .iter()
            .map(|j| 1 + source[..j.span.offset()].matches('\n').count())
            .collect();
        assert_eq!(lines, vec![2, 3, 6]);
    }

    #[test]
    fn load_errors_on_bad_fennel() {
        let result = Pipeline::load("{:bad {:}", "ci.fnl");
        assert!(result.is_err(), "malformed Fennel should fail");
    }

    /// Parse a Fennel source into per-job results. Pre-graph rules
    /// run during parsing, so each entry is `Ok(Job)` or
    /// `Err(ValidationError)`. The local Fennel is dropped on return,
    /// but the returned `Job`s only need their non-VM fields here.
    fn parse_results(source: &str) -> Vec<std::result::Result<Job, ValidationError>> {
        let f = Fennel::new().expect("Fennel::new() should succeed");
        lua::parse(&f, source, "ci.fnl")
            .expect("parse should succeed")
            .jobs
    }

    /// Discard parse errors and return only the successfully registered
    /// jobs — for tests that exercise post-graph rules.
    fn parsed_jobs(source: &str) -> Vec<Job> {
        parse_results(source)
            .into_iter()
            .filter_map(|r| r.ok())
            .collect()
    }

    /// Run post-graph validation against `jobs`, building the dependency
    /// graph the same way `load` does.
    fn validate(jobs: &[Job]) -> std::result::Result<(), Vec<ValidationError>> {
        let (graph, _) = build_graph(jobs);
        validate_post_graph(jobs, &graph)
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
        assert!(validate(&jobs).is_ok());
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
        let errs = validate(&jobs).unwrap_err();
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
        let errs = validate(&jobs).unwrap_err();
        let cycle_errs: Vec<&Vec<String>> = errs
            .iter()
            .filter_map(|e| match e {
                ValidationError::Cycle { cycle_jobs, .. } => Some(cycle_jobs),
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
        let jobs = parsed_jobs(
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
    fn validate_does_not_double_report_cycle_as_unreachable() {
        // Jobs in a cycle are technically also unreachable from any
        // source ref, but reporting both is noise. Cycle alone is enough.
        let jobs = parsed_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :a [:b] (fn [_] nil))
(ci.job :b [:a] (fn [_] nil))
"#,
        );
        let errs = validate(&jobs).unwrap_err();
        let unreachable_count = errs
            .iter()
            .filter(|e| matches!(e, ValidationError::Unreachable { .. }))
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
        let jobs = parsed_jobs(
            r#"(local ci (require :quire.ci))
(ci.job :orphan [:does-not-exist] (fn [_] nil))"#,
        );
        let errs = validate(&jobs).unwrap_err();
        assert!(
            errs.iter().any(
                |e| matches!(e, ValidationError::Unreachable { job_id, .. } if job_id == "orphan")
            ),
            "should report unreachable job 'orphan': {errs:?}"
        );
    }

    #[test]
    fn reachability_handles_diamond_dependencies() {
        // Diamond: push -> a -> b -> d, push -> a -> c -> d.
        // `d` is reachable and `a` is visited multiple times
        // through different paths.
        let jobs = parsed_jobs(
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
        let jobs = parsed_jobs(
            r#"
(local ci (require :quire.ci))
(ci.job :orphan [:a :a] (fn [_] nil))"#,
        );
        let errs = validate(&jobs).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::Unreachable { .. })),
            "expected unreachable: {errs:?}"
        );
    }

    #[test]
    fn transitive_inputs_collects_direct_and_indirect() {
        let pipeline = Pipeline::load(
            r#"(local ci (require :quire.ci))
(ci.job :setup [:quire/push] (fn [_] nil))
(ci.job :build [:setup] (fn [_] nil))
(ci.job :test [:build :setup] (fn [_] nil))"#,
            "ci.fnl",
        )
        .expect("load should succeed");

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
        let pipeline = Pipeline::load(
            r#"(local ci (require :quire.ci))
(ci.job :only [:quire/push] (fn [_] nil))"#,
            "ci.fnl",
        )
        .expect("load should succeed");

        let map = pipeline.transitive_inputs();
        assert!(!map["only"].contains("only"), "self should not be in set");
    }

    #[test]
    fn load_registers_pipeline_image() {
        let source = r#"(local ci (require :quire.ci))
(ci.image "alpine")
(ci.job :build [:quire/push] (fn [_] nil))"#;
        let pipeline = Pipeline::load(source, "ci.fnl").expect("load should succeed");
        assert_eq!(pipeline.image(), Some("alpine"));
    }

    #[test]
    fn load_succeeds_without_image() {
        let source = r#"(local ci (require :quire.ci))
(ci.job :build [:quire/push] (fn [_] nil))"#;
        let pipeline = Pipeline::load(source, "ci.fnl").expect("load should succeed");
        assert_eq!(pipeline.image(), None);
    }

    #[test]
    fn duplicate_image_variant_exists() {
        let err = ValidationError::DuplicateImage {
            span: SourceSpan::from((0, 0)),
        };
        assert!(err.to_string().contains("image"));
    }

    #[test]
    fn load_reports_both_parse_and_post_graph_errors() {
        // `setup` has empty inputs (pre-graph error) and `orphan` is unreachable
        // (post-graph error). Both should be reported.
        let result = Pipeline::load(
            r#"(local ci (require :quire.ci))
(ci.job :setup [] (fn [_] nil))
(ci.job :orphan [:does-not-exist] (fn [_] nil))"#,
            "ci.fnl",
        );
        let Err(e) = result else { unreachable!() };
        let msg = e.to_string();
        assert!(
            msg.contains("CI validation failed"),
            "expected validation error: {msg}"
        );
    }

    #[test]
    fn load_errors_on_duplicate_image() {
        let source = r#"(local ci (require :quire.ci))
(ci.image "alpine")
(ci.image "ubuntu")
(ci.job :build [:quire/push] (fn [_] nil))"#;
        let result = Pipeline::load(source, "ci.fnl");
        assert!(result.is_err(), "duplicate image should fail");
        let Err(e) = result else { unreachable!() };
        let msg = e.to_string();
        assert!(
            msg.contains("CI validation failed"),
            "expected validation error: {msg}"
        );
    }
}
