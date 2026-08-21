# Build the bare `nexum` engine binary.
build-engine:
    cargo build -p nexum-cli

# Build the example WASM module.
build-module:
    cargo build --target wasm32-wasip2 --release -p example

# Build the example modules (price-alert + balance-tracker + http-probe)
# for wasm32-wasip2.
build-examples:
    cargo build --target wasm32-wasip2 --release -p price-alert -p balance-tracker -p http-probe

# Build the test fixture modules for wasm32-wasip2.
build-fixtures:
    cargo build --target wasm32-wasip2 --release \
        -p clock-reader -p env-reader -p flaky-bomb -p fuel-bomb \
        -p log-bomb -p memory-bomb -p panic-bomb -p slow-host -p topic-parity

# Build everything the E2E suite needs.
build: build-engine build-module build-examples build-fixtures

# Build the module then run the engine with it. The second argument is the
# module's component.toml; a manifest is mandatory (an explicit path or a
# component.toml sibling of the wasm), and the engine refuses to boot without one.
run: build-module build-engine
    cargo run -p nexum-cli -- target/wasm32-wasip2/release/example.wasm modules/example/component.toml

# Run host engine unit tests.
test:
    cargo nextest run -p nexum-runtime -p nexum-primitives -p nexum-runtime-api -p nexum-runtime-chain -p nexum-runtime-config -p nexum-runtime-http -p nexum-runtime-logs -p nexum-runtime-manifest -p nexum-runtime-metrics -p nexum-runtime-store -p nexum-runtime-supervisor -p nexum-runtime-testing -p nexum-runtime-wasm --all-features

# Build module + engine, then run E2E integration tests.
test-e2e: build-module build-engine
    cargo nextest run -p nexum-runtime-supervisor -p nexum-runtime supervisor::tests::e2e supervisor::tests::digest::e2e_ harness::tests::host_interface_records harness::tests::a_log_flood_is_capped

# Format the workspace.
fmt:
    cargo fmt --all

# Lint the workspace.
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Check house style: banned characters in tracked files, and the commit
# messages this branch adds over main. Compiles nothing.
content range="main..HEAD":
    ./scripts/content-lint.sh "{{ range }}"

# Check the venue-agnostic invariant the nexum-runtime rustdoc claims.
# Compiles nothing.
zero-leak:
    ./scripts/zero-leak.sh

msrv:
    ./scripts/msrv-lint.sh

# Every `[workspace.dependencies]` entry has an inheritor, and no guest module
# is one. Compiles nothing.
workspace-deps:
    ./scripts/workspace-deps-lint.sh

# Per-crate unused dependencies. Compiles nothing.
machete:
    cargo machete

# Check the workspace quickly.
check:
    cargo check --target wasm32-wasip2 -p example
    cargo check -p nexum-runtime
    cargo check -p nexum-cli

# Run the full CI series locally before pushing. Mirrors
# .github/workflows/ci.yml one-to-one: house style, the zero-leak gate, the
# The full CI series.
ci:
    #!/usr/bin/env bash
    set -euo pipefail
    # Append -D warnings without clobbering the devshell's flags (mold linker,
    # set in flake.nix), so the local run keeps fast native linking. RUSTC_WRAPPER
    # is already sccache from the devshell shellHook.
    export RUSTFLAGS="${RUSTFLAGS:-} -D warnings"
    export RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -D warnings"
    ./scripts/content-lint.sh "main..HEAD"
    ./scripts/zero-leak.sh
    ./scripts/msrv-lint.sh
    ./scripts/workspace-deps-lint.sh
    cargo machete
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo doc --workspace --all-features --no-deps
    cargo build --release --target wasm32-wasip2 \
        -p example -p price-alert -p balance-tracker -p http-probe \
        -p clock-reader -p env-reader -p flaky-bomb -p fuel-bomb \
        -p log-bomb -p memory-bomb -p panic-bomb -p slow-host -p topic-parity
    # nextest for the suite (as CI does); doctests run separately since nextest
    # does not cover them.
    cargo nextest run --workspace --all-features --no-fail-fast
    cargo test --doc --workspace --all-features
