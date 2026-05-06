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
COPY --from=planner /build/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/build/target \
    cargo chef cook --release --recipe-path recipe.json
COPY . .
# Copy the binary out of the cache mount so it survives into the runtime stage.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin quire && \
    cp target/release/quire /build/quire

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
COPY --from=builder /build/quire /usr/local/bin/quire

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
