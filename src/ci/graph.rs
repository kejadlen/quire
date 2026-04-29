//! CI job graph: evaluation of `ci.fnl` and validation rules.

use crate::Result;
use crate::fennel::{Fennel, FennelError};

/// A registered job definition extracted from ci.fnl.
pub struct JobDef {
    pub id: String,
    pub inputs: Vec<String>,
}

/// The result of evaluating a ci.fnl file.
pub struct EvalResult {
    pub jobs: Vec<JobDef>,
}

/// Evaluate a ci.fnl source string, registering jobs via the `job` macro.
///
/// Injects a `job` global that accumulates into a registration table,
/// evaluates the source, and extracts the registered jobs.
pub fn eval_ci(fennel: &Fennel, source: &str, name: &str) -> Result<EvalResult> {
    fennel.eval_raw(source, name, |lua| {
        // Create a registration table. `job` will push into this.
        let registry: mlua::Table = lua.create_table()?;
        lua.globals().set("_quire_jobs", registry)?;

        // Define the `job` global: (job id inputs run-fn)
        let job_fn = lua.create_function(
            |lua, (id, inputs, run_fn): (mlua::String, mlua::Table, mlua::Function)| {
                let registry: mlua::Table = lua.globals().get("_quire_jobs")?;
                let entry = lua.create_table()?;
                entry.set("id", id)?;
                entry.set("inputs", inputs)?;
                entry.set("run", run_fn)?;
                registry.push(entry)?;
                Ok(())
            },
        )?;
        lua.globals().set("job", job_fn)?;

        Ok(())
    })?;

    // Extract the registration table.
    let lua_err = |e: mlua::Error| FennelError::from_lua(source, name, e);
    let registry: mlua::Table = fennel.lua().globals().get("_quire_jobs").map_err(lua_err)?;
    let mut jobs = Vec::new();
    for entry in registry.sequence_values::<mlua::Table>() {
        let entry = entry.map_err(lua_err)?;
        let id: String = entry.get("id").map_err(lua_err)?;
        let inputs_table: mlua::Table = entry.get("inputs").map_err(lua_err)?;
        let mut inputs = Vec::new();
        for input in inputs_table.sequence_values::<String>() {
            inputs.push(input.map_err(lua_err)?);
        }
        jobs.push(JobDef { id, inputs });
    }

    Ok(EvalResult { jobs })
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
pub fn validate(jobs: &[JobDef]) -> std::result::Result<(), Vec<ValidationError>> {
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
    fn eval_ci_registers_a_job() {
        let f = fennel();
        let source = r#"(job :test [:quire/push] (fn [_] nil))"#;
        let result = eval_ci(&f, source, "ci.fnl").expect("eval should succeed");
        assert_eq!(result.jobs.len(), 1);
        assert_eq!(result.jobs[0].id, "test");
        assert_eq!(result.jobs[0].inputs, vec!["quire/push"]);
    }

    #[test]
    fn eval_ci_registers_multiple_jobs() {
        let f = fennel();
        let source = r#"
(job :build [:quire/push] (fn [_] nil))
(job :test [:build] (fn [_] nil))
"#;
        let result = eval_ci(&f, source, "ci.fnl").expect("eval should succeed");
        assert_eq!(result.jobs.len(), 2);
        assert_eq!(result.jobs[0].id, "build");
        assert_eq!(result.jobs[0].inputs, vec!["quire/push"]);
        assert_eq!(result.jobs[1].id, "test");
        assert_eq!(result.jobs[1].inputs, vec!["build"]);
    }

    #[test]
    fn eval_ci_errors_on_bad_fennel() {
        let f = fennel();
        let result = eval_ci(&f, "{:bad {:}", "ci.fnl");
        assert!(result.is_err(), "malformed Fennel should fail");
    }

    #[test]
    fn validate_accepts_valid_config() {
        let jobs = vec![
            JobDef {
                id: "build".into(),
                inputs: vec!["quire/push".into()],
            },
            JobDef {
                id: "test".into(),
                inputs: vec!["build".into(), "quire/push".into()],
            },
        ];
        assert!(validate(&jobs).is_ok());
    }

    #[test]
    fn validate_rejects_cycle() {
        let jobs = vec![
            JobDef {
                id: "a".into(),
                inputs: vec!["b".into()],
            },
            JobDef {
                id: "b".into(),
                inputs: vec!["a".into()],
            },
        ];
        let errs = validate(&jobs).unwrap_err();
        assert!(
            errs.iter().any(|e| matches!(e, ValidationError::Cycle { cycle_jobs } if cycle_jobs.contains(&"a".to_string()) && cycle_jobs.contains(&"b".to_string()))),
            "should report a cycle involving a and b: {errs:?}"
        );
    }

    #[test]
    fn validate_cycle_only_reports_cycle_members() {
        // `clean` is acyclic; `a` and `b` form a cycle. Only a/b should be
        // flagged, and `clean` must not appear in any Cycle error.
        let jobs = vec![
            JobDef {
                id: "a".into(),
                inputs: vec!["b".into(), "quire/push".into()],
            },
            JobDef {
                id: "b".into(),
                inputs: vec!["a".into(), "quire/push".into()],
            },
            JobDef {
                id: "clean".into(),
                inputs: vec!["quire/push".into()],
            },
        ];
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
        // Two independent cycles: (a <-> b) and (c <-> d).
        let jobs = vec![
            JobDef {
                id: "a".into(),
                inputs: vec!["b".into(), "quire/push".into()],
            },
            JobDef {
                id: "b".into(),
                inputs: vec!["a".into(), "quire/push".into()],
            },
            JobDef {
                id: "c".into(),
                inputs: vec!["d".into(), "quire/push".into()],
            },
            JobDef {
                id: "d".into(),
                inputs: vec!["c".into(), "quire/push".into()],
            },
        ];
        let errs = validate(&jobs).unwrap_err();
        let cycle_count = errs
            .iter()
            .filter(|e| matches!(e, ValidationError::Cycle { .. }))
            .count();
        assert_eq!(cycle_count, 2, "expected two cycle errors: {errs:?}");
    }

    #[test]
    fn validate_rejects_empty_inputs() {
        let jobs = vec![JobDef {
            id: "setup".into(),
            inputs: vec![],
        }];
        let errs = validate(&jobs).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::EmptyInputs { job_id } if job_id == "setup")),
            "should report empty inputs for 'setup': {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_unreachable_jobs() {
        let jobs = vec![JobDef {
            id: "orphan".into(),
            inputs: vec!["orphan".into()],
        }];
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
        let jobs = vec![JobDef {
            id: "foo/bar".into(),
            inputs: vec!["quire/push".into()],
        }];
        let errs = validate(&jobs).unwrap_err();
        assert!(
            errs.iter().any(
                |e| matches!(e, ValidationError::ReservedSlash { job_id } if job_id == "foo/bar")
            ),
            "should report slash in job id: {errs:?}"
        );
    }
}
