# Git build stage.
#
# Debian trixie ships git 2.47, but we need 2.54+ for hook.<name>.command
# config support. This lets quire register hooks via git config instead of
# writing shim scripts to disk — the hook dispatches directly into the
# quire binary as `quire hook <name>`.
ARG GIT_VERSION=2.54.0
FROM debian:trixie-slim AS git-builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        gcc \
        gettext \
        libcurl4-openssl-dev \
        libexpat1-dev \
        libssl-dev \
        libz-dev \
        make \
    && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL https://github.com/git/git/archive/refs/tags/v${GIT_VERSION}.tar.gz \
    | tar xz \
    && cd git-${GIT_VERSION} \
    && make -j$(nproc) prefix=/usr/local NO_TCLTK=1 NO_GETTEXT= \
    && make prefix=/usr/local install

# Quire build stage.
FROM rust:1.88-trixie AS builder

WORKDIR /usr/src/quire
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/quire/target \
    cargo install --path .

# Runtime stage.
FROM debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libcurl4 \
        libexpat1 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=git-builder /usr/local/bin/git /usr/local/bin/git
COPY --from=git-builder /usr/local/libexec/git-core/ /usr/local/libexec/git-core/
COPY --from=builder /usr/local/cargo/bin/quire /usr/local/bin/quire

# Configure git hooks globally so all repos inherit the post-receive dispatch.
RUN git config --system hook.postReceive.command "quire hook post-receive"

# Volume layout per PLAN.md. Ownership is set on the host; the container
# runs as the host uid/gid passed via `docker exec --user`, so no user
# is created in the image.
RUN mkdir -p /var/quire/repos /var/quire/runs

WORKDIR /var/quire

ENTRYPOINT ["quire"]
CMD ["serve"]
