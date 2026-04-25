# Build stage.
FROM rust:1.88-bookworm AS builder

WORKDIR /usr/src/quire
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/quire/target \
    cargo install --path .

# Runtime stage.
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        git \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/cargo/bin/quire /usr/local/bin/quire

# Volume layout per PLAN.md. Ownership is set on the host; the container
# runs as the host uid/gid passed via `docker exec --user`, so no user
# is created in the image.
RUN mkdir -p /var/quire/repos /var/quire/runs

WORKDIR /var/quire

ENTRYPOINT ["quire"]
CMD ["serve"]
