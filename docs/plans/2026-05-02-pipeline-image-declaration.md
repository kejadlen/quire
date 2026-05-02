# Pipeline-level container image declaration

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `(ci.image "alpine")` to `ci.fnl` so pipelines can declare the container image for all their jobs. The executor resolves the image at run time: declared image → `.quire/Dockerfile` on the branch → `"debian"` default.

**Architecture:** Extend the `Registration` Lua module with an `image` function. The image string is stored in a shared `Rc<RefCell<Option<String>>>` (same pattern as `jobs`). After Fennel evaluation, the image is extracted and stored on `Pipeline`. No validation that an image is declared — the executor resolves a default at run time.

**Tech stack:** Rust, mlua (Lua bridge), miette (diagnostics), existing test patterns in `src/ci/pipeline.rs` and `src/ci/lua.rs`.

---

### Task 1: Add `DuplicateImage` validation error variant

**Files:**
- Modify: `src/ci/pipeline.rs` — add one new `ValidationError` variant

- [ ] **Step 1: Write the failing test**

Add to `src/ci/pipeline.rs` tests module:

```rust
#[test]
fn duplicate_image_variant_exists() {
    let err = ValidationError::DuplicateImage {
        span: miette::SourceSpan::from((0, 0)),
    };
    assert!(err.to_string().contains("image"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ci::pipeline::tests::duplicate_image_variant_exists -- --nocapture`
Expected: FAIL — `DuplicateImage` variant does not exist on `ValidationError`

- [ ] **Step 3: Write minimal implementation**

Add one variant to `ValidationError` in `src/ci/pipeline.rs`:

```rust
#[error("Pipeline image declared more than once.")]
DuplicateImage {
    #[label("duplicate image declaration")]
    span: SourceSpan,
},
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib ci::pipeline::tests::duplicate_image_variant_exists -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit**

```
Add DuplicateImage validation error variant
```

---

### Task 2: Wire `ci.image` into the Registration module and Pipeline

**Files:**
- Modify: `src/ci/pipeline.rs` — make `span_for_line` `pub(super)`, add `image` field and accessor to `Pipeline`, update `load` for new `ParseOutput`
- Modify: `src/ci/lua.rs` — add `image` field to `Registration`, add `register_image` callback, update `parse` return type

- [ ] **Step 1: Write the failing test**

Add to `src/ci/pipeline.rs` tests:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ci::pipeline::tests::load_registers_pipeline_image -- --nocapture`
Expected: FAIL — `image()` method does not exist on `Pipeline`

- [ ] **Step 3: Write minimal implementation**

**`src/ci/pipeline.rs` changes:**

Make `span_for_line` visible to `lua.rs`:

```rust
pub(super) fn span_for_line(source: &str, line: u32) -> SourceSpan {
```

Add `image` field to `Pipeline`:

```rust
pub struct Pipeline {
    jobs: Vec<Job>,
    graph: JobGraph,
    node_index: HashMap<String, NodeIndex>,
    fennel: Fennel,
    image: Option<String>,
}
```

Add accessor:

```rust
/// The container image declared via `(ci.image ...)`, if any.
/// The executor resolves the final image at run time:
/// declared image → `.quire/Dockerfile` → default.
pub fn image(&self) -> Option<&str> {
    self.image.as_deref()
}
```

Update `Pipeline::load` to use `ParseOutput`:

```rust
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
```

**`src/ci/lua.rs` changes:**

Add image tracking to `Registration`:

```rust
/// A pending image registration extracted from the Lua callback.
struct ImageRegistration {
    name: String,
    line: u32,
}

struct Registration {
    jobs: Rc<RefCell<Vec<std::result::Result<Job, ValidationError>>>>,
    image: Rc<RefCell<Option<ImageRegistration>>>,
    source: Rc<String>,
}
```

Add new return type for `parse`:

```rust
pub(super) struct ParseOutput {
    pub(super) jobs: Vec<std::result::Result<Job, ValidationError>>,
    pub(super) image: Option<String>,
}
```

Update `parse`:

```rust
pub(super) fn parse(
    fennel: &Fennel,
    source: &str,
    name: &str,
) -> Result<ParseOutput> {
    let jobs = Rc::new(RefCell::new(Vec::new()));
    let image = Rc::new(RefCell::new(None));
    let src = Rc::new(source.to_string());

    fennel.eval_raw(source, name, |lua| {
        lua.register_module(
            "quire.ci",
            Registration {
                jobs: jobs.clone(),
                image: image.clone(),
                source: src.clone(),
            },
        )
    })?;

    let image_name = image.borrow().as_ref().map(|i| i.name.clone());
    Ok(ParseOutput {
        jobs: jobs.take(),
        image: image_name,
    })
}
```

Update `IntoLua for Registration` to add `image` to the module table:

```rust
impl IntoLua for Registration {
    fn into_lua(self, lua: &Lua) -> mlua::Result<mlua::Value> {
        lua.set_app_data(self);
        let table = lua.create_table()?;
        table.set("job", lua.create_function(register_job)?)?;
        table.set("image", lua.create_function(register_image)?)?;
        table.into_lua(lua)
    }
}
```

Add the callback:

```rust
fn register_image(lua: &Lua, (name,): (String,)) -> mlua::Result<()> {
    let r = lua
        .app_data_ref::<Registration>()
        .ok_or_else(|| mlua::Error::external("quire.ci registration not installed on Lua VM"))?;
    let line = lua
        .inspect_stack(1, |d| d.current_line())
        .flatten()
        .map(|l| l as u32)
        .unwrap_or(0);
    let mut image = r.image.borrow_mut();
    match &*image {
        Some(_) => {
            let span = super::pipeline::span_for_line(&r.source, line);
            r.jobs
                .borrow_mut()
                .push(Err(ValidationError::DuplicateImage { span }));
        }
        None => {
            *image = Some(ImageRegistration { name, line });
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib ci::pipeline::tests::load_registers_pipeline_image -- --nocapture`
Run: `cargo test --lib ci::pipeline::tests::load_succeeds_without_image -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit**

```
Add ci.image function to register pipeline container image

Extends the quire.ci Lua module with an `image` function that
stores the container image name on the Pipeline struct. The image
is exposed via `Pipeline::image()` for the executor to read at
job spawn time. Pipelines without an image declaration load
successfully — the executor resolves a default at run time.
```

---

### Task 3: Reject duplicate `ci.image` calls

**Files:**
- Modify: `src/ci/pipeline.rs` — test

- [ ] **Step 1: Write the failing test**

Add to `src/ci/pipeline.rs` tests:

```rust
#[test]
fn load_errors_on_duplicate_image() {
    let source = r#"(local ci (require :quire.ci))
(ci.image "alpine")
(ci.image "ubuntu")
(ci.job :build [:quire/push] (fn [_] nil))"#;
    let result = Pipeline::load(source, "ci.fnl");
    assert!(result.is_err(), "duplicate image should fail");
    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("CI validation failed"), "expected validation error: {msg}");
}
```

- [ ] **Step 2: Run test to verify it passes**

The `register_image` callback from Task 2 already pushes `DuplicateImage` on the second call, so this should pass immediately. If it does, this test documents the invariant.

Run: `cargo test --lib ci::pipeline::tests::load_errors_on_duplicate_image -- --nocapture`
Expected: PASS

- [ ] **Step 3: Commit**

```
Test that duplicate ci.image declarations produce a validation error
```

---

### Task 4: Error when `ci.image` is called inside a run-fn

**Files:**
- Modify: `src/ci/run.rs` — add test
- No production changes needed

- [ ] **Step 1: Write the test**

The `quire.ci` module is cached by `require`. During execution, the `Registration` app data has been replaced by `Rc<Runtime>`, so calling `ci.image` inside a run-fn triggers the "registration not installed" error. This test locks in that behavior.

Add to `src/ci/run.rs` tests:

```rust
#[test]
fn execute_errors_when_image_called_in_run_fn() {
    let (_dir, quire) = tmp_quire();
    let runs = test_runs(&quire);
    let run = runs.create(&test_meta()).expect("create");

    let pipeline = load(
        r#"(local ci (require :quire.ci))
(ci.image "alpine")
(ci.job :bad [:quire/push]
  (fn [_]
    (ci.image "sneaky")))"#,
    );

    let err = run
        .execute(pipeline, HashMap::new(), std::path::Path::new("."))
        .expect_err("expected failure");
    let Error::JobFailed { job, source } = err else {
        unreachable!()
    };
    assert_eq!(job, "bad");
    let msg = source.to_string();
    assert!(
        msg.contains("registration not installed"),
        "expected registration error, got: {msg}"
    );
}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test --lib ci::run::tests::execute_errors_when_image_called_in_run_fn -- --nocapture`
Expected: PASS — naturally enforced by the app-data swap

- [ ] **Step 3: Commit**

```
Test that ci.image errors when called inside a run-fn
```

---

### Task 5: Fix existing tests broken by API changes

**Files:**
- Modify: `src/ci/pipeline.rs` — update `parse_results`/`parsed_jobs` helpers for `ParseOutput`
- Modify: `src/ci/mod.rs` — no changes needed (tests don't use `ParseOutput` directly)
- Modify: `src/ci/run.rs` — no changes needed (tests don't use `ParseOutput` directly)

- [ ] **Step 1: Run the full test suite**

Run: `cargo test --lib`
Expected: Failures in tests that use `parse_results`/`parsed_jobs` helpers, since `lua::parse` now returns `ParseOutput` instead of `Vec<Result<Job, ValidationError>>`

- [ ] **Step 2: Update helpers in `pipeline.rs` tests**

The `parse_results` helper currently calls `lua::parse` and returns the vec. Update it to unwrap `.jobs`:

```rust
fn parse_results(source: &str) -> Vec<std::result::Result<Job, ValidationError>> {
    let f = Fennel::new().expect("Fennel::new() should succeed");
    lua::parse(&f, source, "ci.fnl").expect("parse should succeed").jobs
}
```

- [ ] **Step 3: Run full test suite**

Run: `cargo test --lib`
Expected: All tests pass

- [ ] **Step 4: Commit**

```
Update test helpers for ParseOutput return type
```

---

### Task 6: Run full check suite and verify

**Files:**
- No changes expected

- [ ] **Step 1: Run `just all`**

Run: `just all`
Expected: Everything passes — fmt, clippy, test

- [ ] **Step 2: Mark task done**

```
ranger task edit vo --state done
```
