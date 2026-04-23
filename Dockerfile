FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        git \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --system quire \
    && useradd --system --gid quire --create-home quire

# Volume layout per PLAN.md.
RUN mkdir -p /var/quire/repos /var/quire/runs \
    && chown -R quire:quire /var/quire

# Pre-create a test repo for step 1 verification.
RUN git init --bare /var/quire/repos/foo.git \
    && chown -R quire:quire /var/quire/repos/foo.git

COPY <<'EOF' /usr/local/bin/entrypoint
#!/usr/bin/env bash
set -euo pipefail

exec "$@"
EOF
RUN chmod +x /usr/local/bin/entrypoint

USER quire
WORKDIR /var/quire

ENTRYPOINT ["/usr/local/bin/entrypoint"]
CMD ["sleep", "infinity"]
