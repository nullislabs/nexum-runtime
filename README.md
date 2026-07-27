# nexum-runtime

Nexum is a WASM Component Model host runtime for web3 modules. It supervises guest components built against the `nexum:host` WIT world, giving each module a capability-gated view of the host: chain access over JSON-RPC, an allowlisted `wasi:http` outbound gate, a local key-value store, clocks, and structured logging — with fuel, memory, and epoch limits enforced per module.

This repository is the leaf of the Nullis runtime stack: it carries no cross-repo dependencies. Downstream repositories (videre, shepherd) build on the SDK and runtime published here.

## Layout

- `crates/nexum-runtime` — the engine host: wasmtime embedding, supervisor, capability providers, metrics.
- `crates/nexum-cli` — the bare `nexum` engine binary.
- `crates/nexum-launch` — shared launch surface (config loading, logging, presets).
- `crates/nexum-sdk` — the guest-side SDK modules build against.
- `crates/nexum-sdk-test` — SDK acceptance-test harness.
- `crates/nexum-module-macros` — proc-macros for module entrypoints.
- `crates/nexum-tasks` — task lifecycle and graceful shutdown.
- `crates/nexum-world` — single-source capability and fault-label vocabularies.
- `modules/example` — minimal reference module.
- `modules/examples/` — example modules (balance-tracker, http-probe, price-alert).
- `modules/fixtures/` — adversarial test fixtures (clock-reader, flaky-bomb, fuel-bomb, memory-bomb, panic-bomb, slow-host).
- `tools/load-gen` — load generator for soak runs.
- `wit/nexum-host` — the `nexum:host` WIT package.

## Development

The repository pins its toolchain via a Nix flake (Rust 1.94.0, matching CI):

```sh
nix develop        # or `direnv allow` once
just build         # engine + all guest wasms
just test          # host engine unit tests
just ci            # full CI series locally (fmt, clippy, doc, wasms, nextest, doctests)
```

Without Nix, any Rust 1.94+ toolchain with the `wasm32-wasip2` target, `cargo-nextest`, and `just` works.

## Running a module

```sh
just run           # builds the example module and runs the engine with it
```

The engine takes a component wasm and its `module.toml` (capabilities + config):

```sh
cargo run -p nexum-cli -- target/wasm32-wasip2/release/example.wasm modules/example/module.toml
```

## Licence

AGPL-3.0. See [LICENSE](LICENSE).
