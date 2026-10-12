# Build your first module

This tutorial takes you from a fresh clone to a module of your own that runs under the engine.
It has four steps.
Step 1 runs the shipped `example` module against a live chain.
Step 2 reads the `price-alert` example, which shows the parts every module has.
Step 3 reads the `balance-tracker` example, which adds persistent state.
Step 4 builds a new module in its own repository, outside this one.

## Before you start

You need two things.

- **A toolchain.**
  The repository pins one in a Nix flake: `nix develop` enters it.
  Without Nix, install Rust 1.96 or later with the `wasm32-wasip2` target, `cargo-nextest`, and `just`.
- **A Sepolia JSON-RPC URL.**
  The examples declare triggers on Sepolia, chain id 11155111.
  A public endpoint is sufficient, for example `https://ethereum-sepolia-rpc.publicnode.com`.
  An `http(s)://` URL polls for new blocks, and a `ws(s)://` URL subscribes to them.

## Step 1: run the example module

```sh
git clone https://github.com/nullislabs/nexum-runtime
cd nexum-runtime
nix develop
export SEPOLIA_RPC_URL=https://ethereum-sepolia-rpc.publicnode.com
just run
```

`engine.dev.toml` reads the variable.
The engine substitutes `${SEPOLIA_RPC_URL}` before it parses the file, so the URL and any key in it stay out of the repository.
Export it in each shell you start the engine from.

`just run` builds `modules/example` for `wasm32-wasip2`, builds the engine, and starts the engine over `engine.dev.toml`.
The engine writes one JSON object per log line.
These four lines are from a real run, with the other lines removed:

```json
{"timestamp":"2026-10-11T23:34:06.427157Z","level":"INFO","message":"example module init (name=example)","module":"example","run":0,"channel":"host_interface","target":"nexum_runtime_logs::logs"}
{"timestamp":"2026-10-11T23:34:06.427283Z","level":"INFO","message":"supervisor ready","modules":1,"alive":1,"verified":0,"chains":1,"shutdown_drain_secs":150,"target":"nexum_runtime::builder"}
{"timestamp":"2026-10-11T23:34:06.632985Z","level":"INFO","message":"block source open","chain_id":11155111,"source_kind":"block","target":"nexum_runtime_supervisor::runtime::event_loop"}
{"timestamp":"2026-10-11T23:34:06.906367Z","level":"INFO","message":"block 11895035 on chain 11155111 (ts=1791761640000ms)","module":"example","run":0,"channel":"host_interface","target":"nexum_runtime_logs::logs"}
```

The first line is the module's `init` handler.
The last line is its `on_block` handler, and one such line follows for each new block.
Press Ctrl-C to stop the engine; it finishes the dispatch in flight and exits.

Add `--pretty-logs` for a human-readable format:

```sh
cargo run -p nexum-cli -- --engine-config engine.dev.toml --pretty-logs
```

A `WARN` line that says `loading unverified` is expected here.
`engine.dev.toml` relaxes the digest requirement because the artifact changes on every build: see [production.md](production.md).

If the engine stops at once, read the last line it wrote.

| Last line | Cause |
|---|---|
| ``environment variable `SEPOLIA_RPC_URL` referenced via ${SEPOLIA_RPC_URL} in engine.toml but not set`` | The variable is not exported in this shell. |
| `module example declares a trigger on chain 11155111 but engine.toml declares no [chains.11155111] entry` | The engine config has no entry for the chain the manifest names. |
| `module example declares a trigger on chain 11155111 but no engine.toml was found` | The command has no `--engine-config` flag, and the working directory has no `engine.toml`. |
| `no [[trigger]] entries - engine has nothing to run; exiting` | No loaded manifest declares a trigger. |

## Step 2: the parts of a module

A module is one Rust crate with four files.
Read the real files in [`modules/examples/price-alert`](../modules/examples/price-alert) beside this section.
`price-alert` reads a Chainlink oracle on each block and logs a warning when the price crosses a threshold.

```
price-alert/
|-- Cargo.toml       # a cdylib crate
|-- component.toml   # the manifest: identity, dependencies, triggers, config
`-- src/
    |-- lib.rs       # the #[nexum_sdk::module] glue
    `-- logic.rs     # pure logic and its tests
```

### The crate

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
nexum-sdk = { path = "../../../crates/nexum-sdk" }
alloy-primitives = { version = "1.6", default-features = false, features = ["std"] }
tracing = { version = "0.1", default-features = false }
strum = { version = "0.28", default-features = false, features = ["derive"] }
wit-bindgen = { version = "0.59", default-features = false, features = ["macros", "realloc"] }

[dev-dependencies]
nexum-sdk-test = { path = "../../../crates/nexum-sdk-test" }
alloy-sol-types = { version = "1.6", default-features = false, features = ["std"] }
```

- `crate-type = ["cdylib"]` makes the `wasm32-wasip2` build emit a component.
- `nexum-sdk` supplies the host traits, the helpers, and the `#[nexum_sdk::module]` macro.
- `wit-bindgen` is a direct dependency because the macro expands to a `wit_bindgen::generate!` call in your crate.
- `nexum-sdk-test` is a dev-dependency, so its mocks never reach the artifact.
- A module never depends on the engine crates.

### The manifest

`component.toml` sits beside `Cargo.toml`.
The engine reads it at load, and the macro reads it at compile time.

```toml
[component]
name = "price-alert"
version = "0.1.0"

[dependencies]
logging = {}
chain = {}

[[trigger]]
on = "block"
chain_id = 11155111

[config]
oracle_address = "0x694AA1769357215DE4FAC081bf1f309aDC325306"
decimals = "8"
threshold = "2500.00"
direction = "below"
every_n_blocks = "1"
```

- `[component].name` is mandatory and must not be blank.
  The engine uses it as the namespace for the module's stored state.
- `[dependencies]` is mandatory, and an empty table is valid.
  Each key names one host capability: `chain`, `local-store`, `logging`, or `http`.
  The macro builds a WIT world that imports exactly these, so the bindings for an undeclared capability do not exist in your crate.
- Each `[[trigger]]` table declares one thing the engine sends to the module.
  `on = "block"` sends each new block on `chain_id`.
  `on = "event"` sends each log that matches `address` and `event_signature` on `chain_id`.
  `on = "schedule"` parses but the engine does not dispatch it, so do not build on it.
- `[config]` is opaque to the engine.
  The module receives it in `init` as a list of string pairs.

[02-modules-triggers-packaging.md](02-modules-triggers-packaging.md) is the full reference for the manifest.

### The pure logic

`src/logic.rs` holds the decision logic as a function that is generic over the host.
It names neither `wit-bindgen` nor the engine, so a test calls it directly.

```rust
use nexum_sdk::chain::chainlink::read_latest_answer;
use nexum_sdk::host::{ChainHost, Fault, LoggingHost};

pub fn on_block<H: ChainHost + LoggingHost>(
    host: &H,
    chain_id: u64,
    settings: &Settings,
    block_number: u64,
) -> Result<(), Fault> {
    if !block_number.is_multiple_of(settings.every_n_blocks) {
        return Ok(());
    }
    let Some(answer) = read_latest_answer(host, chain_id, settings.oracle_address, "price-alert")
    else {
        // read_latest_answer already logged the failure at Warn.
        return Ok(());
    };
    if classify(answer, settings.threshold_scaled, settings.direction) {
        tracing::warn!(
            answer = %answer,
            threshold = %settings.threshold_scaled,
            direction = ?settings.direction,
            "price-alert: THRESHOLD CROSSED",
        );
    } else {
        tracing::info!(
            answer = %answer,
            threshold = %settings.threshold_scaled,
            direction = ?settings.direction,
            "price-alert: ok",
        );
    }
    Ok(())
}
```

- The function binds only the host traits it uses: `ChainHost + LoggingHost`, which match the two declared dependencies.
  It does not bind the composed `Host` trait: see [ADR-0015](adr/0015-host-trait-surface.md).
- A failed oracle read returns `Ok`, so the next block polls again.
  A handler that returns `Err` stays alive: the engine logs `on-trigger returned fault` and counts it.
  A handler that panics ends the run, and the supervisor restarts the module after a backoff.

`parse_config` in the same file turns the `[config]` pairs into a typed `Settings` with the `nexum_sdk::config` helpers.

### The glue

`src/lib.rs` declares the handlers and leaves the rest to the macro.

```rust
#![allow(clippy::too_many_arguments)]

mod logic;

use std::sync::OnceLock;

use nexum::host::types;

static SETTINGS: OnceLock<logic::Settings> = OnceLock::new();

struct PriceAlert;

#[nexum_sdk::module]
impl PriceAlert {
    fn init(config: Vec<(String, String)>) -> Result<(), Fault> {
        install_tracing();
        let cfg = logic::parse_config(&config)?;
        tracing::info!(
            "price-alert init: oracle={:#x} threshold={} direction={:?} every_n_blocks={}",
            cfg.oracle_address,
            cfg.threshold_scaled,
            cfg.direction,
            cfg.every_n_blocks,
        );
        let _ = SETTINGS.set(cfg);
        Ok(())
    }

    fn on_block(block: types::Block) -> Result<(), Fault> {
        let Some(cfg) = SETTINGS.get() else {
            return Ok(());
        };
        logic::on_block(&WitBindgenHost, block.chain_id, cfg, block.number)?;
        Ok(())
    }
}
```

Apply `#[nexum_sdk::module]` to an inherent `impl` block of a named, non-generic type.
The macro recognizes five handler names.

| Handler | Argument | Called |
|---|---|---|
| `init` | `Vec<(String, String)>` | Once after each instantiation, with the `[config]` pairs. |
| `on_block` | `types::Block` | For each block of a `block` trigger. |
| `on_event` | `types::Log` | For each log of an `event` trigger. |
| `on_schedule` | `types::ScheduleTick` | Never today: the engine does not dispatch `schedule` triggers. |
| `on_extension` | `types::ExtensionTrigger` | For each delivery of an extension trigger kind. |

Each handler returns `Result<(), Fault>`.
An absent handler is a no-op for its trigger.
The macro refuses an `impl` with no recognized handler.
It also refuses a function whose name starts with `on_` and is not in the table, so a misspelt handler fails the build.

The macro generates the following items in your crate:

- the `wit_bindgen::generate!` call, which defines the `nexum::host` modules and the `Fault` type;
- `WitBindgenHost`, which implements `ChainHost`, `LocalStoreHost`, and `LoggingHost` for those of `chain`, `local-store`, and `logging` that the manifest declares;
- the conversions between the generated `Fault` and `nexum_sdk::host::Fault`, so `?` works in a handler;
- `install_tracing`, when the manifest declares `logging`;
- the export glue that routes each trigger to its handler.

Call `install_tracing()` once at the top of `init`.
After that, the `tracing` macros reach the host log from any function.

The crate root must not define its own `Result`, `Vec`, or `Ok`, because the generated code uses those names unqualified.

### The tests

The logic is generic over the host, so its tests are plain Rust and need no wasm build.
They run it against `nexum_sdk_test::MockHost` and capture the `tracing` output.

```rust
#[test]
fn on_block_crosses_below_threshold() {
    let host = MockHost::new();
    let settings = sample_settings(250_050_000_000, Direction::Below);
    programmed_eth_call(
        &host,
        settings.oracle_address,
        Ok(oracle_response_json(200_000_000_000)),
    );

    let (result, logs) = capture_tracing(|| on_block(&host, 11_155_111, &settings, 100));
    result.unwrap();

    let ev = logs.expect_one(|e| e.level == Level::WARN);
    assert_eq!(ev.message, "price-alert: THRESHOLD CROSSED");
    assert_eq!(ev.field_str("direction").as_deref(), Some("Below"));
    assert_eq!(ev.field_str("answer").as_deref(), Some("200000000000"));
}
```

```sh
cargo nextest run -p price-alert
```

[testing.md](testing.md) describes the mocks.

### Build and run

```sh
just build-examples
cargo run -p nexum-cli -- \
  target/wasm32-wasip2/release/price_alert.wasm \
  modules/examples/price-alert/component.toml \
  --engine-config engine.dev.toml --pretty-logs
```

A wasm path and a manifest path on the command line boot that one module.
The engine then ignores the `[[modules]]` entries in the config, and it says so in a `WARN` line.
It still reads `[chains]` from the config, which the block trigger needs.

The module writes its `init` line, and then one line per block:

```
INFO nexum_runtime_logs::logs: price-alert init: oracle=0x694aa1769357215de4fac081bf1f309adc325306 threshold=250000000000 direction=Below every_n_blocks=1 module="price-alert" run=0 channel="host_interface" source="price_alert"
INFO nexum_runtime_logs::logs: price-alert: ok module="price-alert" run=0 channel="host_interface" source="price_alert::logic" fields="answer=253132000000 threshold=250000000000 direction=Below"
```

The line reads `price-alert: THRESHOLD CROSSED` at `WARN` when the answer is on the firing side of the threshold.
Change `threshold` or `direction` in the manifest and start the engine again to see it.
Config is read at load, so a config change needs no rebuild.

## Step 3: add persistent state

[`modules/examples/balance-tracker`](../modules/examples/balance-tracker) reads the native balance of each configured address on every block.
It stores the last value, and it logs a warning when a balance moves by more than a threshold.

It differs from `price-alert` in two places.
The manifest declares one more dependency:

```toml
[dependencies]
logging = {}
chain = {}
local-store = {}
```

The logic binds the store trait in place of the logging trait, and it reads and writes through the host:

```rust
fn check_one<H: ChainHost + LocalStoreHost>(
    host: &H,
    chain_id: u64,
    addr: Address,
    threshold: U256,
) -> Result<(), Fault> {
    let current = fetch_balance(host, chain_id, addr)?;
    let key = balance_key(&addr);
    let prior = host.get(&key)?.and_then(|b| parse_u256_le(&b));

    if let Some(prior) = prior
        && abs_diff(current, prior) >= threshold
    {
        let direction = if current > prior { "+" } else { "-" };
        tracing::warn!(
            "balance-tracker {addr:#x} changed {direction}{} wei (prior={prior}, current={current})",
            abs_diff(current, prior),
        );
    }
    host.set(&key, &u256_to_le_bytes(current))?;
    Ok(())
}
```

The key is `balance:{addr}`, and the value is the balance as 32 little-endian bytes.

```sh
cargo run -p nexum-cli -- \
  target/wasm32-wasip2/release/balance_tracker.wasm \
  modules/examples/balance-tracker/component.toml \
  --engine-config engine.dev.toml --pretty-logs
```

The module writes `balance-tracker init: 2 addresses, threshold=1000000000000000 wei` and is then silent until a tracked balance moves.
A run with no line after `block source open` is correct.
The shutdown line reports the work, for example `graceful shutdown complete dispatched_blocks=4`.

Three properties of the store matter here.

- Each module has its own namespace, keyed on `[component].name`.
  Two modules that write the same key do not collide.
- The store is outside the wasm instance.
  The data is under `state_dir`, which is `./data` in `engine.dev.toml`, and it survives a module restart and an engine restart.
- Module memory does not survive a restart.
  Each restart runs `init` again on a fresh instance, so keep in memory only what `init` can rebuild.

`nexum_sdk::store` has typed helpers over the same trait, so a module does not write its own byte encoding: see [sdk.md](sdk.md#store-typed-local-store-helpers).

## Step 4: write a module in your own repository

The examples live inside this workspace, so two things reach them without any work from you.
A module in its own repository must supply both.

| Need | In this workspace | In your repository |
|---|---|---|
| The SDK crates | `path` dependencies | `git` dependencies pinned to a commit |
| The `nexum:host` WIT files | the `wit/` directory at the repository root | a copy in your crate at `wit/nexum-host` |

The SDK is not published to crates.io, so a `git` dependency with a `rev` is the only way to take it.
The other three requirements are the same in both places: the `cdylib` crate type, a direct `wit-bindgen` dependency, and a `component.toml` beside `Cargo.toml`.

The module in this step counts the blocks it sees and stores the count.
Make the directory beside your `nexum-runtime` clone:

```
block-counter/
|-- Cargo.toml
|-- component.toml
|-- src/
|   |-- lib.rs
|   `-- logic.rs
`-- wit/
    `-- nexum-host/      # copied from nexum-runtime
```

### Pin the SDK

Take the commit of your clone:

```sh
git -C ../nexum-runtime rev-parse HEAD
```

Put that value in place of `<commit>` in `Cargo.toml`:

```toml
[package]
name = "block-counter"
version = "0.1.0"
edition = "2024"
rust-version = "1.96.0"

[lib]
crate-type = ["cdylib"]

[dependencies]
nexum-sdk = { git = "https://github.com/nullislabs/nexum-runtime", rev = "<commit>" }
tracing = { version = "0.1", default-features = false }
wit-bindgen = { version = "0.59", default-features = false, features = ["macros", "realloc"] }

[dev-dependencies]
nexum-sdk-test = { git = "https://github.com/nullislabs/nexum-runtime", rev = "<commit>" }
```

Use the same `rev` for both crates.
`wit-bindgen` must be a direct dependency.
Start from the version the examples in the pinned commit declare, which is 0.59 here.

### Copy the WIT files

```sh
mkdir -p wit
cp -r ../nexum-runtime/wit/nexum-host wit/nexum-host
```

The macro looks for a `wit/` directory in your crate directory, and then in each parent directory in turn.
It uses the first one it finds and no other.
That directory must hold the `nexum-host` package, at `wit/nexum-host` or at `wit/deps/nexum-host`.

The copy must come from the same commit as the `rev`.
The engine you run must be built from that commit too.
The bindings, the SDK, and the engine then agree on one `nexum:host` interface.
When you move the `rev`, copy the directory again.

### Write the manifest

`component.toml`:

```toml
[component]
name = "block-counter"
version = "0.1.0"

[dependencies]
logging = {}
local-store = {}

[[trigger]]
on = "block"
chain_id = 11155111
```

### Write the logic

`src/logic.rs`:

```rust
use nexum_sdk::host::{Fault, LocalStoreHost};
use nexum_sdk::store::Counter;

/// Count the block and log the running total.
pub fn on_block<H: LocalStoreHost>(host: &H, number: u64) -> Result<u64, Fault> {
    let seen = Counter::new(host, "blocks_seen").add(1)?;
    tracing::info!(block = number, seen, "block-counter: block");
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexum_sdk::Level;
    use nexum_sdk_test::{MockHost, capture_tracing};

    #[test]
    fn counts_each_block() {
        let host = MockHost::new();

        let (first, _) = capture_tracing(|| on_block(&host, 100));
        let (second, logs) = capture_tracing(|| on_block(&host, 101));

        assert_eq!(first.unwrap(), 1);
        assert_eq!(second.unwrap(), 2);
        let event = logs.expect_one(|e| e.level == Level::INFO);
        assert_eq!(event.message, "block-counter: block");
        assert_eq!(event.field_str("seen").as_deref(), Some("2"));
    }
}
```

`src/lib.rs`:

```rust
// The generated host-import shims can have more arguments than clippy allows.
#![allow(clippy::too_many_arguments)]

mod logic;

use nexum::host::types;

struct BlockCounter;

#[nexum_sdk::module]
impl BlockCounter {
    fn init(_config: Vec<(String, String)>) -> Result<(), Fault> {
        install_tracing();
        tracing::info!("block-counter init");
        Ok(())
    }

    fn on_block(block: types::Block) -> Result<(), Fault> {
        logic::on_block(&WitBindgenHost, block.number)?;
        Ok(())
    }
}
```

### Test, build, run

Test on the native target:

```sh
cargo test
```

Build the component:

```sh
cargo build --target wasm32-wasip2 --release
```

The artifact is `target/wasm32-wasip2/release/block_counter.wasm`.
Cargo replaces each hyphen in the package name with an underscore.

Run it with the engine from your clone.
Give both paths as absolute paths, because the command runs in the clone:

```sh
cd ../nexum-runtime
cargo run -p nexum-cli -- \
  /path/to/block-counter/target/wasm32-wasip2/release/block_counter.wasm \
  /path/to/block-counter/component.toml \
  --engine-config engine.dev.toml --pretty-logs
```

```
INFO nexum_runtime_logs::logs: block-counter init module="block-counter" run=0 channel="host_interface" source="block_counter"
INFO nexum_runtime_logs::logs: block-counter: block module="block-counter" run=0 channel="host_interface" source="block_counter::logic" fields="block=11895072 seen=1"
INFO nexum_runtime_logs::logs: block-counter: block module="block-counter" run=0 channel="host_interface" source="block_counter::logic" fields="block=11895073 seen=2"
```

Stop the engine and start it again.
The count continues from the stored value.

### When the build or the boot fails

| Message | Cause |
|---|---|
| ``no `wit/` tree exists under <dir> or any ancestor`` | Your crate and its parent directories have no `wit/` directory. Copy the WIT files. |
| ``declared capabilities need the `nexum-host` WIT package, but neither `wit/deps/nexum-host` nor `wit/nexum-host` exists in <dir>`` | The nearest `wit/` directory does not hold the package. If `<dir>` is not in your crate, a parent directory has a `wit/` directory of its own: add one to your crate. |
| ``could not read <dir>/component.toml`` | The manifest is not beside `Cargo.toml`. |
| ``component.toml has no [dependencies] table`` | Add the table. An empty one is valid. |
| ``unknown dependency `<name>` in component.toml [dependencies]`` | The key is not a capability name. The message lists the valid names. |
| `` use of unresolved module or unlinked crate `wit_bindgen` `` | `wit-bindgen` is not a direct dependency of your crate. |
| `` `on_blocks` is not a recognized #[nexum_sdk::module] handler `` | A function name starts with `on_` and is not one of the five handlers. |
| The build succeeds and there is no `.wasm` file | `crate-type = ["cdylib"]` is absent, so cargo built an `.rlib` only. |
| `module block-counter declares a trigger on chain <id> but engine.toml declares no [chains.<id>] entry` | Add the chain to the engine config. |

A `[chains.<id>]` entry is an operator grant.
A manifest cannot add a chain from its side: see [ADR-0001](adr/0001-operator-config-separate-and-trusted.md).

### Add a module to this repository instead

A module inside this workspace takes the shorter path.
Put the crate under `modules/`, use `path` dependencies as the examples do, and add the crate to `members` in the root `Cargo.toml`.
The crate needs no `wit/` directory, because the search reaches the one at the repository root.
Declare external dependencies in the module's own `Cargo.toml`: a module under `modules/` does not inherit from `[workspace.dependencies]`.

## Where to go from here

- [sdk.md](sdk.md) is the overview of the SDK: the host traits, `Fault`, and each helper module.
- [testing.md](testing.md) covers the module mocks and the engine-side harness.
- [02-modules-triggers-packaging.md](02-modules-triggers-packaging.md) is the reference for the manifest, the lifecycle, and the trigger system.
- [07-rpc-namespace-design.md](07-rpc-namespace-design.md) lists the JSON-RPC methods a module may call.
- [`modules/examples/http-probe`](../modules/examples/http-probe) is a module that makes outbound HTTP requests through the `http` dependency.
- [production.md](production.md) is the operator handbook: digests, systemd, backup, and metrics.
