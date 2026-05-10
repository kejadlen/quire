# quire — `.quire/ci.fnl` design

The job spec language. Sibling to CI.md (which covers the runtime). This doc is about what `.quire/ci.fnl` *looks like* and the model it expresses.

## Framing

A CI config is a **dataflow graph of jobs**. Each job is a function from inputs to outputs. Edges are input references: job B taking job A as an input creates an A → B edge.

Inputs come in two flavors, used uniformly:

* **Job references.** `[:build]` — depend on another job's outputs.
* **Source references.** `[:quire/push]` — depend on an external event. The runner provides the outputs. Builtins live under the `quire/` namespace; user job ids cannot contain `/`.

There's no structural distinction between "trigger jobs" and "regular jobs." Sources are just things you list as inputs, in the same place as job references.

The mental model: **jobs are functions from inputs to outputs; sources are reserved input names whose outputs the runner provides; runs are slices of the graph that fire when a source's event arrives.**

This is closer to Concourse's resources-and-jobs model than to GitHub Actions' triggers-and-jobs model. More elegant, less familiar. Worth being deliberate about.

## Pipeline-level container image

```
(ci.image "rust:1.76")
```

Top-level form, called once before any `(ci.job ...)`. Declares the image used to start the run's container; every `(sh ...)` call from every job in the run is `docker exec`'d into this container. Pipelines that need heterogeneous images per job will get a per-job override later — for now, one image per pipeline keeps the model simple.

Calling `ci.image` more than once errors with the same shape as other duplicate-registration errors.

A pipeline can also build its image from a checked-in `.quire/Dockerfile` instead of declaring a public image. The resolution order is `(ci.image ...)` → `.quire/Dockerfile` → error.

> **v0 status:** the docker executor only honors `.quire/Dockerfile` today; `(ci.image ...)` is parsed and validated but not yet wired into the executor. Pipelines targeting docker need a `.quire/Dockerfile` until the declared-image path lands.

## Mirroring with `(ci.mirror ...)`

```
(ci.mirror "https://github.com/example/repo.git"
  {:secret :github_auth_header
   :tag    (fn [push] (.. "quire-" (string.sub push.sha 1 8)))
   :refs   ["refs/heads/main"]   ; optional
   :after  [:test]})             ; optional
```

Top-level form. Registers a singleton `quire/mirror` job that tags the pushed commit and `git push`es the configured refs (plus the tag) to the remote.

Options:

- `:secret` (required) — name of a secret in the global `:secrets` map. The secret's value is passed verbatim as an `http.extraHeader` value, so it should be the entire header (e.g. `"Authorization: Bearer ..."`).
- `:tag` (required) — function `(fn [push] tag-name)`. Called at execute time with the `quire/push` table; the result names the tag created on `push.sha` and pushed alongside the configured refs.
- `:refs` (optional, default empty) — list of refspecs to push. Doubles as a trigger filter: when set, the mirror runs only if the trigger ref is in the list. When empty, the mirror always runs and pushes the trigger ref.
- `:after` (optional) — extra job ids the mirror should sequence after, listed as inputs alongside the implicit `:quire/push`.

`(ci.mirror ...)` may be called once per pipeline; a second call collides on the reserved `quire/mirror` id and registers a `DuplicateJob` error.

## The `job` primitive

```
(job id inputs run)
```

Three positional arguments:

* **`id`** — keyword. The job's identity. Cannot contain `/`.
* **`inputs`** — list of names. Each is a job id or a source ref. Must be non-empty. v1: strings/keywords only (see "Future: input args" below).
* **`run`** — function from inputs to outputs (a table) or `nil` (skipped).

That's the entire surface. Conditional firing, output extraction, follow-up commands — all done inside `run` using runtime primitives. Image lives at the pipeline level (see above), not on individual jobs. Fennel-as-code means there's no need for config-language conveniences when a function will do.

If a fourth concept ever genuinely needs to be expressible at the job level (per-job image override, timeout, retry policy, secret scoping), that's the moment to introduce a map-form variant — `(job id {:inputs ... :run ... :image ...})`. Migration would be mechanical. Until then, the positional form is shorter and reads better for the actual surface.

## Inputs

```
[:quire/push :compute-version]
```

A list of names. Each name is either a job id (defined elsewhere via `(job :foo ...)`) or a source ref (a reserved name in the `quire/` namespace — for v1, just `:quire/push`). The runner gathers each named input's outputs and passes them in as a table on the function's argument.

The dependency graph is *derived* from the inputs list. No separate `:needs` field. Topological sort and cycle detection run over the input graph. Sources are leaf nodes (nothing flows into them).

**Dependency without data.** If you depend on a job for ordering but don't read its outputs, list it anyway: `[:setup :quire/push]`. The runner doesn't enforce that you read what you list. (If a convention helps readability, prefix unused inputs with underscore: `[:_setup :quire/push]`.)

### Accessing inputs

Run-fns are zero-arg functions. The runtime is available as a global `runtime` table whose `__index` metatable dispatches `sh`, `secret`, and `jobs` to closures over the active runtime:

```
(fn []
  (let [push (runtime.jobs :quire/push)]
    (runtime.sh ["git" "checkout" push.sha])))
```

`runtime.jobs` returns the outputs for `name` if `name` is a transitive ancestor of the calling job in the input graph; an unknown or non-ancestor name raises a Lua error. Self-lookup is rejected. Sources and jobs share one namespace — `(runtime.jobs :quire/push)` reads the source's outputs uniformly.

The `runtime.` prefix is a visible explicit-context marker so reviewers can grep for effect sites. Accessing `runtime` outside a run-fn raises: `runtime accessed outside a job — primitives are only available while a run-fn is executing`.

> **v0 status:** `(jobs :quire/push)` is wired. Job-to-job outputs (where `(jobs :build)` returns a job's `run-fn` return value) are not — there's no writer API yet, and a reachable name with no recorded outputs returns `nil`.

### Sources

For v1, the only source is `:quire/push`. Outputs:

```
{:sha             "abc123..."
 :ref             "refs/heads/main"
 :branch          "main"           ; nil if the ref is a tag
 :tag             nil              ; set if the ref is a tag
 :previous-sha    "def456..."      ; the ref's previous head; nil for new refs
 :files-changed   [...]            ; paths changed between previous-sha and sha
 :pusher          "alice"
 :commit-message  "..."}
```

Every push to any ref fires a run that includes every job whose transitive inputs include `:quire/push`. Filtering "which pushes do I care about" happens inside `run` — return `nil` to skip:

```
(job :test-main [:quire/push]
  (fn []
    (let [push (runtime.jobs :quire/push)]
      (when (= "main" push.branch)
        (runtime.sh (.. "git checkout " push.sha " && cargo test"))))))

(job :release [:quire/push]
  (fn []
    (let [push (runtime.jobs :quire/push)]
      (when (and push.tag (string.match push.tag "^v"))
        (runtime.sh (.. "publish " push.tag))))))
```

Fennel's `when` returns `nil` if the predicate is false, otherwise the body. That nil propagates out as the `run` return value, the runner records the job as skipped. The gate and the work are in the same expression.

This means **every push starts a run**, even if no job's predicate matches. Skipped jobs show in the run record as skipped. For a personal forge this is fine; the run-creation cost is small and the explicit skip record is useful debugging information ("did my filter match?"). If push frequency ever makes this wasteful, a future pre-execution skip hook is the escape valve — but the v1 model is "skipping is just early-return."

### Future: input args

Source types that need configuration — cron schedules, webhook paths — can't be expressed as bare keywords. The planned shape is a constructor call returning a value the runner recognizes:

```
(job :nightly-audit [(cron :daily)]
  (fn []
    (let [tick (runtime.jobs :cron)] ...)))

(job :hourly-check [(cron :every "1h" :as :hourly)]
  (fn []
    (let [tick (runtime.jobs :hourly)] ...)))
```

`cron`, `webhook`, etc. would be quire-provided functions in the eval scope. They return marker values; the runner inspects the inputs list for them, registers their event sources, instantiates runs when they fire. The `:as` keyword names the binding when the default name (the source type) would collide.

The same constructor form is the natural place for **cherry-picking job outputs** when that becomes desired: `(output :build :sha :as :commit)` would name a single output of an upstream job. Same mechanism, different target.

**v1 supports only string/keyword inputs.** Constructor calls are the planned extension for cron, webhook, and cherry-picking — not implemented. The shape above is settled enough to commit to; the implementation waits until cron is the second source we want.

### Validation

Three structural rules at registration eval, plus one at parse time. All fail-closed.

1. **Acyclic.** No cycles in the input graph. Detected by Kahn's algorithm; error names the cycle.
2. **Non-empty inputs.** Every job must list at least one input. The error tells the user what to fix:
   `Job 'setup' has empty inputs. Pass [:quire/push] (or another input) as the second argument so it has something to fire it.`
3. **Reachability.** Every job's transitive inputs must include at least one source ref. Pure job-to-job chains with no source at the root are dead code; the error names the orphaned jobs.
4. **No `/` in user job ids** (parse time). Error: `Job id 'foo/bar' contains '/', which is reserved for the 'quire/' source namespace. Use 'foo-bar' or another delimiter.`

A bad `ci.fnl` push gets a CI run that fails immediately with the parse error, same path as a Fennel syntax error.

## `run` — the only primitive

`run` is a host-side Fennel function called when the job is about to execute. It receives the runtime handle and returns either:

* **A table** — the job's outputs. Available to dependent jobs through `(jobs <this-job>)`.
* **`nil`** — the job is skipped. Dependents see `(jobs <this-job>)` return `nil`.

That's the whole contract. No sugar layer, no introspection, no defaulting. The runner records what was returned.

Inside `run`, the function uses **runtime primitives** exposed on the ambient `runtime` global. The most important is `(runtime.sh cmd opts?)`, which `docker exec`'s a command into the run's container and returns a result table:

```
(job :test [:quire/push]
  (fn []
    (let [push (runtime.jobs :quire/push)]
      (runtime.sh ["git" "checkout" push.sha])
      (runtime.sh "cargo test"))))
```

`(sh ...)` returns `{:exit :stdout :stderr :cmd}`. The run-fn can branch on that — checking exit, parsing stdout, deciding whether to issue follow-up commands. That dynamism is the whole reason ci.fnl is Fennel and not YAML:

```
(job :test-and-package [:quire/push]
  (fn []
    (let [push (runtime.jobs :quire/push)]
      (runtime.sh ["git" "checkout" push.sha])
      (let [test (runtime.sh "cargo test")]
        (when (= 0 test.exit)
          (let [pkg (runtime.sh "tar czf out.tar.gz target/release")]
            {:exit pkg.exit
             :artifacts ["out.tar.gz"]
             :test-stdout test.stdout}))))))
```

If the test fails, the outer `(when ...)` returns nil → job skipped. If it passes, the package step runs and the function returns a custom output table. One mechanism, scales from "run a command" to "orchestrate a multi-step pipeline."

`sh` is the only host-effect primitive. There is no `(container ...)` form — the run's container is started by the runner before the run-fn is invoked, and every `sh` call tunnels into it via `docker exec`. Making `sh` the chokepoint is what lets the in-process VM sandbox (`io`/`os`/`debug` removed from the execute VM) actually mean something — the script can't quietly bypass logging or persistence by reaching for `os.execute`.

### Why `run` is "just a function"

Earlier drafts of this design had three return shapes (string, list of strings, table) plus an `:outputs` field for declarative output extension plus a `:when` field for conditional firing plus an `:image` field for the default container image. All gone. They were paying for conveniences that aren't conveniences in a code-first config:

* **String sugar.** `:run "cargo test"` saves about ten characters over `(fn [] (runtime.sh "cargo test"))`. Not worth a second mental model.
* **`:outputs` declarative extension.** "Read coverage.json after the command exits" is a Fennel one-liner inside `run`: `(let [r (sh "...")] {:exit r.exit :coverage (read-json "coverage.json")})`. Helpers compose to clean up repetition.
* **`:when`.** Returning `nil` from `run` already means "skip." Filtering and work end up in the same expression, which makes the intent more visible, not less.
* **`:image`.** Image is declared once at the pipeline level via `(ci.image ...)`. Per-job override can be added as a map-form opts arg if a pipeline ever needs heterogeneity.

The residual things that *aren't* "just functions" — the inputs list and the id — are the ones that genuinely need to be language-level. They define the graph and the identity. Everything else is user-space.

## Runtime primitives

Exposed on the ambient `runtime` global inside each run-fn. Zero-arg functions — no destructuring needed. Accessing `runtime` outside a run-fn raises an error.

* `(runtime.jobs name)` — return outputs for `name` (a transitive ancestor of the calling job, or a source ref). Errors if `name` is not in the calling job's transitive inputs.
* `(runtime.sh cmd opts?)` — `docker exec` a command into the run's container, return `{:exit :stdout :stderr :cmd}`. `cmd` is either a string (run under `sh -c` inside the container) or a non-empty sequence of strings (argv, no shell). `opts` accepts `:env` (table of overrides) and `:cwd` (path inside `/work`).
* `(runtime.secret name)` — resolve a named secret from the operator's config. Errors if the name isn't declared.
* `(runtime.read-file path)`, `(runtime.read-json path)`, `(runtime.write-file path content)` — workspace I/O. Paths relative to the workspace.
* `(runtime.log msg)` — append to the job's log file. Visible in the web UI.
* `(runtime.env name)` — read an environment variable from the runner's environment.

Each of these blocks the Fennel function until it returns. Multi-`sh`-call parallelism inside one job is a v2 want; the v1 model is "the function runs sequentially, calling primitives that block."

`sh` is the only host-effect channel. There is no `(container ...)` primitive — the run's container is started by the runner before any run-fn executes (with the image declared via `(ci.image ...)` at the pipeline level), and every `sh` call execs into it via `docker exec`. Stdout and stderr stay separated (no TTY); ordering is approximate but each chunk has its own timestamp in the JSONL log.

> **v0 status:** `sh`, `secret`, and `jobs` are bound today. `sh` currently shells out on the host; the per-run container + `docker exec` tunneling is planned (see backlog `lpmoszxo`, `knmkqkvx`). `read-file`/`read-json`/`write-file`, `log`, and `env` are planned and tracked separately.

The execute VM is sandboxed (no `io`/`os`/`debug`), so `runtime.sh` is the documented chokepoint for any host effect — `os.execute` and `io.open` are not available alternates. See CI.md for the full sandbox shape and the bwrap opt-in for the untrusted-code threat model.

`runtime` is also reachable as a module: `(let [{: sh : secret} (require :quire.runtime)] …)`. Same table, same closures — useful for library code that wants its dependencies explicit.

## Stdlib (`quire.stdlib`)

Helpers that compose runtime primitives into common recipes. Embedded into the binary; available via `(require :quire.stdlib)` from any run-fn.

The kernel (`sh`/`secret`/`jobs`) stays small. Higher-level operations like tag-and-push live in Fennel where they're easier to read and evolve.

```
(local {: mirror} (require :quire.stdlib))

(ci.job :mirror [:quire/push :test]
  (fn []
    (let [push (runtime.jobs :quire/push)
          auth (runtime.secret :github_auth_header)]
      (mirror {:url         "https://github.com/example/repo.git"
               :auth-header auth
               :sha         push.sha
               :tag         (.. "quire-" (string.sub push.sha 1 8))
               :git-dir     (. push :git-dir)
               :refs        ["refs/heads/main"]}))))
```

Available helpers:

* `(mirror opts)` — tag a commit and push it (plus optional refs) to a remote. `opts.url`, `opts.auth-header`, `opts.sha`, `opts.tag`, and `opts.git-dir` are required; `opts.refs` defaults to `[]`. The caller resolves the credential (typically via `runtime.secret`) and passes the full HTTP header line as `:auth-header`; mirror passes it to git via `GIT_CONFIG_*` env vars rather than `-c http.extraHeader=…` in argv, so it doesn't appear in `ps` listings. Returns `{:tag :pushed_refs}`. Raises on missing required opts or non-zero git exits.

`(ci.mirror …)` (the registration-time form) remains as a convenience wrapper that registers a singleton `quire/mirror` job. Use the stdlib form when you want to mirror conditionally or as part of a larger run-fn.

## A worked example

```
(local ci (require :quire.ci))

(ci.image "rust:1.76")  ; one image for the whole pipeline

;; Test on every push to main
(ci.job :test [:quire/push]
  (fn []
    (let [push (runtime.jobs :quire/push)]
      (when (= "main" push.branch)
        (runtime.sh ["git" "checkout" push.sha])
        (runtime.sh "cargo test --all-features")))))

;; Build only if test passed
(ci.job :build [:test :quire/push]
  (fn []
    (let [push (runtime.jobs :quire/push)
          test (runtime.jobs :test)]
      (when (and test (= 0 test.exit))
        (runtime.sh ["git" "checkout" push.sha])
        (let [r (runtime.sh "cargo build --release")]
          {:exit r.exit
           :artifacts ["target/release/quire"]})))))

;; Deploy on push to main only
(ci.job :deploy [:build]
  (fn []
    (when (runtime.jobs :build)
      (runtime.sh "scp target/release/quire host:/usr/local/bin/"))))

;; Tagged release: publish to a registry
(ci.job :publish [:quire/push]
  (fn []
    (let [push (runtime.jobs :quire/push)]
      (when (and push.tag (string.match push.tag "^v"))
        (runtime.sh ["git" "checkout" push.tag])
        (runtime.sh "cargo publish")))))
```

What this expresses:

* Every push fires a run. The test job checks `push.branch` and returns nil for non-main pushes; the build/deploy chain skips with it (their inputs are nil, their `(when ...)` checks see nil).
* Tagged pushes additionally fire `:publish`, which has its own predicate.
* The "test passed" check in `:build` is visible in code rather than implicit. More verbose than a `:when` field, but the verbosity is honest about what's happening.
* All jobs run inside the same per-run container started from `rust:1.76`. `cargo`, `git`, and `scp` are expected to be present in the image (or installed by an earlier `sh` in the run); pipelines that need different toolchains today should pick an image that has all of them, or wait for per-job image override.

## Evaluation timing

> **v0 status:** the three-context model below is the eventual target. Initial implementation collapses to a single in-process eval per run — registration and per-job execution happen together at run start. The model expands back out to three contexts when cross-job inputs (job B consuming job A's outputs) make per-job re-eval necessary.

`ci.fnl` is evaluated in **three contexts**, all in-process inside `quire serve` (see CI.md for the threat model and the bwrap opt-in for untrusted code):

1. **Registration eval.** When `ci.fnl` changes on the default branch. The runner walks the resulting job set, runs structural validation (cycles, non-empty inputs, reachability, namespace rule). For v1, nothing else needs to happen here — `:quire/push` is implicit, requires no registration. When source types that need registration arrive (cron schedules, webhook routes), they'll be discovered here via the constructor form in inputs.
2. **Run eval.** When a push arrives and a run starts. The runner evaluates `ci.fnl` to get the current job set, computes which jobs are reachable from `:quire/push`, schedules them.
3. **Per-job eval.** When a job is about to execute, its `run` function is invoked with concrete input values.

The three-context model means **`ci.fnl` is re-evaluated more than you might expect.** Pure functions, no caching across runs. This is fine — eval is fast — but worth knowing if a future helper does expensive work at the top level (parsing a large file, hitting a network endpoint). Top-level work runs three times per change, plus once per job. Move expensive work into `run` where it runs once per job execution.

## Open questions

* **Source events with no matching jobs.** If `ci.fnl` has no jobs whose transitive inputs include `:quire/push`, do pushes still create empty runs? Probably no — skip silently. But worth being explicit.
* **What's the exact set of runtime primitives?** `sh`, `read-file` are obvious. Less obvious: do we expose `tcp-connect`, `http-get`? They'd enable real "jobs as observers" patterns, but they're a long road into "Fennel is a real programming environment." Probably no, defer.
* **Artifacts as inputs.** Job B with `[:build]` as inputs — does B's workspace start with build's artifacts already in place? Under per-run container, `/work` is shared across jobs already; artifacts written by job A are visible to job B by default. The open question is whether *outputs* declared from a job carry artifact paths the runner should pin for retention beyond the run.
* **Image pre-pull.** With a single pipeline-level `(ci.image ...)` declaration, the runner knows the image up front and can pull before starting the run container. Pull-on-demand at `docker run` time works too. A `quire ci pull <image>` command lets users warm explicitly if they want to avoid first-push latency.
* **Error semantics inside `run`.** What if it throws? Job marked failed, exception text into the log. What if it returns a malformed value (not nil, not a table)? Mark failed, log a schema warning.
* **Push payload size.** `:quire/push.files-changed` could be huge for a large merge. Do we cap it? Stream it differently? Defer to first time it bites.
* **Composition across files.** A `quire/stdlib.fnl` of common helpers, or per-repo Fennel modules. Real want eventually; not v1.
* **Pre-execution skip hook.** "Every push starts a run" is fine for personal scale. If it ever isn't, a hook that runs *before* workspace materialization to skip the whole run is the escape valve. Currently you can return nil from any `run` to skip that job, but the run still happens.
* **Map-form variant trigger.** What's the threshold for switching from `(job id inputs run)` positional to `(job id {:inputs ... :run ... :extra ...})` map-form? First option that genuinely needs to exist at the job level — likely candidates would be per-job timeout or retry policy. None planned for v1.

## Locked-in decisions

* **`(job id inputs run)`** — three positional arguments. No options map; if a fourth option ever needs to exist, that's the moment to introduce a map-form variant.
* **`id`** is a keyword; cannot contain `/`. Validation rule, parse-time error.
* **`inputs`** is a non-empty list of names. Each is either a job id or a source ref (reserved name in the `quire/` namespace).
* **v1 supports only strings/keywords in `inputs`.** Constructor calls (for cron, webhook, output cherry-picks) are the planned extension; shape settled, implementation deferred.
* **Builtins live under `quire/`**; user job ids cannot contain `/`.
* **For v1, the only source is `:quire/push`.** Cron, webhook, manual deferred.
* **Filtering happens inside `run`** by returning `nil`. Every push starts a run; jobs that return nil from `run` are skipped.
* **Ambient `runtime` global.** Run-fns are zero-arg `(fn [] …)`. The runtime is installed as a global Lua table whose `__index` metatable dispatches `runtime.sh`, `runtime.secret`, and `runtime.jobs` to closures over the active runtime. The `runtime.` prefix makes effect sites grep-able. One-arg run-fns are rejected at registration with a clear error message.
* **`(runtime.jobs name)` is the only accessor for upstream outputs**, covering both source refs and job outputs. Transitive ancestors are visible; non-ancestors and unknown names raise a Lua error.
* **Dependency graph derived from the inputs list**, not declared separately. No `:needs`.
* **Four structural validations**: acyclic (registration eval), non-empty inputs (registration eval), reachability from a source (registration eval), no `/` in user job ids (parse time). All fail-closed with named-target error messages.
* **`run` is a zero-arg function** `(fn [] …)`. Returns a table (the outputs) or `nil` (skipped). Runtime primitives accessed via the ambient `runtime` global. No sugar.
* **`(runtime.sh cmd opts?)` is the only host-effect primitive.** `docker exec`s into the run's container; returns `{:exit :stdout :stderr :cmd}`. There is no `(container ...)` form. The execute VM is sandboxed (no `io`/`os`/`debug`) so `runtime.sh` is the documented chokepoint.
* **`(ci.image <name>)` declares the image** at the pipeline level. One image per pipeline. Per-job override deferred until pipelines actually need heterogeneity; would arrive as a map-form `(ci.job ...)` opts arg.
* **Three eval contexts** — registration, run start, per job — all in-process inside `quire serve`. Sandboxing model and threat model are described in CI.md.
* **Source registration sourced from the default branch only** (relevant once registration becomes meaningful — for v1 it's a no-op since `:quire/push` needs no registration).
