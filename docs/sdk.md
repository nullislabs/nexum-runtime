# The module SDK

`nexum-sdk` is the guest-side library a module builds against.
It supplies the `#[nexum_sdk::module]` macro, a trait for each host interface, the `Fault` error type, and helpers for chain reads, stored state, config, HTTP, and logging.
It is host-neutral: module logic written against its traits runs on the native target against mocks, with no wasm build.

Two crates go with it.
`nexum-sdk-test` holds the in-memory mocks for module tests: see [testing.md](testing.md).
`nexum-module-macros` holds the macro, and a module reaches it through `nexum_sdk::module` and never names the crate.

This page is an overview.
[tutorial-first-module.md](tutorial-first-module.md) is the place to start if you have not built a module yet.

## How to depend on it

The SDK crates are at version 0.1.0 and are not published to crates.io.

A module inside this repository uses a `path` dependency:

```toml
[dependencies]
nexum-sdk = { path = "../../../crates/nexum-sdk" }

[dev-dependencies]
nexum-sdk-test = { path = "../../../crates/nexum-sdk-test" }
```

A module in another repository uses a `git` dependency pinned to a commit:

```toml
[dependencies]
nexum-sdk = { git = "https://github.com/nullislabs/nexum-runtime", rev = "<commit>" }

[dev-dependencies]
nexum-sdk-test = { git = "https://github.com/nullislabs/nexum-runtime", rev = "<commit>" }
```

That module also carries a copy of `wit/nexum-host` from the same commit.
[The tutorial](tutorial-first-module.md#step-4-write-a-module-in-your-own-repository) gives the full procedure.

## API reference

The API reference is the rustdoc of the three crates.
Build it and open it from the repository root:

```sh
cargo doc -p nexum-sdk -p nexum-sdk-test -p nexum-module-macros --no-deps --open
```

The entry page is `target/doc/nexum_sdk/index.html`.
CI builds the workspace with the `missing_docs` lint denied, so every public item has a doc comment.

## The module macro

`#[nexum_sdk::module]` goes on an inherent `impl` block of a named, non-generic type.
It recognizes five handlers, and each returns `Result<(), Fault>`.

| Handler | Argument | Trigger |
|---|---|---|
| `init` | `Vec<(String, String)>` | none: it runs once after each instantiation, with the `[config]` pairs |
| `on_block` | `types::Block` | `block` |
| `on_event` | `types::Log` | `event` |
| `on_schedule` | `types::ScheduleTick` | `schedule`, which the engine parses and does not dispatch |
| `on_extension` | `types::ExtensionTrigger` | a kind an extension owns |

`types` is `nexum::host::types`, which the macro generates in your crate.
An absent handler is a no-op.
An `impl` with no handler, or with an `on_` function outside the table, does not compile.
Handlers are synchronous.

The macro reads `component.toml` beside `Cargo.toml` and builds a WIT world that imports exactly the `[dependencies]` the manifest declares.
It then emits the `wit_bindgen::generate!` call, the `WitBindgenHost` adapter, the `Fault` conversions, and the export glue.
The bindings for an undeclared capability do not exist, so a use of one is a compile error and not a load-time refusal.
`http` is the exception: it has no world import, so the engine checks it at load and the host allowlist gates each request.
A crate that keeps its own `wit_bindgen::generate!` call invokes `nexum_sdk::bind_host_via_wit_bindgen!` to get the same adapter.

`#[nexum_sdk::module(sol_events(Transfer, Approval))]` adds a compile-time check.
The build fails unless the topic-0 hashes of the named `sol!` events and the `event_signature` values of the manifest's event triggers are the same set.
`modules/fixtures/topic-parity` is a worked example.

## The host traits

`nexum_sdk::host` has one trait for each `nexum:host` interface.

| Trait | Interface | Manifest key | Methods |
|---|---|---|---|
| `ChainHost` | `nexum:host/chain` | `chain` | `request` |
| `LocalStoreHost` | `nexum:host/local-store` | `local-store` | `get`, `set`, `delete`, `list_keys`, `list_entries`, `apply`, `contains`, `len`, `count` |
| `LoggingHost` | `nexum:host/logging` | `logging` | `log` |

`WitBindgenHost` implements each trait the manifest declares.
`nexum_sdk_test::MockHost` implements all three.

Write a logic function against the traits it uses and no more:

```rust
pub fn on_block<H: ChainHost + LocalStoreHost>(host: &H, chain_id: u64) -> Result<(), Fault>
```

Do not bind the composed `Host` trait in module logic.
A module that declares two of the three capabilities has an adapter that cannot satisfy it: see [ADR-0015](adr/0015-host-trait-surface.md).

## Fault

`nexum_sdk::host::Fault` is the error type every handler and every host call returns.
It mirrors the `fault` variant in `wit/nexum-host/types.wit`.

| Case | Label | Meaning |
|---|---|---|
| `Unsupported(String)` | `unsupported` | The operator has not provisioned the capability. |
| `Unavailable(String)` | `unavailable` | The capability is down for now, for example an RPC outage. |
| `Denied(String)` | `denied` | The host refused the request, for example a method outside the permitted set. |
| `RateLimited` | `rate_limited` | An upstream service limited the rate. |
| `Timeout` | `timeout` | The operation took too long. |
| `InvalidInput(String)` | `invalid_input` | The input did not parse or validate. |
| `Internal(String)` | `internal` | Any other failure. |

The enum is `#[non_exhaustive]`, so a `match` on it needs a wildcard arm.
`Fault::invalid_input("...")` and the other lower-case constructors take anything that converts to a `String`.

A chain call returns `ChainError`, which is a `Fault` or an `RpcError`.
`RpcError` carries the node's `code`, its `message`, and the decoded revert bytes in `data`.
`ChainError` converts to `Fault` with `?`, and an `RpcError` becomes `Fault::Internal` on the way.
`HostFault::label` gives the label in the table, or `rpc` for an `RpcError`.

The macro generates the conversions between this type and the wit-bindgen `Fault` in your crate.
A logic function returns `nexum_sdk::host::Fault`, and `?` in the handler converts it.

What the engine does with a fault depends on where it comes from.

- `init` returns a fault at boot: the module is dead, and the supervisor does not restart it.
- A trigger handler returns a fault: the engine logs `on-trigger returned fault`, counts it in `nexum_runtime_module_errors_total`, and keeps the module alive.
- A handler panics: the run ends, and the supervisor restarts the module after a backoff.

Return a fault for a failure the next trigger can recover from.
Do not panic for one.

## Modules

### `chain`: chain reads

- `eth_call_params(to, data)` builds the `[{to, data}, "latest"]` params string.
- `parse_eth_call_result(json)` decodes the `"0x..."` result into bytes.
- `chainlink::read_latest_answer(host, chain_id, oracle, domain)` reads `latestRoundData` from a Chainlink AggregatorV3 feed.
  It returns `None` and logs a warning on any failure.
- `ChainMethod` is the closed set of JSON-RPC methods the host forwards.
  [07-rpc-namespace-design.md](07-rpc-namespace-design.md) lists them.
- `Chain` is the alloy chain type.

A module that needs one raw call uses `host.request(chain_id, method, params)`, as `balance-tracker` does for `eth_getBalance`.

`ProviderHost::provider(chain)` gives an alloy `RootProvider` over the host, and `block_on` drives its futures.
It needs a host that is `Clone + Send + Sync + 'static`.
The generated `WitBindgenHost` is a unit struct with no `Clone` impl, so add one in your crate before you call `provider` on it:

```rust
use alloy_provider::Provider as _;
use nexum_sdk::chain::{Chain, ProviderHost, block_on};

impl Clone for WitBindgenHost {
    fn clone(&self) -> Self {
        Self
    }
}

// In a handler:
let provider = WitBindgenHost.provider(Chain::sepolia());
let head = block_on(provider.get_block_number());
```

The `Provider` trait comes from `alloy-provider`, which your crate then takes as a direct dependency with `default-features = false`.
`MockHost` does not meet the `Clone + Send + Sync` bound, so test provider-level logic over a stub `ChainHost`.

### `store`: typed local-store helpers

The helpers work over any `LocalStoreHost`.
Typed values cross the store as borsh bytes.

| Type | Purpose | Methods |
|---|---|---|
| `TypedCell<H, T>` | one typed value under one key | `get`, `set`, `clear` |
| `TypedMap<H, T>` | typed values under a key prefix | `insert`, `get`, `remove`, `keys`, `clear` |
| `Counter<H>` | a `u64` that reads as 0 when absent | `get`, `add`, `set` |
| `WriteBatch<H>` | several writes in one host call | `set`, `delete`, `len`, `is_empty`, `flush` |

`clear_prefix(host, prefix)` deletes every key under a prefix and returns the count.

```rust
use borsh::{BorshDeserialize, BorshSerialize};
use nexum_sdk::host::{Fault, LocalStoreHost};
use nexum_sdk::store::{Counter, TypedCell, TypedMap, WriteBatch};

#[derive(BorshSerialize, BorshDeserialize)]
struct Watch {
    owner: String,
    since_block: u64,
}

fn store_tour<H: LocalStoreHost>(host: &H) -> Result<(), Fault> {
    let head = TypedCell::<_, u64>::new(host, "head");
    head.set(&19_000_000)?;
    assert_eq!(head.get()?, Some(19_000_000));

    let watches = TypedMap::<_, Watch>::new(host, "watch:");
    watches.insert(
        "alice",
        &Watch {
            owner: "alice".into(),
            since_block: 19_000_000,
        },
    )?;
    assert_eq!(watches.keys()?, ["alice"]);

    let seen = Counter::new(host, "blocks_seen");
    assert_eq!(seen.add(1)?, 1);

    let mut batch = WriteBatch::new(host);
    batch.set("cursor", b"19000000").delete("stale");
    batch.flush()
}
```

- A type you derive borsh for needs `borsh` as a direct dependency with the `derive` feature.
  The primitive types need nothing.
- `WriteBatch::flush` is atomic on the real host: every staged write lands or none does.
  The host caps one batch at 1024 writes and 4 MiB.
- `Counter::add` reads, adds, and writes.
  This is safe because the engine dispatches to a module one trigger at a time.
- A value that does not decode gives `Fault::Internal` with the key in the message.

### `config`: the `[config]` table

- `get_required(entries, key)` returns the value or a `ConfigError::MissingKey`.
- `get_optional(entries, key)` returns an `Option`.
- `scale_decimal(value, decimals, key)` turns a decimal string such as `"2500.00"` into an integer scaled by `10^decimals`.
- `Slot<T>` is a write-once holder for the parsed config: `store` it in `init`, and `get` it in a handler.

`ConfigError` converts to `nexum_sdk::host::Fault` with `?`.
The host flattens each TOML value to text, so every value arrives as a string.

### `address`: EVM addresses

`parse_address(raw)` parses one `0x` address.
`parse_address_list(raw)` parses a comma-separated list, and it tolerates white space around each entry.
Both errors convert to `Fault::InvalidInput`.

### `http`: outbound HTTP

The manifest must declare the `http` dependency and list each host the module may reach:

```toml
[dependencies]
http = { hosts = ["api.example.com"] }
```

- `Fetch` is the trait module logic binds, with `fetch` and `fetch_with`.
- `WasiFetch` is the implementation the glue passes in.
  It sends the request through `wasi:http`.
- `FetchOptions` sets the connect, first-byte, and between-bytes timeouts, which are 30 seconds each by default.
- `FetchError` has four cases: `Denied`, `InvalidRequest`, `Timeout`, and `Transport`.

Requests and responses are the `http` crate's `Request<Vec<u8>>` and `Response<Vec<u8>>`, so the module takes `http` as a direct dependency.
The host checks each request against `hosts` before it connects, and a host outside the list gives `FetchError::Denied`.
A test passes a stub that implements `Fetch`.
[`modules/examples/http-probe`](../modules/examples/http-probe) is a complete module with such a test.

### `tracing`: logs

A module logs with the standard `tracing` macros, and takes `tracing` as a direct dependency.
Call the generated `install_tracing()` once at the top of `init`.
The manifest must declare `logging`, or the function does not exist.

```rust
tracing::info!(block = number, seen, "block-counter: block");
```

- Each event crosses to the host with its level, its message, its call site, and its fields in their recorded types.
- Spans are inert.
- `install_tracing` also sets a panic hook, so a panic message reaches the host log before the run ends.
- The operator bounds a module's log volume with `[policy]` in `engine.toml`.
  A record over the size cap is truncated, and a module over the rate loses records.

### `keeper`: stores for conditional commitments

This module is a set of persistent-state conventions for a module that holds commitments and acts on each one when a condition is met.
Each type works over any `LocalStoreHost`.

- `CommitmentSet` is the registry: one `commitment:{owner}:{hash}` row per commitment.
- `Gates` holds a `next_block` or `next_epoch` threshold per commitment, and `is_ready` tests it.
- `Journal` is an idempotency journal keyed by receipt.
  `Journal::guard` runs one effect in a fixed reserve, effect, commit order, so a restart cannot repeat a committed effect.
- `Poller` is the trait for one probe of one commitment.
- `Retrier` applies a `RetryAction` to the stores after a failed run.

The module rustdoc has a runnable example.

### `sol_events`: logs as alloy types

`sol_events::Log` is the alloy RPC `Log`.
The macro generates `From<types::Log>` for it, so `let log: nexum_sdk::sol_events::Log = log.into();` in `on_event` gives a value the `alloy_sol_types` decoders accept.
`nexum_sdk::sol_types` re-exports `alloy_sol_types` for the macro's check; a crate that uses `sol!` still takes `alloy-sol-types` as a direct dependency.

### `prelude` and `Level`

`nexum_sdk::prelude` re-exports `Address`, `B256`, `Bytes`, `U256`, `address!`, `b256!`, `hex`, and `keccak256` from `alloy-primitives`.
`nexum_sdk::Level` is the log level type for every SDK log path.

## Compatibility

A module, the SDK, and the engine agree on one WIT package, `nexum:host@0.1.0`.
The engine checks a component's imports against its manifest at load and refuses a mismatch.
Build the module and the engine from the same commit until the SDK has published versions.
