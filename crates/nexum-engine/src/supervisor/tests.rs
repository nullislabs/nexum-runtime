use std::path::{Path, PathBuf};

use super::*;
use crate::engine_config::ModuleLimits;

#[test]
fn empty_supervisor_returns_no_subscriptions() {
    let sup = Supervisor {
        modules: Vec::new(),
    };
    assert!(sup.block_chains().is_empty());
    assert!(sup.log_subscriptions().is_empty());
    assert_eq!(sup.module_count(), 0);
}

/// Regression guard: engines whose modules only declare
/// `[[subscription]] kind = "block"` (or only `kind = "log"`) must not
/// bail at boot. Previously `select_all` on an empty `Vec` yielded
/// `None` immediately and the "stream ended -> shut down" arm fired
/// before any event flowed. The fix in `runtime/event_loop.rs`
/// substitutes `stream::pending()` when the Vec is empty so the
/// corresponding select arm is never selected.
///
/// Surfaced when wiring up `engine.m3.toml` for the M3 testnet runbook:
/// the 3 M3 example modules (price-alert, balance-tracker, stop-loss)
/// all subscribe to blocks only, no logs. The engine bailed within
/// ~50 ms of `supervisor ready` until this fix landed.
#[tokio::test]
async fn run_does_not_bail_when_both_stream_kinds_are_empty() {
    use std::time::{Duration, Instant};

    let mut supervisor = Supervisor {
        modules: Vec::new(),
    };
    let started = Instant::now();
    let shutdown = tokio::time::sleep(Duration::from_millis(50));

    crate::runtime::event_loop::run(&mut supervisor, Vec::new(), Vec::new(), shutdown).await;

    // If the bug were present, `run` returns ~0 ms (the empty `logs`
    // stream's first `.next()` yields `None` and the loop bails on
    // the bail-on-None arm). With the fix, `run` blocks on `shutdown`
    // for the full 50 ms.
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(40),
        "run returned in {elapsed:?}, expected >= ~50ms (shutdown timer)",
    );
}

// ── E2E helpers ───────────────────────────────────────────────────────

/// Path to the pre-built example WASM component. Tests that need it
/// call `example_wasm_or_skip()` which skips gracefully if absent.
fn example_wasm() -> PathBuf {
    // CARGO_MANIFEST_DIR → crates/nexum-engine
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target/wasm32-wasip2/release/example.wasm")
}

fn example_module_toml() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("modules/example/module.toml")
}

/// Returns `None` and prints a skip message if the fixture isn't built.
fn example_wasm_or_skip() -> Option<PathBuf> {
    let p = example_wasm();
    if p.exists() {
        Some(p)
    } else {
        eprintln!(
            "SKIP: {} not found - run `just build-module` to enable E2E tests",
            p.display()
        );
        None
    }
}

fn make_wasmtime_engine() -> wasmtime::Engine {
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    wasmtime::Engine::new(&config).expect("wasmtime engine")
}

fn make_linker(engine: &wasmtime::Engine) -> Linker<crate::HostState> {
    let mut linker = Linker::<crate::HostState>::new(engine);
    crate::Shepherd::add_to_linker::<
        crate::HostState,
        wasmtime::component::HasSelf<crate::HostState>,
    >(&mut linker, |s| s)
    .expect("add_to_linker");
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).expect("add_wasi");
    linker
}

/// Return `(dir, store)` so the test holds the `TempDir` for the
/// duration of the test scope and cleans it up on drop. Forgetting
/// the dir (the old `ManuallyDrop` approach) leaks it for the
/// entire process lifetime.
fn temp_local_store() -> (tempfile::TempDir, crate::host::local_store_redb::LocalStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ls.redb");
    let store = crate::host::local_store_redb::LocalStore::open(path).expect("local store");
    (dir, store)
}

// ── E2E tests ─────────────────────────────────────────────────────────

/// Boot supervisor with the example module; verify it starts alive.
#[tokio::test]
async fn e2e_supervisor_boots_example_module() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let cow_pool = crate::host::cow_orderbook::OrderBookPool::default();
    let provider_pool = crate::host::provider_pool::ProviderPool::empty();
    let (_dir, local_store) = temp_local_store();

    let limits = ModuleLimits::default();
    let supervisor = Supervisor::boot_single(
        &engine,
        &linker,
        &wasm,
        Some(example_module_toml()).as_deref(),
        &cow_pool,
        &provider_pool,
        &local_store,
        &limits,
    )
    .await
    .expect("boot_single");

    assert_eq!(supervisor.module_count(), 1);
    assert_eq!(supervisor.alive_count(), 1);
}

/// Boot with a manifest that subscribes to block events; dispatch one
/// block event and verify the module was invoked and stayed alive.
#[tokio::test]
async fn e2e_block_subscription_dispatched() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        r#"
[module]
name = "example"

[capabilities]
required = ["logging"]

[[subscription]]
kind     = "block"
chain_id = 1
"#,
    )
    .unwrap();

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let cow_pool = crate::host::cow_orderbook::OrderBookPool::default();
    let provider_pool = crate::host::provider_pool::ProviderPool::empty();
    let (_dir, local_store) = temp_local_store();
    let limits = ModuleLimits::default();

    let mut supervisor = Supervisor::boot_single(
        &engine,
        &linker,
        &wasm,
        Some(&manifest),
        &cow_pool,
        &provider_pool,
        &local_store,
        &limits,
    )
    .await
    .expect("boot_single");

    let block = nexum::host::types::Block {
        chain_id: 1,
        number: 19_000_000,
        hash: vec![0xab; 32],
        timestamp: 1_700_000_000_000,
    };
    let dispatched = supervisor.dispatch_block(block).await;
    assert_eq!(dispatched, 1, "one module subscribed to chain 1 blocks");
    assert_eq!(supervisor.alive_count(), 1, "module must remain alive");
}

// ── COW-1068: production module integration tests ────────────────────
//
// One test per module that goes through the real wit-bindgen +
// WitBindgenHost adapter + supervisor dispatch path, not just the
// strategy-level MockHost coverage. Mirrors the example-module e2e
// shape above; each test is guarded by `module_wasm_or_skip()` so
// local runs without a fresh `--target wasm32-wasip2 --release`
// build are skipped rather than failing.

const SEPOLIA: u64 = 11_155_111;

/// Path to a production module's .wasm artefact under the workspace
/// target dir. `Cargo` writes the artefact as `<name>.wasm` with
/// hyphens replaced by underscores, so the helper mirrors that.
fn module_wasm(module_name: &str) -> PathBuf {
    let artifact = module_name.replace('-', "_");
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("target/wasm32-wasip2/release/{artifact}.wasm"))
}

fn module_wasm_or_skip(module_name: &str) -> Option<PathBuf> {
    let p = module_wasm(module_name);
    if p.exists() {
        Some(p)
    } else {
        eprintln!(
            "SKIP: {} not found - build with `cargo build -p {module_name} --target wasm32-wasip2 --release`",
            p.display()
        );
        None
    }
}

/// Resolve a real `module.toml` for one of the production modules.
/// Looking up the real manifest (rather than synthesising one) keeps
/// the integration test honest about the capability set + subscription
/// shape each module actually ships.
fn production_module_toml(relative_path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(relative_path)
}

fn synthetic_sepolia_block() -> nexum::host::types::Block {
    nexum::host::types::Block {
        chain_id: SEPOLIA,
        number: 19_000_000,
        hash: vec![0xab; 32],
        timestamp: 1_700_000_000_000,
    }
}

/// Boot a single module from `(wasm, manifest)` and return the live
/// supervisor. Shared body across the 5 integration tests.
async fn boot_production_module(
    engine: &wasmtime::Engine,
    linker: &Linker<crate::HostState>,
    local_store: &crate::host::local_store_redb::LocalStore,
    wasm: &Path,
    manifest: &Path,
) -> Supervisor {
    let cow_pool = crate::host::cow_orderbook::OrderBookPool::default();
    let provider_pool = crate::host::provider_pool::ProviderPool::empty();
    Supervisor::boot_single(
        engine,
        linker,
        wasm,
        Some(manifest),
        &cow_pool,
        &provider_pool,
        local_store,
    )
    .await
    .expect("boot_single")
}

#[tokio::test]
async fn e2e_twap_monitor_block_dispatch() {
    let Some(wasm) = module_wasm_or_skip("twap-monitor") else {
        return;
    };
    let manifest = production_module_toml("modules/twap-monitor/module.toml");
    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, store) = temp_local_store();

    let mut supervisor = boot_production_module(&engine, &linker, &store, &wasm, &manifest).await;
    assert_eq!(supervisor.module_count(), 1);
    assert_eq!(supervisor.alive_count(), 1);

    // twap-monitor subscribes to Sepolia blocks (poll path). A real
    // poll would call chain::request, which ProviderPool::empty() does
    // not satisfy - the module surfaces a host-error and warns; the
    // supervisor must keep the module alive because the strategy
    // catches the error and returns Ok(()).
    let dispatched = supervisor.dispatch_block(synthetic_sepolia_block()).await;
    assert_eq!(dispatched, 1);
    assert_eq!(supervisor.alive_count(), 1);
}

#[tokio::test]
async fn e2e_ethflow_watcher_log_dispatch() {
    let Some(wasm) = module_wasm_or_skip("ethflow-watcher") else {
        return;
    };
    let manifest = production_module_toml("modules/ethflow-watcher/module.toml");
    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, store) = temp_local_store();

    let mut supervisor = boot_production_module(&engine, &linker, &store, &wasm, &manifest).await;
    assert_eq!(supervisor.alive_count(), 1);

    // A log with an unrecognised topic is silently skipped by the
    // module's decoder (returns `None` from `decode_order_placement`),
    // so the test only proves: supervisor delivered, module did not
    // trap, module stayed alive. Stronger asserts (submitted:{uid}
    // markers etc.) require a hand-crafted ABI-encoded OrderPlacement
    // payload and the real ETH_FLOW_PRODUCTION address, deferred to
    // COW-1064 testnet integration.
    let synthetic_log = alloy_rpc_types_eth::Log::default();
    let dispatched = supervisor
        .dispatch_log("ethflow-watcher", SEPOLIA, synthetic_log)
        .await;
    assert!(dispatched);
    assert_eq!(supervisor.alive_count(), 1);
}

#[tokio::test]
async fn e2e_price_alert_block_dispatch() {
    let Some(wasm) = module_wasm_or_skip("price-alert") else {
        return;
    };
    let manifest = production_module_toml("modules/examples/price-alert/module.toml");
    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, store) = temp_local_store();

    let mut supervisor = boot_production_module(&engine, &linker, &store, &wasm, &manifest).await;
    let dispatched = supervisor.dispatch_block(synthetic_sepolia_block()).await;
    assert_eq!(dispatched, 1);
    assert_eq!(supervisor.alive_count(), 1);
}

#[tokio::test]
async fn e2e_balance_tracker_block_dispatch() {
    let Some(wasm) = module_wasm_or_skip("balance-tracker") else {
        return;
    };
    let manifest = production_module_toml("modules/examples/balance-tracker/module.toml");
    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, store) = temp_local_store();

    let mut supervisor = boot_production_module(&engine, &linker, &store, &wasm, &manifest).await;
    let dispatched = supervisor.dispatch_block(synthetic_sepolia_block()).await;
    assert_eq!(dispatched, 1);
    assert_eq!(supervisor.alive_count(), 1);
}

#[tokio::test]
async fn e2e_stop_loss_block_dispatch() {
    let Some(wasm) = module_wasm_or_skip("stop-loss") else {
        return;
    };
    let manifest = production_module_toml("modules/examples/stop-loss/module.toml");
    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, store) = temp_local_store();

    let mut supervisor = boot_production_module(&engine, &linker, &store, &wasm, &manifest).await;
    let dispatched = supervisor.dispatch_block(synthetic_sepolia_block()).await;
    assert_eq!(dispatched, 1);
    assert_eq!(supervisor.alive_count(), 1);
}

// ── build_alloy_filter ────────────────────────────────────────────────

#[test]
fn alloy_filter_with_address_and_topic() {
    let addr = "0xC92E8bdf79f0507f65a392b0ab4667716BFE0110";
    let topic = "0x237e158222e3e6968b72b9db0d8043aacf074ad9f650f0d1606b4d82ee432c00";
    let filter = build_alloy_filter(Some(addr), Some(topic)).unwrap();
    // Check address is set (alloy Filter doesn't expose a simple getter,
    // but we can verify the filter serialises the address field).
    let serialised = serde_json::to_value(&filter).unwrap();
    let addr_field = serialised
        .get("address")
        .unwrap()
        .to_string()
        .to_lowercase();
    assert!(addr_field.contains(&addr.to_lowercase()[2..])); // strip 0x
}

#[test]
fn alloy_filter_no_address_no_topic() {
    let filter = build_alloy_filter(None, None).unwrap();
    let serialised = serde_json::to_value(&filter).unwrap();
    // Address and topics should be absent or null.
    assert!(
        serialised.get("address").is_none()
            || serialised["address"].is_null()
            || serialised["address"] == serde_json::json!([])
    );
}

#[test]
fn alloy_filter_rejects_bad_address() {
    let err = build_alloy_filter(Some("not-an-address"), None);
    assert!(err.is_err());
}

#[test]
fn alloy_filter_rejects_bad_topic() {
    let addr = "0xC92E8bdf79f0507f65a392b0ab4667716BFE0110";
    let err = build_alloy_filter(Some(addr), Some("not-a-topic"));
    assert!(err.is_err());
}
