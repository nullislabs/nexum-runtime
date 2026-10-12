# Testing

The repository has two test surfaces, and each tests a different thing.
Read this page before you write a test, so the test goes in the right one.

| Surface | Crate | Tests | Needs a wasm build |
|---|---|---|---|
| Module mocks | `nexum-sdk-test` | what a module decides, given a host state | no |
| Engine harness | `nexum-runtime` with the `test-utils` feature, over `nexum-runtime-testing` | what the engine does with a real component | yes |

## The rule

- **Module logic is tested as plain Rust.**
  The logic lives in `logic.rs` as functions generic over the host traits, and its tests run it against `nexum_sdk_test::MockHost`.
  There is no wasmtime, no component boundary, and no engine crate in such a test.
  Every shipped example follows this pattern.
- **The engine harness is for the engine.**
  It covers supervision, restart and poison, resource limits, dispatch across modules, log capture, capability wiring, and stream reconnect.
  These need a compiled `.wasm` component over mock backends.
- **Do not test module logic through the harness.**
  A harness test that could be a `MockHost` test with no loss of coverage is in the wrong place.

## Module tests: `nexum-sdk-test`

Add the crate as a dev-dependency, so the mocks never reach the artifact.
[sdk.md](sdk.md#how-to-depend-on-it) shows the two forms of the dependency line.

### `MockHost`

`MockHost::new()` gives one value that implements `ChainHost`, `LocalStoreHost`, and `LoggingHost`.
Its three public fields are the mock for each seam: `host.chain` is a `MockChain`, `host.store` is a `MockLocalStore`, and `host.logging` is a `MockLogging`.
A test programs and inspects the mocks through the fields, and it passes `&host` to the logic.

The mock has no manifest.
Every seam answers whether or not `component.toml` declares it.
The check that a module uses only what it declares is the `#[nexum_sdk::module]` build, not a `MockHost` test.

### `MockChain`

`MockChain` answers `request` from a map and records every call.

```rust
use nexum_sdk::chain::eth_call_params;
use nexum_sdk::host::{ChainError, Fault};

let params = eth_call_params(&oracle, &call_data);
host.chain.respond_to("eth_call", &params, Ok("\"0x...\"".into()));
host.chain.respond_to("eth_getBalance", "[\"0xabc...\",\"latest\"]", Err(ChainError::Fault(Fault::Timeout)));

assert_eq!(host.chain.call_count(), 1);
let last = host.chain.last_call().unwrap();
```

- The map key is the exact `(method, params)` pair, so build `params` with the same code the module uses.
- A response is the JSON text of the result, which for a hex value includes the quotes.
- A call with no programmed response returns `Fault::Unsupported` with a message that names the method and the params.
- `calls()` gives every call in order, with its `chain_id`, `method`, and `params`.

### `MockLocalStore`

`MockLocalStore` is an in-memory store with the limits and the failures of the real one.

| Method | Use |
|---|---|
| `snapshot()` | Read every row, for an assertion. |
| `len()`, `is_empty()` | Count the rows. |
| `namespaced(name)` | A view under another namespace, to test isolation. |
| `set_max_entries(n)` | A `set` of a new key fails once the store holds `n` rows. |
| `set_max_bytes(n)` | A `set` fails once the stored bytes would pass `n`. |
| `set_max_page(n)` | One `list_entries` page returns at most `n` rows, as the host's cap does. |
| `fail_on(prefix, fault)` | Every operation on a key under `prefix` returns `fault`. |

```rust
host.store.fail_on("balance:", Fault::Unavailable("disk".into()));
assert!(host.set("balance:0xabc", b"1").is_err());
```

### `MockLogging` and `capture_tracing`

A module has two log paths, and each has its own capture.

A `tracing` macro goes to the `tracing` subscriber.
`capture_tracing` runs a closure and returns its value with every event the closure emitted on the calling thread:

```rust
use nexum_sdk::Level;
use nexum_sdk_test::capture_tracing;

let (result, logs) = capture_tracing(|| on_block(&host, 11_155_111, &settings, 100));
result.unwrap();

let ev = logs.expect_one(|e| e.level == Level::WARN);
assert_eq!(ev.message, "price-alert: THRESHOLD CROSSED");
assert_eq!(ev.field_str("direction").as_deref(), Some("Below"));
```

- `expect_one(pred)` returns the single matching event and panics with the full capture otherwise.
- `count_at(level)`, `any(pred)`, `is_empty()`, and `events()` cover the other assertions.
- An event carries its `level`, `target`, `message`, and `fields`.
  `field(name)` gives the typed value and `field_str(name)` gives its text.
- The capture is per thread, so tests in parallel do not see each other's events.

A direct `host.log(level, message)` call goes to `host.logging`.
The SDK helper `chain::chainlink::read_latest_answer` logs this way.
`host.logging.contains("eth_call failed")`, `count_at(level)`, and `lines()` read it back.

### `TrapStore`

A module can trap between two writes, and the store then holds the first write without the second.
`TrapStore` wraps any `LocalStoreHost` and simulates this.

```rust
use nexum_sdk::host::{Fault, LocalStoreHost};
use nexum_sdk_test::{MockLocalStore, TrapStore};

fn two_writes<H: LocalStoreHost>(host: &H) -> Result<(), Fault> {
    host.set("a", b"1")?;
    host.set("b", b"2")
}

#[test]
fn a_trap_between_two_writes_leaves_the_first() {
    let store = TrapStore::new(MockLocalStore::default());
    store.arm_after(1);

    assert!(two_writes(&store).is_err());
    assert!(store.tripped());
    assert_eq!(store.writes(), 1);

    store.disarm();
    assert_eq!(store.inner().len(), 1);
    two_writes(&store).unwrap();
    assert_eq!(store.inner().len(), 2);
}
```

`arm_after(n)` lets the next `n` writes land.
The write after that trips the trap, and every later operation fails until `disarm`.
Run a flow once for each `n` from zero to its write count, and assert that a second pass recovers from each torn state.
The rule this enforces: no invariant spans two `set` calls unless the state between them heals itself, or the writes go through one `WriteBatch`.

### HTTP

`nexum-sdk-test` has no HTTP mock.
Module logic binds the `nexum_sdk::http::Fetch` trait, and a test passes its own stub that implements it.
The tests in `modules/examples/http-probe/src/logic.rs` show one.

### Run the module tests

```sh
cargo nextest run -p price-alert -p balance-tracker -p http-probe
```

A module in its own repository runs `cargo test`.

## Engine tests: the harness

### Where the pieces are

- `nexum-runtime-testing` holds the seam-level doubles.
  `FakeNode` is an in-process JSON-RPC node, and `MockRpc` is a scripted transport.
  `MockStateStore` is a store with no disk, and `ManualClock` is a clock the test advances.
  `TestManifest` builds a `component.toml`, and `capture_metrics` records the metrics a closure emits.
  `module_wasm_or_skip` finds a built guest artifact.
- `nexum_runtime::test_utils` re-exports all of those and adds the two harnesses that need the engine: `TestRuntime` and `BootScenario`.
  The module exists only under the `test-utils` feature of `nexum-runtime`.

A crate that tests against the harness takes the feature as a dev-dependency:

```toml
[dev-dependencies]
nexum-runtime = { path = "../nexum-runtime", features = ["test-utils"] }
```

A test crate that needs only the doubles depends on `nexum-runtime-testing` directly: see [ADR-0023](adr/0023-crate-decomposition.md).

### Build the guest wasms first

The harness loads real components from `target/wasm32-wasip2/release`.

```sh
just build
```

A missing artifact fails the test.
`NEXUM_ALLOW_MISSING_WASM=1` turns each such test into a skip.
The skip is opt-in because a skipped run reports the same counts as a real one.

### `TestRuntime`

`TestRuntime` launches modules through the public `RuntimeBuilder` path over the mocks and runs the real event loop.

```rust
use alloy_rpc_types_eth::Header;
use nexum_runtime::test_utils::{TestRuntime, manifest, module_wasm_or_skip};

#[tokio::test]
async fn example_logs_an_injected_block() {
    let Some(wasm) = module_wasm_or_skip("example") else {
        return;
    };

    let mut rt = TestRuntime::builder(wasm)
        .module(manifest("example").cap("logging").block_trigger(1))
        .launch()
        .await
        .expect("launch the example module");

    let mut header: Header = Header::default();
    header.inner.number = 42;
    rt.push_block(header);

    rt.wait_for_log("example", "block 42 on chain 1")
        .await
        .expect("the module logged the block");

    rt.shutdown();
    rt.wait().await.expect("clean shutdown");
}
```

- The harness serves chain id 1, so declare the triggers on chain 1.
- Dispatch runs on the event-loop task.
  Push an input, then await an effect: `wait_for_log` resolves when a matching record lands, and it fails after 5 seconds.
  Do not assume the dispatch is done when `push_block` returns.
- `manifest(name)` builds the manifest through the real TOML types, so a test cannot assert against text the parser would refuse.
  `.module(...)` also accepts a TOML string or a path.

The builder and the launched handle share the same mocks.

| Call | Use |
|---|---|
| `rt.push_block(header)` | Deliver a block to every `block` trigger. |
| `rt.push_chain_log(log)` | Deliver a log to the `event` triggers. |
| `.configure_chain(\|node\| { node.on_method(ChainMethod::EthCall, json); })` | Program the response to a `chain.request` method before launch. |
| `rt.chain()` | The `FakeNode`: `recorded_requests`, `fail_head_fetches`, `delay_next_request`. |
| `rt.store()` | The `MockStateStore` the modules write to. |
| `rt.clock()` | The `ManualClock` behind the guest's WASI clocks: `set` and `advance`. |
| `rt.logs()` | The log pipeline, for a read that `wait_for_log` does not cover. |
| `.limits(...)`, `.policy(...)`, `.require_digest()` | Set the `[limits]`, the `[policy]`, and the digest requirement the launch resolves. |
| `.extension(...)` | Register an extension. |

`ChainMethod` is `nexum_world::ChainMethod`.

### Dispatch without the event loop

`boot_supervisor` boots through the same admission path and returns the supervisor, with no event loop.
The test then dispatches directly, so there is nothing to wait for.

```rust
let mut booted = TestRuntime::builder(wasm)
    .module(manifest("example").cap("logging").block_trigger(1))
    .boot_supervisor()
    .await
    .expect("boot");

assert_eq!(booted.dispatch_block_on(1).await, 1);
assert!(
    booted
        .records("example")
        .iter()
        .any(|record| record.message.contains("block 19000000 on chain 1")),
);
```

`dispatch_block_on` returns the number of modules the block reached.

### `BootScenario`

`BootScenario` tests the boot itself: what the engine admits and what it refuses.
`core_scenario()` gives one over the core runtime.

```rust
use nexum_runtime::error::BootRefusal;
use nexum_runtime::test_utils::{core_scenario, manifest};

core_scenario()
    .module(manifest("example").cap("logging").block_trigger(424_242))
    .expect_refusal()
    .await
    .variant::<BootRefusal>(|e| {
        matches!(e, BootRefusal::UnconfiguredChain { chain_id: 424_242, .. })
    })
    .names("[chains.424242]");
```

`expect_refusal` returns a `Refusal`.
`variant` asserts on the typed error, and `names` and `lacks` assert on the operator wording.
The scenario configures chains 1, 100, and 11155111, so a trigger on any other chain refuses.

### Run the engine tests

```sh
just build
just test
```

`just test` runs the engine crates through nextest.
`just ci` runs the whole CI series, which adds the lints, the docs build, the module tests, and the doctests.
