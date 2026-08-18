# quire — architecture

The system shape, the design decisions that are locked in, and the durable
design intent. This replaces the old `PLAN.md`: the build sequence that
lived there is done or superseded, and remaining work is tracked in the
`ranger` backlog. Dated design documents for individual changes live in
[`plans/`](./plans/).

## Architecture at a glance

The **host** does auth and network plumbing. The **container** is pure quire.

**Host-side:**

1. **openssh** — the host's sshd authenticates git/quire connections. A `Match User git` block uses `ForceCommand` to dispatch authenticated commands into the container via `docker exec`. One set of host keys, one `authorized_keys`, one process doing auth.
2. **Reverse proxy** — Caddy (likely). Terminates TLS, obtains certs, handles web authentication, injects an identity header (`Remote-User`, trusted because the proxy is the only ingress), and reverse-proxies to `quire serve` inside the container. The actual auth mechanism behind the proxy (OAuth, HTTP basic, an SSO layer like Authelia, whatever) is the proxy's problem; quire only sees the header.

**Container-side:**

1. **`quire` binary** — serves as both the HTTP server (`quire serve`) and the dispatch target (`quire exec <cmd>` invoked from the host's ForceCommand via `docker exec`). No sshd inside. Git hooks installed in each repo via `hook.<n>.command` config call back into the binary as `quire hook <n>`.
2. **CI** — triggered from the push-event listener inside `quire serve`; the pipeline itself runs in a separate `quire-ci` subprocess. See [`CI.md`](./CI.md) for the design, [`CI-STATE.md`](./CI-STATE.md) for what the code does today, and [`plans/2026-08-12-ci-rearchitecture.md`](./plans/2026-08-12-ci-rearchitecture.md) for where it's headed (per-run containers, a real queue).
3. **Git** — invoked as a subprocess by both the dispatch path (`git-receive-pack`, `git-upload-pack` invoked from `quire exec`) and the hooks.

**Access matrix:**

| Request state         | SSH (git push/pull) | Web: repos & history | Web: CI run logs |
|-----------------------|---------------------|----------------------|------------------|
| Authenticated         | yes                 | yes                  | yes              |
| Unauthenticated       | yes (via sshd keys) | yes (public repos)   | no               |

Repo content is public by default because most of it ends up on GitHub anyway. CI logs require auth because "my CI never prints secrets" is easy to break (env values echoed by a misbehaving script, stack traces with file paths, dependency debug output). Per-repo opt-ins cover the exceptions: `(private true)` for repos that should require auth even to browse; `(public_runs true)` for repos where build status is worth publishing.

**How auth is enforced.** The reverse proxy is the only web ingress — the container publishes its HTTP port to host loopback only, nothing else can reach it. When a request comes in, the proxy authenticates the user (by whatever scheme it's configured for), strips any client-supplied `Remote-User` header, and injects its own. Quire trusts that header because the proxy is the only source of it.

Stripping is load-bearing: without it, anyone could impersonate anyone by setting the header themselves. Quire's handlers read the header, apply per-repo visibility rules, and serve or 404 accordingly. A missing header means "unauthenticated" — handled gracefully, not an error.

**Why this shape.** SSH pass-through from host to container is a requirement (host sshd on 22 can't coexist with a second sshd bound to the same port). Once the host is doing auth for SSH, running another sshd in the container is redundant at best and confusing at worst. Putting web auth at the reverse proxy — rather than building it into quire — means the auth scheme can change (basic → OAuth → SSO) without touching the container, and quire's HTTP layer stays small and focused.

**SQLite for CI state, filesystem for everything else.** Refs and repo metadata are in the git repos themselves. Per-repo config is Fennel on disk. CI run/job state lives in SQLite at `/var/quire/quire.db` — the filesystem approach for runs hit the predicted "concurrency + aggregate queries" wall first, and the migration was a contained change. Migrations live under `migrations/` and are embedded into the binary via `include_str!`. Future tables (config snapshots, hook event audit, etc.) live in the same database.

## Volume layout

One volume mounted into the container:

```
/var/quire/
  quire.db                       # SQLite database
  repos/
    foo.git/
      quire/
    work/
      bar.git/
        quire/...
  runs/
    <repo>/<run-id>/
      workspace/                 # materialized checkout
      jobs/
        <job-id>/
          sh-<n>.log             # one CRI-format log file per (sh ...) call
  config.fnl                 # global config
```

Per-repo config (`mirrors`, etc.) is checked into the repo at `.quire/config.fnl`, not stored in the bare repo's `quire/` directory. The `quire/` directory holds only generated artifacts.

No SSH config or host keys in this volume — those live on the host. The container image brings the `quire` binary and git; the volume brings repos, runs, and per-repo state.

`docker compose down && up` loses nothing in the volume. Host identity (ssh host keys, reverse-proxy certs and state) persists on the host.

## Host configuration

The container expects a specific host setup: an sshd `Match User git` block dispatching into the container, a reverse proxy terminating TLS and injecting `Remote-User`, and a container start command that mounts the volume and publishes the HTTP port to loopback only. Reference configs and setup steps live in [`host/`](./host/README.md), version-controlled with the code rather than pretending to be handled by the container. This is a real cost — more moving parts than "one container does everything" — but it's the honest shape of the problem.

## Future: all-in-one image variant (not building)

Worth noting for completeness: nothing in the base image's design prevents a second, derivative image that layers sshd + a supervisor on top and handles the auth layer inside the container. That would be the turnkey "docker run this and you have a git server" story — useful for people deploying quire on a VPS without existing host infrastructure, or for quick evaluation.

The shape, sketched: `quire:standalone` extends `quire:latest` with:

- openssh-server.
- A supervisor (tini or s6) so sshd and `quire serve` can run together.
- An entrypoint that starts both processes.
- sshd configured with `ForceCommand /usr/local/bin/quire exec "$SSH_ORIGINAL_COMMAND"` in its sshd_config.
- Authorized keys from a volume-mounted file or env var.

Everything downstream of `quire exec` is identical to the host-mediated path — same allowlist, same dispatch logic — so there's no divergent code to maintain.

Flagging the possibility now because it costs nothing at design time (the `quire exec` dispatch boundary is already the right shape for either deployment), and it'd be a thoughtful contribution from someone who wants it later. Not building it — the base image plus reference host configs cover the deployment story I actually want.

## Client assumptions

The primary client is **jj** (Jujutsu), not git directly. In practice this changes very little server-side — `jj git push` speaks the git wire protocol, so `git-receive-pack` and `git-upload-pack` handle it transparently. A few things are still worth keeping in mind because they shape UX defaults, not protocol handling:

- **Force-pushes are routine, not exceptional.** jj users rebase and amend constantly; force-pushing a bookmark is part of the normal flow. CI cancels an in-flight run when a new push supersedes it for the same ref, and records the cancellation in the run history.
- **Short-lived refs are common.** jj's push-anywhere workflows can produce refs like `push-xxxxxxxx` that exist only to move work around. The web UI shouldn't give every ref equal prominence — surface branches the operator has opinions about (main, plus anything pinned in per-repo config), fold the rest into a "see all" affordance.
- **Commit SHAs aren't stable identities.** Don't build URLs or features that assume a given SHA will exist forever. Prefer refs where possible; accept that deep-linking to a SHA may 404 after a rebase.
- **No assumption of linear history.** Even post-rebase, merge commits and non-linear shapes show up. The log view shouldn't require linearity.

Nothing here requires jj-specific code. It's all just "don't make git-flow-shaped assumptions."

## Planned: email notifications

Not built yet. The design intent: shell out to `msmtp` (or `sendmail`-compatible) as a subprocess — the container ships `msmtp`, and global config (`config.fnl`) specifies SMTP server + credentials once. Quire builds the message, pipes it to `msmtp -t`, done. No native SMTP library, no retry queue, no HTML templates; a plain-text email with subject and body is the whole thing.

What triggers a notification, per-repo-configurable in `.quire/config.fnl`:

- CI run failed (default: on, if any address is configured)
- CI run that was previously failing now succeeds (default: on — the "fixed" notification is the one you actually want)
- CI run succeeded after a success (default: off — noise)

The minimal config to enable failure-and-recovery emails:

```fennel
{:notifications {:to ["alpha@example.com"]
                 :on [:ci-failed :ci-fixed]}}
```

Global config has the SMTP connection details and a default `:to` list that per-repo config can override.

Send failures (SMTP down, auth rejected, etc.) are logged but don't block anything else — a failed notification shouldn't fail a push or a CI run. Logged to quire's own log so there's a place to notice drift.

## Key design decisions locked in

- **Host mediates SSH; container is quire-only.** Host sshd authenticates, `ForceCommand` dispatches into the container via `docker exec`, container has no sshd. One auth layer, on the host, where the keys belong.
- **TLS and web auth on the reverse proxy.** Caddy (or equivalent) terminates TLS, handles authentication, and injects a trusted identity header. Quire reads the header and makes visibility decisions. Auth mechanism is the proxy's problem; quire stays scheme-agnostic.
- **Mirroring is server-side, not a CI job.** On every push event, `quire serve` pushes each updated ref (non-force) to the remotes in the repo's `:mirrors` map, authenticating with tokens from the global `:secrets`. Independent of CI; mirror failures are logged, they don't fail the push — `ssh git@host quire mirror push <repo> <ref>` re-triggers a ref by hand after a mirror-side failure. (An earlier design routed mirroring through a CI job; [`plans/2026-08-12-ci-rearchitecture.md`](./plans/2026-08-12-ci-rearchitecture.md) removed it as a duplicate path.)
- **Web visibility: public by default, per-repo opt-outs.** Repos are public (they go to GitHub anyway); CI logs require auth. Per-repo `(private true)` and `(public_runs true)` flags cover the exceptions.
- **Trust the proxy-injected identity header.** `Remote-User` is trusted because the reverse proxy is the only ingress. Proxy must strip any client-supplied version before injecting its own — this is the security-critical invariant.
- **Explicit repo creation, not implicit on first push.** `ssh git@host quire repo new <name>`. No magic, no shims parsing first pushes.
- **Hooks via `hook.<n>.command` config.** Git 2.54+ (the version we build into the container image). No shim scripts on disk; `hook.<n>.command = /usr/local/bin/quire hook <n>`. Set at creation time.
- **Post-receive hook sends push events over Unix socket.** The post-receive hook sends a JSON push event over a Unix domain socket (`/var/quire/server.sock`) to `quire serve`. The server dispatches CI triggers and mirror pushes. The hook exits fast. When the server isn't running, the hook prints a warning and exits cleanly.
- **No reverse-direction mirroring.** quire is the source of truth; GitHub is the replica.
- **CI pipelines are Fennel code, not data tables.** The whole point is real code. Shared steps can be factored into `.quire/lib/*.fnl` and `require`'d.
- **One level of repo grouping max.** `foo.git` and `work/foo.git` are fine. `a/b/c.git` is rejected.
- **Read-only web UI.** No write operations from the browser, ever.
- **`quire exec` is the only SSH-originated entry point.** The command string is parsed shell-style (a real parser, not regex) and validated against a strict allowlist: `git-receive-pack`, `git-upload-pack`, `git-upload-archive`, plus the `quire repo` subcommands and `quire mirror push`. Reject by default, explicit enumeration, tests for the rejection paths as well as the accept paths. No sshd in the container means no fallback if the parser is too loose — the allowlist is the actual security boundary, not a UX convenience.

## Open questions

- **CI network policy.** Default on (you'll want it for `cargo`, `npm`), with a per-pipeline `(network false)` opt-out. Or default off with explicit `(network true)`? Default on is more ergonomic; default off is more principled. Becomes real when CI runs in containers.
- **Artifact size limits.** Probably want a per-run cap (1 GB?) and a per-repo cap (10 GB?). Values TBD after real use.
- **Push-time feedback for CI.** When post-receive kicks off CI, should the push block until the run starts (not completes)? Probably yes, so the client sees "CI run #42 queued" in push output.
- **Backup story.** `tar` the data volume. Secrets referenced from the volume travel with the backup — convenient but also means the backup is sensitive. Worth thinking about encryption-at-rest for the backup, not just the source volume. Defer, but don't forget.
- **`docker exec` performance.** Each git push spawns a new `docker exec`. Container startup is not involved (the container is already running), but there's still some latency — tens to hundreds of milliseconds. Probably fine for interactive use, possibly noticeable if something scripts many pushes. Measure, don't optimize preemptively.
- **Reverse-proxy auth scheme.** Which auth mechanism does the proxy actually run? Candidates:
  - HTTP basic — simplest, but the login UI is the browser's ugly default dialog.
  - Caddy's built-in `basic_auth` — same UI, slightly cleaner config.
  - `forward_auth` to a small SSO service like Authelia or oauth2-proxy — proper login page, more moving parts.
  - GitHub OAuth via oauth2-proxy — nice "sign in with GitHub" flow, ties identity to something real.

  Leaning basic auth for v1 — it's ugly but trivial, and "my password is a 40-character string in 1Password" is fine at single-user scale. Can swap to OAuth later without changing quire at all.
- **SMTP credentials** (when notifications land). Global config holds SMTP user + password. Storing in `config.fnl` plain-text is fine for a personal instance where the volume is trusted, but worth noting: anyone who reads the volume can read the password. Alternatives: env var (fine, same trust boundary), file outside the volume that the container reads on startup (marginal), actually encrypt (overkill for this). Lean: plain in `config.fnl`, document the trust assumption.
- **Notification deduplication** (when notifications land). If CI is flaky and the same build fails twice in a row, that's two emails. If it fails ten times, that's ten. Probably fine at personal scale (flaky CI is itself a problem worth noticing), but if it becomes annoying, add simple per-event throttling ("don't send the same event for this repo more than once per N minutes"). Defer; fix if it's actually a nuisance.

## Post-baseline wishlist

Things to build after v1 is stable.

### Richer line/file history view

Tracing "where did this code come from, where did it go" is the thing every forge does poorly and every developer wants. The baseline gets us `git log --follow` and basic blame. This is about going materially beyond that:

- **Blame ladder.** Start on blame for a file at HEAD. Click any line → jump to the commit that last touched it and show blame at that commit's parent. Keep climbing. Turns blame from a point-in-time snapshot into navigable history. The UI affordance is "click to ascend" — like `tig`'s blame navigation but in a browser.
- **Range follow.** Select contiguous lines in the file view, get `git log -L` for that range, rendered as commits with diffs scoped to those lines. Much more useful than full-file history when you're asking about a specific function.
- **Rename trail.** For a file, surface its full `(sha, path)` history as a list, so you can see the file's whole identity arc at once instead of reconstructing it from scattered commits.
- **Cross-file code trails** (aspirational). When a block of code moves between files — extraction into a module, split of a big file — follow it. Hard and heuristic-dependent; no forge does it well. Worth trying `git log --find-copies-harder` as a starting point, maybe with Myers-diff-based block matching on top. If this proves tractable it's the feature that makes quire's web UI distinctive.

None of these should require a database. All are expressible as git subprocess invocations with careful caching of the results.

## Out of scope, explicitly

- Issues, PRs, code review UI
- Multi-user anything
- Web-based repo creation or deletion
- Branch protection, required reviews, merge queues
- Webhooks out (but see the planned email notifications above)
- Pulling from external sources (quire is push-only from the operator's side)
- LFS
- Wiki, pages, packages

## Naming vocabulary (optional, to pepper through UI copy if it doesn't feel forced)

- A **quire** is a repo (bookbinding: a gathering of folded leaves).
- A **scribe** is the CI worker.
- **Marks** could be refs/tags, but this one's a stretch — probably just call them refs.
- **Leaves** for files is too cute. Files are files.
