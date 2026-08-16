# nexum-runtime

Nexum is a WASM Component Model host runtime for web3 modules. It supervises guest components built against the `nexum:host` WIT world, giving each module a capability-gated view of the host: chain access over JSON-RPC, an allowlisted `wasi:http` outbound gate, a local key-value store, clocks, and structured logging, with fuel and memory limits enforced per module.

This repository is the leaf of the Nullis runtime stack: it carries no cross-repo dependencies. Downstream repositories (videre, shepherd) build on the SDK and runtime published here.

## Layout

- `crates/nexum-runtime` - the engine host: wasmtime embedding, supervisor, capability providers, metrics.
- `crates/nexum-cli` - the bare `nexum` engine binary.
- `crates/nexum-launch` - shared launch surface (config loading, logging, presets).
- `crates/nexum-sdk` - the guest-side SDK modules build against.
- `crates/nexum-sdk-test` - SDK acceptance-test harness.
- `crates/nexum-module-macros` - proc-macros for module entrypoints.
- `crates/nexum-tasks` - task lifecycle and graceful shutdown.
- `crates/nexum-world` - single-source capability and fault-label vocabularies.
- `modules/example` - minimal reference module.
- `modules/examples/` - example modules (balance-tracker, http-probe, price-alert).
- `modules/fixtures/` - adversarial test fixtures (clock-reader, env-reader, flaky-bomb, fuel-bomb, memory-bomb, panic-bomb, slow-host, topic-parity).
- `wit/nexum-host` - the `nexum:host` WIT package.

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

The engine takes a component wasm and its `component.toml` (dependencies + config).
The manifest is mandatory: pass its path, or ship a `component.toml` next to the wasm.
Every manifest must declare a `[dependencies]` table; an empty one grants nothing.
Every manifest must also declare a `[component].name` that is not blank.
The engine uses the name as the state namespace, and it refuses a missing, empty, or whitespace-only name.

```sh
cargo run -p nexum-cli -- target/wasm32-wasip2/release/example.wasm modules/example/component.toml
```

A module that subscribes to `block` or `chain-log` events needs its chain declared in `engine.toml`, or the engine refuses to boot.
The smallest working stanza is:

```toml
[chains.11155111]
rpc_url = "http://localhost:8545"
```

`http(s)://` URLs are not dialled at boot; `ws(s)://` URLs are.
The example module declares no subscriptions, so `just run` needs no `engine.toml`; the modules under `modules/examples/` and `modules/fixtures/` do.

## Component integrity

A manifest may pin its artifact with `digest = "sha256:<64 hex chars>"` in `[component]` (one `sha256sum` of the `.wasm`).
A present pin is strictly verified against the loaded bytes before compilation; a mismatch or a malformed pin refuses the boot.
An absent pin loads with a warning that logs the computed digest; set `require_component_digest = true` under `[engine]` in `engine.toml` to make an absent pin a boot error.
The warning is silent when an `engine.toml` `[implements]` row pins the same artifact, because the bytes are verified against that pin instead.
The default sibling `component.toml` lives in the same trust domain as the artifact, so an author-side pin closes accidental drift only.
Against a compromised artifact store, supply an operator-owned manifest from outside the artifact directory via the `manifest` key on `[[modules]]`, combined with `require_component_digest = true`.

## Licence

AGPL-3.0. See [LICENSE](LICENSE).
