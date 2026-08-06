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
        -p clock-reader -p flaky-bomb -p fuel-bomb -p memory-bomb \
        -p panic-bomb -p slow-host -p topic-parity

# Build everything the E2E suite needs.
build: build-engine build-module build-examples build-fixtures

# Build the module then run the engine with it. The second argument is the
# module's module.toml; a manifest is mandatory (an explicit path or a
# module.toml sibling of the wasm), and the engine refuses to boot without one.
run: build-module build-engine
    cargo run -p nexum-cli -- target/wasm32-wasip2/release/example.wasm modules/example/module.toml

# Run host engine unit tests.
test:
    cargo nextest run -p nexum-runtime

# Build module + engine, then run E2E integration tests.
test-e2e: build-module build-engine
    cargo nextest run -p nexum-runtime supervisor::tests::e2e supervisor::tests::digest::e2e_

# Format the workspace.
fmt:
    cargo fmt --all

# Lint the workspace.
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Check the workspace quickly.
check:
    cargo check --target wasm32-wasip2 -p example
    cargo check -p nexum-runtime
    cargo check -p nexum-cli

# Run the full CI series locally before pushing. Mirrors
# .github/workflows/ci.yml one-to-one: rustfmt, clippy, rustdoc, the
# module wasms the integration tests need, and the workspace test
# suite via nextest plus the doctests, all under the `-D warnings` the
# CI workflow sets globally.
ci:
    #!/usr/bin/env bash
    set -euo pipefail
    # Append -D warnings without clobbering the devshell's flags (mold linker,
    # set in flake.nix), so the local run keeps fast native linking. RUSTC_WRAPPER
    # is already sccache from the devshell shellHook.
    export RUSTFLAGS="${RUSTFLAGS:-} -D warnings"
    export RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -D warnings"
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo doc --workspace --no-deps
    cargo build --release --target wasm32-wasip2 \
        -p example -p price-alert -p balance-tracker -p http-probe \
        -p clock-reader -p flaky-bomb -p fuel-bomb -p memory-bomb \
        -p panic-bomb -p slow-host -p topic-parity
    # nextest for the suite (as CI does); doctests run separately since nextest
    # does not cover them.
    cargo nextest run --workspace --all-features --no-fail-fast
    cargo test --doc --workspace --all-features
