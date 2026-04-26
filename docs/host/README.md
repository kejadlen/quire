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
           -e HOME=/tmp \
           -v /var/quire:/var/quire \
           -p 127.0.0.1:3000:3000 \
           quire

   In a compose file, the equivalent is `user: "${QUIRE_UID}:${QUIRE_GID}"`
   with the values templated from `id -u git` / `id -g git` during host
   setup.

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
