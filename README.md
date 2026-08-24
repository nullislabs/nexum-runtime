# nexum-runtime

Nexum is a WASM Component Model host runtime for web3 modules. It supervises guest components built against the `nexum:host` WIT world, giving each module a capability-gated view of the host: chain access over JSON-RPC, an allowlisted `wasi:http` outbound gate, a local key-value store, clocks, and structured logging, with fuel and memory limits enforced per module.

This repository is the leaf of the Nullis runtime stack: it carries no cross-repo dependencies. Downstream repositories (videre, shepherd) build on the SDK and runtime published here.

## Layout

- `crates/nexum-runtime` - the engine host: wasmtime embedding, supervisor, capability providers, metrics.
- `crates/nexum-primitives` - module and interface identity, content digests, and host allowlist patterns.
- `crates/nexum-cli` - the bare `nexum` engine binary.
- `crates/nexum-launch` - shared launch surface (config loading, logging, presets).
- `crates/nexum-sdk` - the guest-side SDK modules build against.
- `crates/nexum-sdk-test` - SDK acceptance-test harness.
- `crates/nexum-module-macros` - proc-macros for module entrypoints.
- `crates/nexum-tasks` - task lifecycle and graceful shutdown.
- `crates/nexum-world` - single-source capability and fault-label vocabularies.
- `modules/example` - minimal reference module.
- `modules/examples/` - example modules (balance-tracker, http-probe, price-alert).
- `modules/fixtures/` - adversarial test fixtures (clock-reader, env-reader, flaky-bomb, fuel-bomb, log-bomb, memory-bomb, panic-bomb, slow-host, topic-parity).
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
cargo run -p nexum-cli -- --engine-config engine.dev.toml
```

`engine.dev.toml` is committed, and it declares the example as a `[[modules]]` entry, which is the path a deployment uses.
The engine also takes a wasm and a manifest as positional arguments, which boots one module without any `engine.toml`:

```sh
cargo run -p nexum-cli -- target/wasm32-wasip2/release/example.wasm modules/example/component.toml
```

A module that declares a `block` or `event` trigger needs its chain declared in `engine.toml`, or the engine refuses to boot.
The smallest working stanza is:

```toml
[chains.11155111]
rpc_url = "http://localhost:8545"
```

`http(s)://` URLs are not dialled at boot; `ws(s)://` URLs are.
The example module declares no triggers, so `engine.dev.toml` configures no chain; the modules under `modules/examples/` and `modules/fixtures/` need one.

## Component integrity

Every `[[modules]]` entry in `engine.toml` carries `digest = "sha256:<64 hex chars>"`, and the engine refuses to boot an entry without one.
`nexum digest <artifact>` prints that value, and it prints nothing else, so the line pastes into the key:

```sh
nexum digest target/wasm32-wasip2/release/example.wasm
```

The refusal also reports the digest of the bytes it read, so a first boot tells you the value to paste.
Set `require_component_digest = false` under `[engine]` to relax the requirement, which is what the committed `engine.dev.toml` does for a build that changes on every `just build-module`.
A component given on the command line instead of in a `[[modules]]` entry is exempt: there is no entry for a pin to sit on, and naming the path is the same authorization.

A manifest may also pin its own artifact with `digest` in `[component]`.
Both pins are verified strictly against the loaded bytes before compilation, and a mismatch or a malformed pin refuses the boot naming which pin failed.
The two are independent, so the author pin never satisfies the operator requirement.
The default sibling `component.toml` lives in the same trust domain as the artifact, so an author-side pin closes accidental drift only.

`[[modules]].digest` does not on its own close a compromised artifact store.
It fixes the bytes of the `.wasm`, but the sibling `component.toml` stays inside the compromised directory, and that manifest is the sole HTTP allowlist, the `[config]` source, and the state-namespace selector.
Closing the gap needs an operator-owned manifest outside the artifact directory, named by the `manifest` key on the `[[modules]]` entry.

## Licence

AGPL-3.0-or-later. See [LICENSE](LICENSE).
