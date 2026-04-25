FROM ramekin-agent
RUN apt-get update && apt-get install -y --no-install-recommends \
    bc \
    cargo \
    grcov \
    jq \
    rustc \
    rustfmt \
    && rm -rf /var/lib/apt/lists/*
ENV RANGER_DEFAULT_BACKLOG=quire
