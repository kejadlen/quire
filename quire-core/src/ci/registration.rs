//! Registration-time DSL: evaluating a `ci.fnl` script with the
//! `(require :quire.ci)` module bound and collecting the jobs and
//! image it registers.
//!
//! The pipeline module calls [`register`] to drive evaluation; the
//! runtime module is not involved here. Per-job validation errors
//! collected during evaluation are returned as a single
//! `PipelineError`, not raised as Lua errors.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{IntoLua, Lua};

use miette::NamedSource;

use super::mirror;
use super::pipeline::{
    self, CompileResult, DefinitionError, Diagnostic, Job, PipelineError, RunFn,
};
use crate::fennel::Fennel;

/// Output of [`register`]: jobs and image successfully registered
/// from the script. Definition-time errors are returned via the `Err`
/// arm, not collected here.
#[derive(Debug)]
pub struct Registrations {
    pub jobs: Vec<Job>,
    pub image: Option<String>,
}

/// Evaluate `source` with the registration module bound and collect
/// what got registered.
///
/// Pre-graph rules run inside the callback, so a single bad job does
/// not abort the rest of the script — but if any rule fired, the
/// whole batch is returned as a `PipelineError` instead of partial
/// registrations.
pub fn register(fennel: &Fennel, source: &str, name: &str) -> CompileResult<Registrations> {
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
        )?;
        Ok(())
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
/// (`sh`, `secret`) live on the per-execution `Runtime` handle, not
/// here.
///
/// ```fennel
/// (local ci (require :quire.ci))
/// (ci.job :build [:quire/push]
///   (fn [{: sh : secret}]
///     (sh ["echo" (secret :github_token)])))
/// ```
pub struct Registration {
    pub jobs: Rc<RefCell<Vec<Job>>>,
    pub errors: Rc<RefCell<Vec<DefinitionError>>>,
    image: Rc<RefCell<Option<ImageRegistration>>>,
    pub source: Rc<String>,
}

impl IntoLua for Registration {
    fn into_lua(self, lua: &Lua) -> mlua::Result<mlua::Value> {
        lua.set_app_data(self);
        let table = lua.create_table()?;
        table.set("job", lua.create_function(register_job)?)?;
        table.set("image", lua.create_function(register_image)?)?;
        table.set("mirror", lua.create_function(mirror::register)?)?;
        // Carry forward the runtime stub from the placeholder
        // `quire.ci` table seeded by `Fennel::new`. `register_module`
        // overwrites `package.loaded["quire.ci"]` with the value we
        // return; reusing the existing stub keeps references captured
        // before registration (and any held by previously preloaded
        // modules like `quire.stdlib`) pointing at the same Lua table
        // that `RuntimeHandle::install` mutates.
        let package: mlua::Table = lua.globals().get("package")?;
        let loaded: mlua::Table = package.get("loaded")?;
        let placeholder: mlua::Table = loaded.get("quire.ci")?;
        let runtime: mlua::Table = placeholder.get("runtime")?;
        table.set("runtime", runtime)?;
        table.into_lua(lua)
    }
}

impl Registration {
    /// Push a registered job after enforcing id uniqueness. On
    /// collision, records `DuplicateJob` against the caller's source
    /// line and drops the new job; the first registration wins.
    pub fn add_job(&self, job: Job, line: u32) {
        let mut jobs = self.jobs.borrow_mut();
        if jobs.iter().any(|j| j.id == job.id) {
            let span = pipeline::span_for_line(&self.source, line);
            self.errors
                .borrow_mut()
                .push(DefinitionError::DuplicateJob {
                    job_id: job.id,
                    span,
                });
            return;
        }
        jobs.push(job);
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
            let span = pipeline::span_for_line(&r.source, line);
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
/// pointing back at the user's source. Enforces the user-facing
/// reserved-slash rule: ids may not contain `/`, since the `quire/`
/// namespace is reserved for built-in helpers (see `mirror::register_mirror`).
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

    if id.contains('/') {
        let span = pipeline::span_for_line(&r.source, line);
        r.errors
            .borrow_mut()
            .push(DefinitionError::ReservedSlash { job_id: id, span });
        return Ok(());
    }

    // Arity check: one-arg run-fns use the old `(fn [{: sh}] …)`
    // pattern. Reject them so users get a clear message instead of
    // a runtime-nil panic when the ambient runtime is absent.
    //
    // Fail open on `debug.getinfo` errors — if the debug library is
    // unavailable or the call shape changes, treat the function as
    // zero-arg and let the user surface any real arity mismatch at
    // execution time.
    let nparams: u32 = lua
        .load("return debug.getinfo(...).nparams")
        .call(&run_fn)
        .unwrap_or(0);
    if nparams != 0 {
        let span = pipeline::span_for_line(&r.source, line);
        r.errors
            .borrow_mut()
            .push(DefinitionError::ArityViolation { job_id: id, span });
        return Ok(());
    }

    match Job::new(id, inputs, RunFn::Lua(run_fn), line, &r.source) {
        Ok(job) => r.add_job(job, line),
        Err(e) => r.errors.borrow_mut().push(e),
    }
    Ok(())
}
