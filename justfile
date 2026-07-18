# Build the bare `nexum` engine.
build-engine:
    cargo build -p nexum-cli

# Build the example WASM module.
build-module:
    cargo build --target wasm32-wasip2 --release -p example

# Build everything.
build: build-engine build-module

# Build the module then run the engine with it. The second argument is the
# module's module.toml — without it the engine prints the 0.1-compat
# deprecation warning and proceeds with empty capabilities/config.
run: build-module build-engine
    cargo run -p nexum-cli -- target/wasm32-wasip2/release/example.wasm modules/example/module.toml

# Run host engine unit tests.
test:
    cargo test -p nexum-runtime

# Build module + engine, then run E2E integration tests.
test-e2e: build-module build-engine
    cargo test -p nexum-runtime supervisor::tests::e2e

# Zero-leak gate: host-layer crate graphs, runtime charter-symbol and
# router-field scans, and the nexum:host WIT leaf and foreign-namespace
# scans. Blocking in CI.
check-venue-agnostic:
    ./scripts/check-venue-agnostic.sh

# Check the workspace.
check:
    cargo check --target wasm32-wasip2 -p example
    cargo check --workspace

# Run the full CI series locally before pushing. Mirrors
# .github/workflows/ci.yml one-to-one: rustfmt, clippy, rustdoc, the
# module wasms the integration tests need, and the workspace test
# suite, all under the `-D warnings` the CI workflow sets globally.
ci:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUSTFLAGS="-D warnings"
    export RUSTDOCFLAGS="-D warnings"
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo doc --workspace --no-deps
    cargo build --release --target wasm32-wasip2 \
        -p example -p price-alert -p balance-tracker -p http-probe \
        -p clock-reader -p flaky-bomb -p fuel-bomb -p memory-bomb \
        -p panic-bomb -p slow-host
    cargo test --workspace --all-features --no-fail-fast
