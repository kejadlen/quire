FROM ramekin-agent
RUN apt-get update && apt-get install -y --no-install-recommends \
    bc \
    curl \
    grcov \
    jq \
    && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --component clippy,llvm-tools-preview,rustfmt
ENV PATH="/root/.cargo/bin:${PATH}"
ENV RANGER_DEFAULT_BACKLOG=quire
