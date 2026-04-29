//! CI job graph: evaluation of `ci.fnl` and validation rules.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::UserData;

use crate::Result;
use crate::fennel::Fennel;

/// A registered job extracted from ci.fnl.
pub struct Job {
    pub id: String,
    pub inputs: Vec<String>,
    /// 1-indexed line in the source where `(ci:job …)` was called.
    /// `0` means the line could not be determined.
    pub line: u32,
    /// The job's run function from the Lua VM.
    /// Stored for future execution — not yet called.
    #[expect(dead_code)]
    pub(crate) run_fn: mlua::Function,
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
/// Registered as `package.loaded["quire.ci"]` so scripts can write:
///
/// ```fennel
/// (local ci (require :quire.ci))
/// (ci:job :build [:quire/push] (fn [_] nil))
/// ```
struct CiModule {
    jobs: Rc<RefCell<Vec<Job>>>,
}

impl UserData for CiModule {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "job",
            |lua, this, (id, inputs, run_fn): (String, Vec<String>, mlua::Function)| {
                let line = lua
                    .inspect_stack(1, |d| d.current_line())
                    .flatten()
                    .map(|l| l as u32)
                    .unwrap_or(0);
                this.jobs.borrow_mut().push(Job {
                    id,
                    inputs,
                    line,
                    run_fn,
                });
                Ok(())
            },
        );
    }
}

/// Parse and validate a ci.fnl source string into a `Pipeline`.
///
/// Injects `quire.ci` into `package.loaded` so scripts can
/// `(require :quire.ci)`, evaluates the source to register jobs, and
/// then runs the structural validations.
pub(crate) fn load(
    fennel: &Fennel,
    source: &str,
    filename: &str,
    display: &str,
) -> Result<Pipeline> {
    let jobs = parse(fennel, source, filename, display)?;
    validate(&jobs)?;
    Ok(Pipeline { jobs })
}

/// Evaluate `source` with the `quire.ci` module bound and collect the
/// registered jobs without validating them.
fn parse(fennel: &Fennel, source: &str, filename: &str, display: &str) -> Result<Vec<Job>> {
    let jobs = Rc::new(RefCell::new(Vec::new()));

    fennel.eval_raw(source, filename, display, |lua| {
        let loaded: mlua::Table = lua.globals().get::<mlua::Table>("package")?.get("loaded")?;
        loaded.set("quire.ci", CiModule { jobs: jobs.clone() })?;
        Ok(())
    })?;

    Ok(jobs.take())
}

/// A validation error found in the job graph.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ValidationError {
    #[error("Cycle detected among jobs: {}", cycle_jobs.join(", "))]
    Cycle { cycle_jobs: Vec<String> },

    #[error(
        "Job '{job_id}' has empty inputs. Pass [:quire/push] (or another input) so it has something to fire it."
    )]
    EmptyInputs { job_id: String },

    #[error("Job '{job_id}' is not reachable from any source ref (e.g. :quire/push).")]
    Unreachable { job_id: String },

    #[error("Job id '{job_id}' contains '/', which is reserved for the 'quire/' source namespace.")]
    ReservedSlash { job_id: String },
}

/// Validate the structural rules of a job graph.
///
/// Returns `Ok(())` if all four rules pass, or `Err` with all violations found.
/// Used by `pipeline::load`.
fn validate(jobs: &[Job]) -> std::result::Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    // Rule 4: no '/' in user job ids.
    for job in jobs {
        if job.id.contains('/') {
            errors.push(ValidationError::ReservedSlash {
                job_id: job.id.clone(),
            });
        }
    }

    // Rule 2: non-empty inputs.
    for job in jobs {
        if job.inputs.is_empty() {
            errors.push(ValidationError::EmptyInputs {
                job_id: job.id.clone(),
            });
        }
    }

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
        let mut cycle_jobs: Vec<String> = scc.iter().map(|&idx| graph[idx].to_string()).collect();
        cycle_jobs.sort();
        errors.push(ValidationError::Cycle { cycle_jobs });
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
(ci:job :test [:quire/push] (fn [_] nil))"#;
        let pipeline = load(&f, source, "ci.fnl", "ci.fnl").expect("load should succeed");
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
(ci:job :build [:quire/push] (fn [_] nil))
(ci:job :test [:build] (fn [_] nil))
"#;
        let pipeline = load(&f, source, "ci.fnl", "ci.fnl").expect("load should succeed");
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
(ci:job :first [:quire/push] (fn [_] nil))
(ci:job :second [:quire/push] (fn [_] nil))


(ci:job :sixth [:quire/push] (fn [_] nil))";
        let pipeline = load(&f, source, "ci.fnl", "ci.fnl").expect("load should succeed");
        let lines: Vec<u32> = pipeline.jobs().iter().map(|j| j.line).collect();
        assert_eq!(lines, vec![2, 3, 6]);
    }

    #[test]
    fn load_errors_on_bad_fennel() {
        let f = fennel();
        let result = load(&f, "{:bad {:}", "ci.fnl", "ci.fnl");
        assert!(result.is_err(), "malformed Fennel should fail");
    }

    /// Parse a Fennel source into raw jobs without validating, for tests
    /// that exercise individual validation rules.
    fn parse_jobs(source: &str) -> Vec<Job> {
        let f = fennel();
        parse(&f, source, "ci.fnl", "ci.fnl").expect("parse should succeed")
    }

    #[test]
    fn validate_accepts_valid_config() {
        let jobs = parse_jobs(
            r#"
(local ci (require :quire.ci))
(ci:job :build [:quire/push] (fn [_] nil))
(ci:job :test [:build :quire/push] (fn [_] nil))
"#,
        );
        assert!(validate(&jobs).is_ok());
    }

    #[test]
    fn validate_rejects_cycle() {
        let jobs = parse_jobs(
            r#"
(local ci (require :quire.ci))
(ci:job :a [:b] (fn [_] nil))
(ci:job :b [:a] (fn [_] nil))
"#,
        );
        let errs = validate(&jobs).unwrap_err();
        assert!(
            errs.iter().any(|e| matches!(e, ValidationError::Cycle { cycle_jobs } if cycle_jobs.contains(&"a".to_string()) && cycle_jobs.contains(&"b".to_string()))),
            "should report a cycle involving a and b: {errs:?}"
        );
    }

    #[test]
    fn validate_cycle_only_reports_cycle_members() {
        let jobs = parse_jobs(
            r#"
(local ci (require :quire.ci))
(ci:job :a [:b :quire/push] (fn [_] nil))
(ci:job :b [:a :quire/push] (fn [_] nil))
(ci:job :clean [:quire/push] (fn [_] nil))
"#,
        );
        let errs = validate(&jobs).unwrap_err();
        let cycle_errs: Vec<&Vec<String>> = errs
            .iter()
            .filter_map(|e| match e {
                ValidationError::Cycle { cycle_jobs } => Some(cycle_jobs),
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
        let jobs = parse_jobs(
            r#"
(local ci (require :quire.ci))
(ci:job :a [:b :quire/push] (fn [_] nil))
(ci:job :b [:a :quire/push] (fn [_] nil))
(ci:job :c [:d :quire/push] (fn [_] nil))
(ci:job :d [:c :quire/push] (fn [_] nil))
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
    fn validate_rejects_empty_inputs() {
        let jobs = parse_jobs(
            r#"(local ci (require :quire.ci))
(ci:job :setup [] (fn [_] nil))"#,
        );
        let errs = validate(&jobs).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::EmptyInputs { job_id } if job_id == "setup")),
            "should report empty inputs for 'setup': {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_unreachable_jobs() {
        let jobs = parse_jobs(
            r#"(local ci (require :quire.ci))
(ci:job :orphan [:orphan] (fn [_] nil))"#,
        );
        let errs = validate(&jobs).unwrap_err();
        assert!(
            errs.iter().any(
                |e| matches!(e, ValidationError::Unreachable { job_id } if job_id == "orphan")
            ),
            "should report unreachable job 'orphan': {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_slash_in_job_id() {
        let jobs = parse_jobs(
            r#"(local ci (require :quire.ci))
(ci:job :foo/bar [:quire/push] (fn [_] nil))"#,
        );
        let errs = validate(&jobs).unwrap_err();
        assert!(
            errs.iter().any(
                |e| matches!(e, ValidationError::ReservedSlash { job_id } if job_id == "foo/bar")
            ),
            "should report slash in job id: {errs:?}"
        );
    }
}
