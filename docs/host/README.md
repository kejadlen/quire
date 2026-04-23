# Host setup for quire

Reference configs for dispatching SSH connections into the quire container.

## Files

- `sshd_config` — drop into `/etc/ssh/sshd_config.d/` on the host
- `quire-dispatch` — copy to `/usr/local/bin/quire-dispatch` and `chmod +x`

## Setup

1. Create the git user on the host:

       sudo useradd --system --create-home git

2. Add your public key:

       sudo mkdir -p /home/git/.ssh
       sudo cp ~/.ssh/id_ed25519.pub /home/git/.ssh/authorized_keys
       sudo chown -R git:git /home/git/.ssh
       sudo chmod 700 /home/git/.ssh
       sudo chmod 600 /home/git/.ssh/authorized_keys

3. Install the dispatch script:

       sudo cp quire-dispatch /usr/local/bin/quire-dispatch
       sudo chmod +x /usr/local/bin/quire-dispatch

4. Install the sshd config:

       sudo cp sshd_config /etc/ssh/sshd_config.d/quire.conf
       sudo systemctl reload sshd

5. Start the quire container:

       docker run -d --name quire-container quire

6. Test:

       git clone git@localhost:foo.git /tmp/test-clone

## Notes

The dispatch script is the security boundary between the host and the container.
It only allows `git-receive-pack`, `git-upload-pack`, and `git-upload-archive`.
Repo paths are validated: no `..` traversal, must end in `.git`, no double slashes.

When `quire exec` is built (step 2), the ForceCommand will change to:

    ForceCommand docker exec -i quire-container quire exec "$SSH_ORIGINAL_COMMAND"

and this dispatch script will be replaced by the quire binary's own allowlist.
