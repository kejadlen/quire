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

RUN groupadd --system quire \
    && useradd --system --gid quire --create-home quire

# Volume layout per PLAN.md.
RUN mkdir -p /var/quire/repos /var/quire/runs \
    && chown -R quire:quire /var/quire

USER quire
WORKDIR /var/quire

ENTRYPOINT ["quire"]
CMD ["serve"]
