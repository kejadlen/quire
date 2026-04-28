# quire — `.quire/ci.fnl` design

The job spec language. Sibling to CI.md (which covers the runtime). This doc is about what `.quire/ci.fnl` *looks like* and the model it expresses.

## Framing

A CI config is a **dataflow graph of jobs**. Each job is a function from inputs to outputs. Edges are input references: job B taking job A as an input creates an A → B edge.

Inputs come in two flavors, used uniformly:

- **Job references.** `[:build]` — depend on another job's outputs.
- **Source references.** `[:quire/push]` — depend on an external event. The runner provides the outputs. Builtins live under the `quire/` namespace; user job ids cannot contain `/`.

There's no structural distinction between "trigger jobs" and "regular jobs." Sources are just things you list as inputs, in the same place as job references.

The mental model: **jobs are functions from inputs to outputs; sources are reserved input names whose outputs the runner provides; runs are slices of the graph that fire when a source's event arrives.**

This is closer to Concourse's resources-and-jobs model than to GitHub Actions' triggers-and-jobs model. More elegant, less familiar. Worth being deliberate about.

## The `job` primitive

```fennel
(job id inputs run)
```

Three positional arguments:

- **`id`** — keyword. The job's identity. Cannot contain `/`.
- **`inputs`** — list of names. Each is a job id or a source ref. Must be non-empty. v1: strings/keywords only (see "Future: input args" below).
- **`run`** — function from inputs to outputs (a table) or `nil` (skipped).

That's the entire surface. Image selection, conditional firing, output extraction — all done inside `run` using runtime primitives. Fennel-as-code means there's no need for config-language conveniences when a function will do.

If a fourth concept ever genuinely needs to be expressible at the job level (per-job timeout, retry policy, secret scoping), that's the moment to introduce a map-form variant — `(job id {:inputs ... :run ... :timeout ...})`. Migration would be mechanical. Until then, the positional form is shorter and reads better for the actual surface.

## Inputs

```fennel
[:quire/push :compute-version]
```

A list of names. Each name is either a job id (defined elsewhere via `(job :foo ...)`) or a source ref (a reserved name in the `quire/` namespace — for v1, just `:quire/push`). The runner gathers each named input's outputs and passes them in as a table on the function's argument.

The dependency graph is *derived* from the inputs list. No separate `:needs` field. Topological sort and cycle detection run over the input graph. Sources are leaf nodes (nothing flows into them).

**Dependency without data.** If you depend on a job for ordering but don't read its outputs, list it anyway: `[:setup :quire/push]`. The runner doesn't enforce that you read what you list. (If a convention helps readability, prefix unused inputs with underscore: `[:_setup :quire/push]`.)

### Accessing inputs

The function receives an outer table with an `:inputs` key. Standard pattern is to destructure:

```fennel
(fn [{: inputs}]
  (.. "checkout " inputs.build.sha))
```

For source inputs whose names contain `/`, the dot-access syntax is awkward. **Destructure at the function arg** — both cleaner and less error-prone:

```fennel
(fn [{:inputs {:quire/push push}}]
  (.. "checkout " push.sha))
```

The `push` local rebinding is the recommended idiom for any source input. Use the same pattern when destructuring multiple inputs:

```fennel
(fn [{:inputs {:quire/push push : build : compute-version}}]
  (.. "deploying " compute-version.version " from " push.sha))
```

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

```fennel
(job :test-main [:quire/push]
  (fn [{:inputs {:quire/push push}}]
    (when (= "main" push.branch)
      (container {:image "rust:1.75"
                  :cmd (.. "git checkout " push.sha " && cargo test")}))))

(job :release [:quire/push]
  (fn [{:inputs {:quire/push push}}]
    (when (and push.tag (string.match push.tag "^v"))
      (container {:image "alpine"
                  :cmd (.. "publish " push.tag)}))))
```

Fennel's `when` returns `nil` if the predicate is false, otherwise the body. That nil propagates out as the `run` return value, the runner records the job as skipped. The gate and the work are in the same expression.

This means **every push starts a run**, even if no job's predicate matches. Skipped jobs show in the run record as skipped. For a personal forge this is fine; the run-creation cost is small and the explicit skip record is useful debugging information ("did my filter match?"). If push frequency ever makes this wasteful, a future pre-execution skip hook is the escape valve — but the v1 model is "skipping is just early-return."

### Future: input args

Source types that need configuration — cron schedules, webhook paths — can't be expressed as bare keywords. The planned shape is a constructor call returning a value the runner recognizes:

```fennel
(job :nightly-audit [(cron :daily)]
  (fn [{:inputs {: cron}}] ...))

(job :hourly-check [(cron :every "1h" :as :hourly)]
  (fn [{:inputs {: hourly}}] ...))
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

`run` is a host-side Fennel function (the container can't run Fennel) called when the job is about to execute, with all upstream inputs resolved. It returns either:

- **A table** — the job's outputs. Whatever keys are in it become available to dependent jobs as `inputs.<this-job>.<key>`.
- **`nil`** — the job is skipped. Dependents see `inputs.<this-job>` as `nil`.

That's the whole contract. No sugar layer, no introspection, no defaulting. The runner records what was returned.

Inside `run`, the function uses **runtime primitives** to do work. The most important is `(container {...})`, which runs a container and returns a result table:

```fennel
(job :test [:quire/push]
  (fn [{:inputs {:quire/push push}}]
    (container {:image "rust:1.75"
                :cmd (.. "git checkout " push.sha " && cargo test")})))
```

`(container ...)` returns `{:exit :stdout :stderr :duration}`. That's what `run` returns. The runner records it as the outputs.

For more complex jobs, the function does its own orchestration: multiple containers, host-side work between them, computed outputs derived from intermediate results:

```fennel
(job :test-and-package [:quire/push]
  (fn [{:inputs {:quire/push push}}]
    (let [test (container {:image "rust:1.75"
                           :cmd ["git checkout" push.sha "&&" "cargo test"]})]
      (when (= 0 test.exit)
        (let [pkg (container {:image "alpine"
                              :cmd "tar czf out.tar.gz target/release"})]
          {:exit pkg.exit
           :artifacts ["out.tar.gz"]
           :test-stdout test.stdout})))))
```

If the test fails, the outer `(when ...)` returns nil → job skipped. If it passes, the package step runs and the function returns a custom output table. One mechanism, scales from "run a command" to "orchestrate a multi-step pipeline."

### Why `run` is "just a function"

Earlier drafts of this design had three return shapes (string, list of strings, table) plus an `:outputs` field for declarative output extension plus a `:when` field for conditional firing plus an `:image` field for the default container image. All gone. They were paying for conveniences that aren't conveniences in a code-first config:

- **String sugar.** `:run "cargo test"` saves about ten characters over `(fn [_] (container {:image "rust:1.75" :cmd "cargo test"}))`. Not worth a second mental model.
- **`:outputs` declarative extension.** "Read coverage.json after the container exits" is a Fennel one-liner inside `run`: `(let [r (container {...})] {:exit r.exit :coverage (read-json "coverage.json")})`. Helpers compose to clean up repetition.
- **`:when`.** Returning `nil` from `run` already means "skip." Filtering and work end up in the same expression, which makes the intent more visible, not less.
- **`:image`.** Image lives on the `(container ...)` call where it's actually used. Lets a single job legitimately use multiple images.

The residual things that *aren't* "just functions" — the inputs list and the id — are the ones that genuinely need to be language-level. They define the graph and the identity. Everything else is user-space.

## Runtime primitives

Functions in scope inside `run`:

- `(container {opts})` — run a container, return `{:exit :stdout :stderr :duration}`. Opts: `:image`, `:cmd` (string or list), `:env`, `:cwd`, `:cache` (cache dir mount, defaults to job's image-keyed cache).
- `(sh cmd)` — run a command on the host, no container. For cheap utility work. Returns the same shape as `container`.
- `(read-file path)`, `(read-json path)`, `(write-file path content)` — workspace I/O. Paths relative to the workspace.
- `(log msg)` — append to the job's log file. Visible in the web UI.
- `(env name)` — read an environment variable from the runner's environment (typically secrets).

Each of these blocks the Fennel function until it returns. Multi-container parallelism inside one job is a v2 want; the v1 model is "the function runs sequentially, calling primitives that block."

The wallclock and memory limits on Fennel eval (10s, 512 MB by default — see CI.md) **don't apply to time spent inside primitives**, because the function blocks on real work. The runner accounts for container time separately. The eval budget is for Fennel-side computation between primitive calls.

## A worked example

```fennel
;; Helper: a parameterized test job
(fn rust-test [version]
  (job (.. "test-" version) [:quire/push]
    (fn [{:inputs {:quire/push push}}]
      (when (= "main" push.branch)
        (container {:image (.. "rust:" version)
                    :cmd [(.. "git checkout " push.sha)
                          "cargo test --all-features"]})))))

;; Matrix testing on every push to main
(each [_ v (ipairs [:1.75 :1.76 :stable])]
  (rust-test v))

;; Build only if all tests passed
(job :build [:test-1.75 :test-1.76 :test-stable :quire/push]
  (fn [{:inputs {:quire/push push : test-1.75 : test-1.76 : test-stable}}]
    (when (and test-1.75 test-1.76 test-stable
               (= 0 test-1.75.exit)
               (= 0 test-1.76.exit)
               (= 0 test-stable.exit))
      (let [r (container {:image "rust:1.75"
                          :cmd [(.. "git checkout " push.sha)
                                "cargo build --release"]})]
        {:exit r.exit
         :artifacts ["target/release/quire"]}))))

;; Deploy on push to main only
(job :deploy [:build]
  (fn [{:inputs {: build}}]
    (when build
      (container {:image "alpine"
                  :cmd "scp target/release/quire host:/usr/local/bin/"}))))

;; Tagged release: publish to a registry
(job :publish [:quire/push]
  (fn [{:inputs {:quire/push push}}]
    (when (and push.tag (string.match push.tag "^v"))
      (container {:image "rust:1.75"
                  :cmd [(.. "git checkout " push.tag)
                        "cargo publish"]}))))
```

What this expresses:
- Every push fires a run. Test jobs check `push.branch` and return nil for non-main pushes; build/deploy chain skips with them (their inputs are nil, their `(when ...)` checks see nil).
- Tagged pushes additionally fire `:publish`, which has its own predicate.
- The "all tests passed" check in `:build` is now visible in code rather than implicit. More verbose than a `:when` field, but the verbosity is honest about what's happening — and a helper (`(all-passed test-1.75 test-1.76 test-stable)`) would clean it up if the pattern repeats.

## Evaluation timing

> **v0 status:** the three-context model below is the eventual target. Initial implementation collapses to a single in-process eval per run — registration and per-job execution happen together at run start. The model expands back out to three contexts when cross-job inputs (job B consuming job A's outputs) and the subprocess sandbox land.

`ci.fnl` is evaluated in **three contexts**, all using the subprocess machinery from CI.md (10s wallclock, 512 MB memory cap):

1. **Registration eval.** When `ci.fnl` changes on the default branch. The runner walks the resulting job set, runs structural validation (cycles, non-empty inputs, reachability, namespace rule). For v1, nothing else needs to happen here — `:quire/push` is implicit, requires no registration. When source types that need registration arrive (cron schedules, webhook routes), they'll be discovered here via the constructor form in inputs.
2. **Run eval.** When a push arrives and a run starts. The runner evaluates `ci.fnl` to get the current job set, computes which jobs are reachable from `:quire/push`, schedules them.
3. **Per-job eval.** When a job is about to execute, its `run` function is invoked with concrete input values. Same subprocess, same limits, but per job.

The three-context model means **`ci.fnl` is re-evaluated more than you might expect.** Pure functions, no caching across runs. This is fine — eval is fast and bounded — but worth knowing if a future helper does expensive work at the top level (parsing a large file, hitting a network endpoint). Top-level work runs three times per change, plus once per job. Move expensive work into `run` where it runs once per job execution.

## Open questions

- **Source events with no matching jobs.** If `ci.fnl` has no jobs whose transitive inputs include `:quire/push`, do pushes still create empty runs? Probably no — skip silently. But worth being explicit.
- **What's the exact set of runtime primitives?** `container`, `sh`, `read-file` are obvious. Less obvious: do we expose `tcp-connect`, `http-get`? They'd enable real "jobs as observers" patterns, but they're a long road into "Fennel is a real programming environment." Probably no, defer.
- **Artifacts as inputs.** Job B with `[:build]` as inputs — does B's workspace start with build's artifacts already in place? Probably yes; otherwise the `:artifacts` output is data-only and you can't use them in subsequent containers. Implementation: artifacts unpacked into B's workspace before B's container starts.
- **Image pre-pull discoverability.** Without a top-level `:image` field, the runner can't statically know what images a job uses — it has to actually run the function (or analyze it, which is fragile). Probably acceptable for v1: pull-on-demand from `(container ...)` calls works fine, just with a one-time latency per new image. A `quire ci pull <image>` command lets users warm explicitly.
- **Error semantics inside `run`.** What if it throws? Job marked failed, exception text into the log. What if it returns a malformed value (not nil, not a table)? Mark failed, log a schema warning.
- **Push payload size.** `:quire/push.files-changed` could be huge for a large merge. Do we cap it? Stream it differently? Defer to first time it bites.
- **Composition across files.** A `quire/stdlib.fnl` of common helpers, or per-repo Fennel modules. Real want eventually; not v1.
- **Pre-execution skip hook.** "Every push starts a run" is fine for personal scale. If it ever isn't, a hook that runs *before* workspace materialization to skip the whole run is the escape valve. Currently you can return nil from any `run` to skip that job, but the run still happens.
- **Map-form variant trigger.** What's the threshold for switching from `(job id inputs run)` positional to `(job id {:inputs ... :run ... :extra ...})` map-form? First option that genuinely needs to exist at the job level — likely candidates would be per-job timeout or retry policy. None planned for v1.

## Locked-in decisions

- **`(job id inputs run)`** — three positional arguments. No options map; if a fourth option ever needs to exist, that's the moment to introduce a map-form variant.
- **`id`** is a keyword; cannot contain `/`. Validation rule, parse-time error.
- **`inputs`** is a non-empty list of names. Each is either a job id or a source ref (reserved name in the `quire/` namespace).
- **v1 supports only strings/keywords in `inputs`.** Constructor calls (for cron, webhook, output cherry-picks) are the planned extension; shape settled, implementation deferred.
- **Builtins live under `quire/`**; user job ids cannot contain `/`.
- **For v1, the only source is `:quire/push`.** Cron, webhook, manual deferred.
- **Filtering happens inside `run`** by returning `nil`. Every push starts a run; jobs that return nil from `run` are skipped.
- **Destructure source inputs at the function arg** — `(fn [{:inputs {:quire/push push}}] ...)` — to avoid awkward dot-access on `/`-containing keys.
- **Dependency graph derived from the inputs list**, not declared separately. No `:needs`.
- **Four structural validations**: acyclic (registration eval), non-empty inputs (registration eval), reachability from a source (registration eval), no `/` in user job ids (parse time). All fail-closed with named-target error messages.
- **`run` is a function** `(fn [{: inputs}] ...)`. Returns a table (the outputs) or `nil` (skipped). No sugar.
- **`(container {opts})` is the primary primitive** for running containers. Opts include `:image`, so a single job can use multiple images by making multiple container calls.
- **Three eval contexts** — registration, run start, per job — all using the same subprocess machinery and limits.
- **Source registration sourced from the default branch only** (relevant once registration becomes meaningful — for v1 it's a no-op since `:quire/push` needs no registration).
