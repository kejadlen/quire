# Git build stage.
#
# Debian trixie ships git 2.47, but we need 2.54+ for hook.<name>.command
# config support. This lets quire register hooks via git config instead of
# writing shim scripts to disk — the hook dispatches directly into the
# quire binary as `quire hook <name>`.
ARG GIT_VERSION=2.54.0
FROM debian:trixie-slim AS git-builder
ARG GIT_VERSION

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
        perl \
    && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL https://github.com/git/git/archive/refs/tags/v${GIT_VERSION}.tar.gz \
    | tar xz \
    && cd git-${GIT_VERSION} \
    && make -j$(nproc) prefix=/usr/local NO_TCLTK=1 NO_GETTEXT= \
    && make prefix=/usr/local install

# Cargo-chef stage for dependency caching.
FROM rust:1.88-trixie AS chef
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    cargo install --locked cargo-chef
WORKDIR /build

# Plan stage: inspect source and produce a recipe of dependencies.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Build stage: cook dependencies first (cached across rebuilds), then build
# the binary. Changes to source code do not retrigger dependency compilation.
FROM chef AS builder
ARG QUIRE_VERSION
ENV QUIRE_VERSION=${QUIRE_VERSION}
# `quire-ci` is built static against musl so it can be `docker cp`'d
# into arbitrary pipeline images regardless of their libc. Use the
# rustup-bundled `rust-lld` to link, so we don't need musl-tools or a
# multilib-aware host `cc` in the build image.
RUN rustup target add x86_64-unknown-linux-musl
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
COPY --from=planner /build/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/build/target \
    cargo chef cook --release --recipe-path recipe.json
COPY . .
# Copy the binaries out of the cache mount so they survive into the runtime
# stage. Stash under /build/bin/ so they don't collide with the workspace
# member directories at /build/quire-ci, /build/quire-server.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin quire && \
    cargo build --release --bin quire-ci --target x86_64-unknown-linux-musl && \
    mkdir -p /build/bin && \
    cp target/release/quire /build/bin/quire && \
    cp target/x86_64-unknown-linux-musl/release/quire-ci /build/bin/quire-ci

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
COPY --from=builder /build/bin/quire /usr/local/bin/quire
COPY --from=builder /build/bin/quire-ci /usr/local/bin/quire-ci
# CI shells out to docker against the host daemon (DooD); see docs/CI.md.
COPY --from=docker:cli /usr/local/bin/docker /usr/local/bin/docker

# Configure git hooks globally so all repos inherit the post-receive dispatch.
# `hook.<label>` is an arbitrary identifier; the hook is bound to an event
# via `event` and run via `command`.
RUN git config --system hook.quire.event "post-receive" \
 && git config --system hook.quire.command "quire hook post-receive"

# Volume layout per PLAN.md. Ownership is set on the host; the container
# runs as the host uid/gid passed via `docker exec --user`, so no user
# is created in the image.
RUN mkdir -p /var/quire/repos /var/quire/runs

WORKDIR /var/quire

EXPOSE 3000
ENTRYPOINT ["quire"]
CMD ["serve"]
