FROM rust:1.88-trixie

RUN apt-get update && apt-get install -y --no-install-recommends \
    bc \
    grcov \
    jq \
    && rm -rf /var/lib/apt/lists/*

RUN rustup component add llvm-tools-preview
