# quire — plan

A build order, architectural notes, and a list of open questions. Living document.

## Architecture at a glance

The **host** does auth and network plumbing. The **container** is pure quire.

**Host-side:**

1. **openssh** — the host's sshd authenticates git/quire connections. A `Match User git` block uses `ForceCommand` to dispatch authenticated commands into the container via `docker exec`. One set of host keys, one `authorized_keys`, one process doing auth.
2. **Reverse proxy** — Caddy (likely). Terminates TLS, obtains certs, handles web authentication, injects an identity header (`Remote-User`, trusted because the proxy is the only ingress), and reverse-proxies to `quire serve` inside the container. The actual auth mechanism behind the proxy (OAuth, HTTP basic, an SSO layer like Authelia, whatever) is the proxy's problem; quire only sees the header.

**Container-side:**

1. **`quire` binary** — serves as both the HTTP server (`quire serve`) and the dispatch target (`quire exec <cmd>` invoked from the host's ForceCommand via `docker exec`). No sshd inside. Git hooks installed in each repo via `hook.<n>.command` config call back into the binary as `quire hook <n>`.
2. **CI runner** — separate long-running process (same binary, different subcommand: `quire ci-runner`), watches for new run directories under `runs/`, executes them. Sandboxing (via bubblewrap) is optional and deferred — single-user personal use doesn't warrant it; revisit if CI ever runs code I haven't written.
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

**Try to avoid a database entirely.** Run history lives on disk as one directory per run. Refs and repo metadata are in the git repos themselves. Per-repo config is Fennel on disk. The threshold for reaching for SQLite is "the filesystem approach is visibly causing problems" — not "I vaguely feel like querying would be nice." Likely triggers, ordered by probability:

1. CI concurrency control that outgrows a lock file.
2. Aggregate queries across repos (e.g. "all failed runs this week").
3. Full-text search over commit messages or file content.

CI is the most likely to force the issue first.

## Volume layout

One volume mounted into the container:

```
/var/quire/
  repos/
    foo.git/
      quire/
        mirror-deploy-key    SSH private key for GitHub mirror (mode 0600)
    work/
      bar.git/
        quire/...
  runs/
    <repo>/<run-id>/
      meta.fnl               status, ref, sha, pipeline source, timings
      log                    streamed stdout/stderr
      artifacts/
  config.fnl                 global config
```

Per-repo config (`mirror`, `public_runs`, etc.) is checked into the repo at `.quire/config.fnl`, not stored in the bare repo's `quire/` directory. Quire reads it from the bare repo via `git show HEAD:.quire/config.fnl`. The `quire/` directory holds only generated artifacts like the mirror deploy key.

No SSH config or host keys in this volume — those live on the host. The container image brings the `quire` binary and git; the volume brings repos, runs, and per-repo state. Bubblewrap is only needed if CI sandboxing is enabled (it isn't by default).

`docker compose down && up` loses nothing in the volume. Host identity (ssh host keys, reverse-proxy certs and state) persists on the host.

## Host configuration

The container expects a specific host setup. Ship reference configs in `docs/host/` alongside the image:

- **sshd_config snippet** — the `Match User git` block with AuthorizedKeysFile, ForceCommand, restrictions (`no-port-forwarding`, `no-agent-forwarding`, `no-X11-forwarding`, `no-pty`).
- **quire-dispatch** — the small script that ForceCommand invokes. In the simplest deployment this is a one-liner: `exec docker exec -i quire-container quire exec "$SSH_ORIGINAL_COMMAND"`. Inlining it directly into `ForceCommand` works too; a script file is only worth having if host-side logic accumulates (rate limiting, logging, per-key policy).
- **Caddyfile** — single vhost, terminates TLS, runs authentication (via `forward_auth` to an auth service, basic auth, or whatever's appropriate), strips any client-supplied `Remote-User` header, injects the proxy's own, reverse-proxies to the container's HTTP port.
- **systemd unit or compose file** — starts the container with the volume mount, publishes the HTTP port to loopback only (Caddy reverse-proxies), restarts on failure.

The host config is documented and version-controlled, not pretending to be handled by the container. This is a real cost — it's more moving parts than "one container does everything" — but it's the honest shape of the problem.

## Future: all-in-one image variant (not building)

Worth noting for completeness: nothing in the base image's design prevents a second, derivative image that layers sshd + a supervisor on top and handles the auth layer inside the container. That would be the turnkey "docker run this and you have a git server" story — useful for people deploying quire on a VPS without existing host infrastructure, or for quick evaluation.

The shape, sketched: `quire:standalone` extends `quire:latest` with:

- openssh-server.
- A supervisor (tini or s6) so sshd and `quire serve` can run together.
- An entrypoint that starts both processes.
- sshd configured with `ForceCommand /usr/local/bin/quire exec "$SSH_ORIGINAL_COMMAND"` in its sshd_config.
- Authorized keys from a volume-mounted file or env var.

Everything downstream of `quire exec` is identical to the host-mediated path — same allowlist, same dispatch logic — so there's no divergent code to maintain.

Flagging the possibility now because it costs nothing at design time (the `quire exec` dispatch boundary is already the right shape for either deployment), and it'd be a thoughtful contribution from someone who wants it later. Not building it for v1 — I don't need it, and the base image plus reference host configs cover the deployment story I actually want.

## Client assumptions

The primary client is **jj** (Jujutsu), not git directly. In practice this changes very little server-side — `jj git push` speaks the git wire protocol, so `git-receive-pack` and `git-upload-pack` handle it transparently. A few things are still worth keeping in mind because they shape UX defaults, not protocol handling:

- **Force-pushes are routine, not exceptional.** jj users rebase and amend constantly; force-pushing a bookmark is part of the normal flow. CI needs a policy for what happens when a new push supersedes an in-flight run for the same ref. Leaning: cancel the in-flight run, start the new one, log the cancellation in the run history.
- **Short-lived refs are common.** jj's push-anywhere workflows can produce refs like `push-xxxxxxxx` that exist only to move work around. The web UI shouldn't give every ref equal prominence — surface branches the operator has opinions about (main, plus anything pinned in per-repo config), fold the rest into a "see all" affordance.
- **Commit SHAs aren't stable identities.** Don't build URLs or features that assume a given SHA will exist forever. Prefer refs where possible; accept that deep-linking to a SHA may 404 after a rebase.
- **No assumption of linear history.** Even post-rebase, merge commits and non-linear shapes show up. The log view shouldn't require linearity.

Nothing here requires jj-specific code. It's all just "don't make git-flow-shaped assumptions."

## Build sequence

The build sequence is ordered by integration risk, not feature priority — the unfamiliar plumbing comes first so the rest can be built on solid ground.

### 1. Host-mediated dispatch to a pushable repo

This is the step with real integration risk — getting host sshd to dispatch authenticated connections into the container cleanly, and making sure stdio is preserved end-to-end. Do it before anything else.

Minimal Dockerfile: `quire` user, git installed, a bare repo pre-created at `/var/quire/repos/foo.git`. No sshd in the container, no quire binary yet. `ENTRYPOINT` is a shell that handles `docker exec` invocations.

On the host: create a `git` user, put your pubkey in its `~/.ssh/authorized_keys`, add the `Match User git` block with `ForceCommand /usr/local/bin/quire-dispatch`, write the quire-dispatch script (parses `$SSH_ORIGINAL_COMMAND`, execs `docker exec -i quire-container /bin/sh -c "cd /var/quire/repos/$REPO && git-receive-pack ."` or similar).

Verify with `git push git@host:foo main`. Push a commit, confirm it lands in the bare repo.

Things most likely to go wrong here:

- Stdio buffering between ssh → docker exec → git-receive-pack.
- Argument quoting through three layers of shell.
- `docker exec -i` vs `-it` (no TTY when invoked from ForceCommand).

### 2. `quire exec` dispatch subcommand

Replace the ad-hoc shell dispatch from step 1 with `docker exec -i quire-container quire exec "$SSH_ORIGINAL_COMMAND"`. The `quire exec` subcommand takes the original command string, parses it properly (shell-style with a real parser, not regex), validates it against a strict allowlist — `git-receive-pack`, `git-upload-pack`, `git-upload-archive`, and a specific set of `quire` subcommands (`new`, `list`, `rm`, `mirror *`) — and execs the appropriate binary.

**This is the only dispatch surface into the container.** There's no sshd in the container to backstop a permissive parser; anything that gets past `quire exec` runs as trusted. The allowlist is the security boundary — not a UX convenience, the actual boundary. Treat it that way: explicit enumeration, reject by default, no regex-based "looks safe enough," tests for the rejection paths as well as the accept paths.

### 3. Hook plumbing

Write the quire binary's `hook` subcommand as a no-op that logs what it was invoked with. Install hooks into the test repo via `git config hook.pre-receive.command "quire hook pre-receive"` etc. Push to the repo, see the log lines. Proves the hook path works.

### 4. Explicit repo creation

`ssh git@host quire new <name>` → `quire exec` → `quire new <name>`. Creates a bare repo under `repos/`, validates the name (no `..`, one level of grouping max, no reserved names), sets `hook.<name>.command` configs. Also: `quire list`, `quire rm`, basic ops. All accessed via the same ssh-dispatch path.

### 5. GitHub mirror via post-receive (deploy key)

Per-repo config checked into the repo at `.quire/config.fnl` with a `mirror` key (GitHub remote URL); quire reads it from the bare repo via `git show HEAD:.quire/config.fnl`. A matching private key at `<repo>.git/quire/mirror-deploy-key` (generated by `quire mirror add <remote-url>`, which also prints the public key for the user to paste into GitHub's deploy-keys UI).

Post-receive hook sends a JSON push event over `/var/quire/server.sock` to `quire serve`. The server listener parses the event, looks up the repo's mirror config, resolves the GitHub token from global config, and runs `git push` in a spawned blocking task. Mirror failures surface in the server's logs. If `quire serve` isn't running, the hook prints a warning to stderr and exits cleanly (no push is created).

Pre-receive: if a mirror is configured, test-run a low-cost git operation against the remote (probably `git ls-remote`) to verify the deploy key still works. If it fails, reject with a clear message. Per-repo override (`accept_without_mirror = true`) for the rare case where you want to push without syncing.

### 6. Web view, minimum viable

`quire serve` starts an HTTP server bound to a container-internal port (published to the host on loopback only). Caddy on the host terminates TLS, handles auth, and reverse-proxies. Repo list, repo home (README + recent commits + refs), tree browser, file view with syntax highlighting, commit view with diff. No JS required. Reads repos from disk on each request (no caching yet).

Quire reads the `Remote-User` header (injected by Caddy). If present, the request is "authenticated" and the full UI is visible. If absent, only public paths serve content: public repos show, private repos 404, `/runs/*` always 404. The policy lives in quire — Caddy's job is just to handle the auth handshake and inject the header correctly. Belt-and-suspenders: if Caddy is misconfigured and fails to strip a client-supplied header, quire has no way to detect that. Document the header-stripping requirement loudly in the reference Caddyfile.

### 7. Web view, nicer

Per-file history following renames (`git log --follow`), compare-between-refs, blame, submodule-aware tree browsing. Skip branch-graph viz. Measure before caching anything — likely candidates if it's needed are rendered READMEs and syntax-highlighted blobs.

### 8. Fennel CI MVP

Embed Lua via `mlua`, ship Fennel compiler as a Lua module. Define a small standard library (`pipeline`, `on`, `step`, `sh`, `artifact`, `cache`, `matrix`, `env`) as a Fennel module. Compile-and-eval `.quire/ci.fnl` at run-trigger time. Steps run directly as subprocesses; per-run tempdir; network on by default.

Sandboxing is deliberately not in the MVP. Since every pipeline is code I'm writing for my own projects, "the CI step can do anything a logged-in me can do in the container" is the right threat model. If that changes (running untrusted forks, for example, or sharing the instance), re-introduce bubblewrap wrapping behind a per-pipeline `(sandbox true)` opt-in.

Post-receive hook materializes a new run directory under `runs/<repo>/<id>/` with `meta.fnl` in a `queued` state. The CI runner process picks it up and executes.

### 9. Run history + artifacts

One directory per run, `meta.fnl` storing status, ref, sha, pipeline source, timings. Artifact retention policy: last 10 runs per repo, or 30 days, whichever is longer. Web UI for run list and run detail with streaming log. Run list reads directory entries and parses meta files — fine at single-user scale. Re-evaluate if cross-repo aggregate queries become something I want, or if CI concurrency needs a real queue.

### 10. Email notifications

Shell out to `msmtp` (or `sendmail`-compatible) as a subprocess — the container ships `msmtp`, and global config (`config.fnl`) specifies SMTP server + credentials once. Quire builds the message, pipes it to `msmtp -t`, done. No native SMTP library, no retry queue, no HTML templates; a plain-text email with subject and body is the whole thing.

What triggers a notification, per-repo-configurable in `.quire/config.fnl`:

- CI run failed (default: on, if any address is configured)
- CI run that was previously failing now succeeds (default: on — the "fixed" notification is the one you actually want)
- CI run succeeded after a success (default: off — noise)
- Mirror push to GitHub failed (default: on — silent mirror failure is exactly the drift we don't want)

The minimal config to enable failure-and-recovery emails:

```fennel
(notifications
  :to ["alpha@example.com"]
  :on [:ci-failed :ci-fixed :mirror-failed])
```

Global config has the SMTP connection details and a default `:to` list that per-repo config can override.

Send failures (SMTP down, auth rejected, etc.) are logged but don't block anything else — a failed notification shouldn't fail a push or a CI run. Logged to quire's own log so there's a place to notice drift.

### 11. Polish

Keyboard navigation in the web UI. Atom feeds for recent commits (public, subject to per-repo visibility) and CI runs (auth-gated, same as the log views). `quire` CLI rounded out (rotate mirror keys, prune runs, re-run a CI job, rotate deploy keys).

## Key design decisions locked in

- **Host mediates SSH; container is quire-only.** Host sshd authenticates, `ForceCommand` dispatches into the container via `docker exec`, container has no sshd. One auth layer, on the host, where the keys belong.
- **TLS and web auth on the reverse proxy.** Caddy (or equivalent) terminates TLS, handles authentication, and injects a trusted identity header. Quire reads the header and makes visibility decisions. Auth mechanism is the proxy's problem; quire stays scheme-agnostic.
- **Mirror to GitHub via per-repo deploy key.** Stored at `<repo>.git/quire/mirror-deploy-key`. Post-receive uses `GIT_SSH_COMMAND` with `-i`. No agent forwarding across the host/container boundary, no fragile socket plumbing. Generated by `quire mirror add`, public half printed for the user to paste into GitHub.
- **Web visibility: public by default, per-repo opt-outs.** Repos are public (they go to GitHub anyway); CI logs require auth. Per-repo `(private true)` and `(public_runs true)` flags cover the exceptions.
- **Trust the proxy-injected identity header.** `Remote-User` is trusted because the reverse proxy is the only ingress. Proxy must strip any client-supplied version before injecting its own — this is the security-critical invariant.
- **Explicit repo creation, not implicit on first push.** `ssh git@host quire new <n>`. No magic, no shims parsing first pushes.
- **Hooks via `hook.<n>.command` config.** Git 2.54+ (the version we build into the container image). No shim scripts on disk; `hook.<n>.command = /usr/local/bin/quire hook <n>`. Set at creation time.
- **Mirror push runs inside `quire serve` via event socket.** The post-receive hook sends a JSON push event over a Unix domain socket (`/var/quire/server.sock`) to `quire serve`, which looks up the repo's mirror config and runs the push in-process. This trades synchronous push-for-push blocking for architectural cleanliness: the hook exits fast, and mirror failures surface in the server's logs, not the pusher's terminal. When the server isn't running, the hook prints a warning and exits cleanly.
- **No reverse-direction mirroring.** quire is the source of truth; GitHub is the replica.
- **CI pipelines are Fennel macros, not data tables.** The whole point is real code. Shared steps can be factored into `.quire/lib/*.fnl` and `require`'d.
- **One level of repo grouping max.** `foo.git` and `work/foo.git` are fine. `a/b/c.git` is rejected.
- **Read-only web UI.** No write operations from the browser, ever.
- **One container, multiple processes inside.** `quire serve` and the CI runner are long-running; `quire exec` and `quire hook *` are short-lived subprocess invocations. Supervised via a minimal init (tini + a simple supervisor, or s6-overlay).
- **`quire exec` is the only SSH-originated entry point.** Strict allowlist, explicit rejection, security-sensitive parser. No sshd in the container means no fallback if the parser is too loose.

## Open questions

- **Inline `ForceCommand` vs. quire-dispatch script.** Simplest is inlining: `ForceCommand docker exec -i quire-container quire exec "$SSH_ORIGINAL_COMMAND"`. No script file, no intermediate layer. The reason to add a `quire-dispatch` script would be host-side logic that needs to run before dispatch (rate limiting, per-key policy, audit logging). Lean: inline, add a script only when a need appears.
- **Deploy key rotation.** Per-repo keys mean per-repo rotation. `quire mirror rotate <repo>` generates a new key, prints the new pubkey. The annoying part is that the *old* pubkey is still on GitHub until you remove it. Flow: print new pubkey, wait for user to confirm they've added it on GitHub, then switch the config to use the new key, then print the old pubkey and tell them to remove it. Four steps, all mediated by quire CLI. Defer the ergonomics but note that rotation must not silently leave the old key authorized.
- **Host config bundle.** The reference sshd_config block, Caddyfile, and docker-compose file should be in the quire repo itself, versioned with the code. Ideally a `quire install-host-config` command that writes them interactively. Or just a `docs/host/` directory with copy-paste instructions. Lean toward the latter — interactive installers that touch host config are scope creep.
- **Public SSH port.** Host's sshd runs on 22. No conflict now — one sshd on the host does everything. Stay on 22.
- **CI network policy.** Default on (you'll want it for `cargo`, `npm`), with a per-pipeline `(network false)` opt-out. Or default off with explicit `(network true)`? Default on is more ergonomic; default off is more principled.
- **Artifact size limits.** Probably want a per-run cap (1 GB?) and a per-repo cap (10 GB?). Values TBD after real use.
- **Push-time feedback for CI.** When post-receive kicks off CI, should the push block until the run starts (not completes)? Probably yes, so the client sees "CI run #42 queued" in push output.
- **Secrets for CI.** Injected from container env or loaded from a file on the volume. Since CI is unsandboxed, there's no meaningful isolation story anyway — any pipeline step has full access to whatever the CI runner has. Env is simplest; punt encrypted-at-rest to "if I ever want it."
- **Backup story.** `tar` the data volume. Deploy keys are in the volume, so they travel with the backup — convenient but also means the backup is sensitive. Worth thinking about encryption-at-rest for the backup, not just the source volume. Defer, but don't forget.
- **`docker exec` performance.** Each git push spawns a new `docker exec`. Container startup is not involved (the container is already running), but there's still some latency — tens to hundreds of milliseconds. Probably fine for interactive use, possibly noticeable if something scripts many pushes. Measure, don't optimize preemptively.
- **Reverse-proxy auth scheme.** Which auth mechanism does the proxy actually run? Candidates:
  - HTTP basic — simplest, but the login UI is the browser's ugly default dialog.
  - Caddy's built-in `basic_auth` — same UI, slightly cleaner config.
  - `forward_auth` to a small SSO service like Authelia or oauth2-proxy — proper login page, more moving parts.
  - GitHub OAuth via oauth2-proxy — nice "sign in with GitHub" flow, ties identity to something real.

  Leaning basic auth for v1 — it's ugly but trivial, and "my password is a 40-character string in 1Password" is fine at single-user scale. Can swap to OAuth later without changing quire at all.
- **Identity header name.** Various proxies use various names (`Remote-User`, `X-Remote-User`, `X-Forwarded-User`, `X-Auth-User`). Quire should pick one and the reference Caddyfile should match. `Remote-User` is short and traditional; `X-Remote-User` signals "this is a custom header, not the standard CGI one." Lean: `Remote-User`, matches the CGI convention and nginx/apache ecosystem.
- **SMTP credentials.** Global config holds SMTP user + password. Storing in `config.fnl` plain-text is fine for a personal instance where the volume is trusted, but worth noting: anyone who reads the volume can read the password. Alternatives: env var (fine, same trust boundary), file outside the volume that the container reads on startup (marginal), actually encrypt (overkill for this). Lean: plain in `config.fnl`, document the trust assumption.
- **Notification deduplication.** If CI is flaky and the same build fails twice in a row, that's two emails. If it fails ten times, that's ten. Probably fine at personal scale (flaky CI is itself a problem worth noticing), but if it becomes annoying, add simple per-event throttling ("don't send the same event for this repo more than once per N minutes"). Defer; fix if it's actually a nuisance.

## Post-baseline wishlist

Things to build after v1 is stable.

### Richer line/file history view

Tracing "where did this code come from, where did it go" is the thing every forge does poorly and every developer wants. The baseline plan (step 7) gets us `git log --follow` and basic blame. This is about going materially beyond that:

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
- Webhooks out (but see email notifications in the build sequence)
- Pulling from external sources (quire is push-only from the operator's side)
- LFS
- Wiki, pages, packages

## Naming vocabulary (optional, to pepper through UI copy if it doesn't feel forced)

- A **quire** is a repo (bookbinding: a gathering of folded leaves).
- A **scribe** is the CI worker.
- **Marks** could be refs/tags, but this one's a stretch — probably just call them refs.
- **Leaves** for files is too cute. Files are files.
