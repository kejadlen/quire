# Host setup for quire

Reference configs for dispatching SSH connections into the quire container.

## Files

- `README.md` — this file
- `sshd_config` — drop into `/etc/ssh/sshd_config.d/` on the host

## Setup

1. Create the git user on the host:

       sudo useradd --system --create-home git

2. Add your public key:

       sudo mkdir -p /home/git/.ssh
       sudo cp ~/.ssh/id_ed25519.pub /home/git/.ssh/authorized_keys
       sudo chown -R git:git /home/git/.ssh
       sudo chmod 700 /home/git/.ssh
       sudo chmod 600 /home/git/.ssh/authorized_keys

3. Install the sshd config:

       sudo cp sshd_config /etc/ssh/sshd_config.d/quire.conf
       sudo systemctl reload sshd

4. Start the quire container, running as the host's `git` user so file
   ownership on the bind-mounted `/var/quire` matches inside and out:

       docker run -d --name quire-container \
           --user "$(id -u git):$(id -g git)" \
           --group-add "$(getent group docker | cut -d: -f3)" \
           -e HOME=/tmp \
           -v /var/quire:/var/quire \
           -v /var/run/docker.sock:/var/run/docker.sock \
           -p 127.0.0.1:3000:3000 \
           quire

   In a compose file, the equivalent is `user: "${QUIRE_UID}:${QUIRE_GID}"`
   with the values templated from `id -u git` / `id -g git` during host
   setup, plus `group_add: ["${DOCKER_GID}"]` and the two bind mounts
   under `volumes:`.

   If you want the interim gitweb view, run it as a separate container
   mounting the same volume (read-only is sufficient):

       docker run -d --name quire-gitweb \
           -v /var/quire/repos:/var/quire/repos:ro \
           -p 127.0.0.1:8080:8080 \
           quire-gitweb

5. Create a test repo inside the container:

       docker exec quire-container quire repo new foo.git

6. Test the dispatch path:

       git clone git@localhost:foo.git /tmp/test-clone

## Notes

SSH dispatch is handled by `quire exec` inside the container. The sshd
ForceCommand passes `$SSH_ORIGINAL_COMMAND` directly to the binary,
which validates the git command against an allowlist (git-receive-pack,
git-upload-pack, git-upload-archive) and sanitizes the repository path
before exec'ing the git subprocess.

The container image doesn't bake in a `quire` user — it runs as whatever
uid/gid the host passes via `--user`. This avoids "dubious ownership"
errors from git when the bind-mounted repo dir's host uid wouldn't
otherwise match a container user. `HOME=/tmp` is set because the host
uid has no `/etc/passwd` entry inside the container, and git needs a
writable `HOME` for its config probing.

The HTTP port (3000) is published to host loopback only. A reverse proxy
on the host terminates TLS and reverse-proxies to it; nothing else
should reach it directly.

## CI: docker-out-of-docker

The CI runner shells out to docker against the **host** daemon —
`docker run` to start a per-run container with the pipeline's image,
`docker exec` for each `(sh ...)` call, `docker stop` at the end.
Architecture and trade-offs in [docs/CI.md](../CI.md).

For this to work the quire container needs:

- The docker CLI (baked into the image; the host daemon does the work).
- The host's `/var/run/docker.sock` bind-mounted in.
- Membership in the host's `docker` group (via `--group-add`) so the
  unprivileged `git` uid can talk to the socket.

Anyone with the docker socket has root on the host. That's an
acceptable trade here because quire and the operator account already
share the box; if that ever stops being true, switch to the OCI+bwrap
path described in CI.md.

One sharp edge: when quire issues `docker run -v /var/quire/runs/...:/work`,
the host path is resolved by the host daemon, not interpreted from
inside the container. So `/var/quire` must resolve to the **same path**
on the host and inside the quire container. The bind mount above
(`-v /var/quire:/var/quire`) already enforces this; just don't get
clever and remap the path on either side.
