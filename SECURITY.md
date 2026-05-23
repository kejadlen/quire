# Security

## Scope

quire is a single-user personal forge. The operator wrote everything it runs — repos, CI pipelines, config. The security model is built around one question: *can an unauthorized party reach something they shouldn't?*

## Threat model

**Who the attacker is.** Someone who is not the operator: an unauthenticated network user, or someone who has obtained a valid SSH key. The attacker does not have shell on the host and has not compromised the reverse proxy.

**What they control.** Network access to the SSH port and/or the web UI. They may control the content of a git push if they somehow hold a valid key.

**What quire is protecting.** Private repo content and CI logs from unauthenticated web access; the container filesystem from SSH-originated command injection; secret values from appearing in stored CI output.

**Out of scope.** An attacker with shell on the host already controls the repos, the container, and `authorized_keys`. quire cannot defend against a compromised host and does not try to.

## Security-critical invariants

These are the things quire is actively defending. A bug here is a real vulnerability.

**`quire exec` allowlist.** The only SSH-originated entry point into the container is `quire exec`, invoked via `docker exec` from the host's `ForceCommand`. It accepts exactly three git commands (`git-receive-pack`, `git-upload-pack`, `git-upload-archive`) and the `quire repo` subcommands; `quire repo` performs its own subcommand validation. Everything else is rejected. A bypass that allows running an unlisted command is remote code execution.

**Repo name validation.** Repository names are validated before any filesystem operation: no `..` components, no `//`, `.git` suffix required, at most one level of grouping, no reserved path components. A validation bypass is path traversal into the host volume.

**Web auth: protected content requires `Remote-User`.** The reverse proxy is the only web ingress; it authenticates the user and injects the `Remote-User` header before forwarding. quire trusts this header because the proxy strips any client-supplied copy — that stripping is load-bearing. Bugs that cause quire to serve CI logs or private repo content to requests without the header are real vulnerabilities.

**Secret redaction in CI output.** Secret values resolved during a CI run must not appear unredacted in stored log files or web-visible output. A bug that lets a revealed secret reach a CRI log file or a web response is a real vulnerability.

## Not security issues

**Unsandboxed CI.** Pipelines run directly in the container without bubblewrap sandboxing by default. This is intentional: every pipeline is code the operator wrote for their own projects. An attacker who could push a malicious `.quire/ci.fnl` would need a valid SSH key — that is, they would be the operator. The bubblewrap opt-in exists for the day CI runs code you haven't written; use it if that day comes.

**Plaintext secrets in `config.fnl`.** The global config holds SMTP credentials and CI secrets in plain text. The volume they live on is a trusted artifact on a trusted host; anyone who can read the volume already controls the repos and the host keys. Volume-at-rest encryption is a deployment concern, not something quire handles.

**No authentication inside the container.** The container's HTTP port is published to host loopback only; authentication lives at the reverse proxy. Someone who bypasses the proxy to reach the container directly has already compromised the host and is outside the threat model.

**Force-pushes overwriting history.** quire is built for Jujutsu workflows where force-pushes are routine. An authorized key-holder rewriting branch history is not a security event.

**CI jobs have outbound network access by default.** Build jobs need to fetch dependencies. An attacker who can run CI jobs already has SSH access (see unsandboxed CI above).

**Secrets shorter than 8 characters are not redacted from CI output.** The false-positive rate for very short strings is too high to redact them automatically; a warning is emitted when this applies. Use secrets of at least 8 characters for anything that matters.

**Documentation not matching behavior, unless the documented behavior is a security guarantee.** "The docs say X, the code does Y" is a quality bug unless the mismatch undermines a security guarantee — a handler documented as requiring authentication that actually doesn't, or a validation rule documented as rejecting `..` that accepts it. Report those. A function documented as returning null on error that throws instead belongs in the issue tracker, not here.

## Standalone image variant

`docs/PLAN.md` sketches a future `quire:standalone` image that layers sshd on top of the base image so the full stack runs in a single container. **This image does not yet exist.** When it does, the auth model differs: sshd runs inside the container rather than on the host, and the `quire exec` allowlist becomes the boundary against in-container users rather than against the host `ForceCommand`. The security notes for that image will accompany it.

## Reporting

To report a bug that breaks one of the invariants above, use [GitHub's private vulnerability reporting](https://github.com/kejadlen/quire/security/advisories/new) for this repository.

This is a single-maintainer personal project. No CVE process, no bounty, no guaranteed response SLA — but real issues will be fixed.
