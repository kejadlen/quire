default: all

fmt:
    cargo fmt --all

check:
    cargo check --workspace

clippy:
    cargo clippy --workspace -- -D warnings

test:
    cargo test --workspace -q

coverage:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUSTFLAGS="-Cinstrument-coverage"
    export CARGO_TARGET_DIR="target/coverage"
    export LLVM_PROFILE_FILE="target/coverage/profraw/%p-%m.profraw"
    rm -rf target/coverage
    cargo test --workspace -q
    REPORT=$(grcov target/coverage/profraw \
        --binary-path ./target/coverage/debug/ \
        -s . \
        -t covdir \
        --ignore-not-existing \
        --keep-only 'quire-server/src/**' \
        --ignore 'quire-server/src/bin/**' \
        --excl-line 'cov-excl-line|unreachable!|tracing::' \
        --excl-start 'cov-excl-start' \
        --excl-stop 'cov-excl-stop')
    echo "$REPORT" | jq -r '
        def files:
            to_entries[] | .value |
            if .children then .children | files
            else "\(.name): \(.coveragePercent)% (\(.linesCovered)/\(.linesTotal))"
            end;
        .children | files
    '
    COVERAGE=$(echo "$REPORT" | jq '.coveragePercent')
    echo ""
    echo "Total: ${COVERAGE}%"
    if [ "$(echo "$COVERAGE < 100" | bc -l)" -eq 1 ]; then
        echo "ERROR: Coverage is below 100%"
        exit 1
    fi

coverage-html:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUSTFLAGS="-Cinstrument-coverage"
    export CARGO_TARGET_DIR="target/coverage"
    export LLVM_PROFILE_FILE="target/coverage/profraw/%p-%m.profraw"
    rm -rf target/coverage
    cargo test --workspace -q
    rm -rf target/coverage/html
    grcov target/coverage/profraw \
        --binary-path ./target/coverage/debug/ \
        -s . \
        -t html \
        --ignore-not-existing \
        --keep-only 'quire-server/src/**' \
        --ignore 'quire-server/src/bin/**' \
        --excl-line 'cov-excl-line|unreachable!|tracing::' \
        --excl-start 'cov-excl-start' \
        --excl-stop 'cov-excl-stop' \
        -o target/coverage/html
    echo "HTML report at target/coverage/html/index.html"

mutants:
    #!/usr/bin/env bash
    set -uo pipefail
    cargo mutants --timeout-multiplier 3 -j4
    rc=$?
    if [ "$rc" -eq 0 ] || [ "$rc" -eq 3 ]; then
        exit 0
    fi
    exit "$rc"

all: fmt clippy test

install:
    cargo install --locked --path quire-server

# Manual release: tag a revision (default: @-) as v<UTC-date>-<short-sha> and push to github.
# Use this when the normal CI path is not working. Triggers the release workflow.
manual-release rev="@-":
    #!/usr/bin/env bash
    set -euo pipefail
    sha=$(jj log -r {{rev}} --no-graph -T commit_id --limit 1)
    short=${sha:0:8}
    date=$(TZ=UTC git show -s --format=%cd --date=format-local:%Y-%m-%d "$sha")
    tag="v${date}-${short}"
    git tag "$tag" "$sha"
    git push github "$tag"
    echo "Tagged and pushed $sha as $tag"
