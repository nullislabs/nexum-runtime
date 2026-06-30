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
