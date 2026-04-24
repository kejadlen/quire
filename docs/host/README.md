# Host setup for quire

Reference configs for dispatching SSH connections into the quire container.

## Files

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

4. Start the quire container:

       docker run -d --name quire-container quire

5. Test:

       git clone git@localhost:foo.git /tmp/test-clone

## Notes

SSH dispatch is handled by `quire exec` inside the container. The sshd
ForceCommand passes `$SSH_ORIGINAL_COMMAND` directly to the binary,
which validates the git command against an allowlist (git-receive-pack,
git-upload-pack, git-upload-archive) and sanitizes the repository path
before exec'ing the git subprocess.
