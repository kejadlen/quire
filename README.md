# quire

A personal source forge. Single-user, self-hosted, minimal.

Named after the old bookbinding term: a gathering of folded leaves, sewn together. Your repos are quires; the whole thing is a quire of quires.

## What it is

A small Rust binary that runs in a Docker container, fronted by the host's sshd and a TLS-terminating reverse proxy. It gives you:

- **Git hosting over SSH**, via the host's sshd dispatching into the container. Explicit repo creation (`ssh git@host quire new <n>`).
- **A read-only web view** for browsing README, tree, history, blame, diffs, and refs.
- **Automatic mirroring to GitHub** on push, when configured per-repo. Uses a per-repo deploy key rather than forwarded agent — simpler, more robust across the host/container boundary.
- **Fennel-based CI**, with pipelines defined in `.quire/ci.fnl`. Unsandboxed by default since it's all my code; bubblewrap wrapping available behind an opt-in if that changes.
- **Email notifications** for CI failures, recoveries, and mirror-push failures. SMTP via `msmtp`; plain text; per-repo config for what to send and to whom.

No issues, no PRs, no user management, no webhooks. Use the GitHub mirror for the social stuff; quire is your forge.

Post-v1, the feature I most want to build is a richer line/file history view — blame ladder, range-follow, rename trails — the thing every forge does poorly.

## Design principles

- **The container is pure quire.** SSH auth and TLS/web auth both live on the host (host sshd, reverse proxy). The container runs `quire` and the minimal things it needs (git, msmtp). One job per surface.
- **Don't own ssh.** The host's sshd handles auth, channels, and key management; `ForceCommand` dispatches authenticated invocations into the container via `docker exec`. Quire's integration point is git hooks and the `quire exec` dispatch target.
- **Web auth at the reverse proxy.** The proxy (Caddy or equivalent) handles authentication and injects a trusted identity header. Quire reads the header and applies per-repo visibility: public repos are world-readable, private repos and CI logs require auth. Any auth mechanism the proxy supports (basic, OAuth, SSO) Just Works — quire stays scheme-agnostic.
- **Git's filesystem is the source of truth.** Bare repos under `/var/quire/repos/` are the primary artifact. CI run history is directories on disk, not a database. A database comes back only if the filesystem approach visibly fails.
- **Built for jj.** The primary client is Jujutsu, which means routine force-pushes, short-lived refs, and unstable SHAs. No git-flow-shaped assumptions in the UI or CI.
- **Push should fail fast, loudly, and correctly.** No silent drift between quire and GitHub. No accepted-but-unreplicated state.
- **Config is code.** Global config and per-repo config are Fennel. CI pipelines are Fennel. If you're going to have a scripting language, have one.

## Layout

```
/var/quire/
  repos/           bare git repos; each has a .git/quire/ dir with config + mirror deploy key
  runs/            CI run metadata, artifacts, and logs; retention-policied
  config.fnl       global config
```

Host-side config (sshd_config block, Caddyfile, docker-compose file) lives on the host, version-controlled separately. See `PLAN.md` for the reference layout.

## Status

Design phase. See `PLAN.md` for the build sequence and open questions.
