//! End-to-end runs of real wasm modules through boot and dispatch.

use super::*;

/// Boot supervisor with the example module; verify it starts alive.
#[tokio::test]
async fn e2e_supervisor_boots_example_module() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, local_store) = temp_local_store();
    let components = test_components(local_store);

    let limits = ModuleLimits::default();
    let supervisor = Supervisor::boot_single(
        &engine,
        &linker,
        &wasm,
        Some(example_module_toml()).as_deref(),
        &components,
        &limits,
        &test_chains(),
        false,
        &core_extensions(),
        None,
    )
    .await
    .expect("boot_single");

    assert_eq!(supervisor.module_count(), 1);
    assert_eq!(supervisor.alive_count(), 1);
}

/// The example component's capability-bearing imports are exactly what its
/// manifest declares (`logging`).
#[test]
fn e2e_example_component_imports_equal_declared_capabilities() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let engine = make_wasmtime_engine();
    let component = wasmtime::component::Component::from_file(&engine, &wasm).expect("compile");
    let imports: Vec<String> = component
        .component_type()
        .imports(&engine)
        .map(|(name, _)| name.to_owned())
        .collect();

    // Capability-bearing imports resolve to exactly the declared set.
    let registry = CapabilityRegistry::core();
    let caps: std::collections::BTreeSet<&str> = imports
        .iter()
        .filter_map(|name| registry.wit_import_to_cap(name))
        .collect();
    assert_eq!(
        caps,
        std::collections::BTreeSet::from(["logging"]),
        "imports were: {imports:?}"
    );

    // No extension interface leaks in either: the per-module world holds
    // exactly what the manifest declared.
    assert!(
        imports
            .iter()
            .all(|name| name.starts_with("nexum:host/") || name.starts_with("wasi:")),
        "imports were: {imports:?}"
    );
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
    let (_dir, local_store) = temp_local_store();
    let components = test_components(local_store);
    let limits = ModuleLimits::default();

    let mut supervisor = Supervisor::boot_single(
        &engine,
        &linker,
        &wasm,
        Some(&manifest),
        &components,
        &limits,
        &test_chains(),
        false,
        &core_extensions(),
        None,
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

/// A `ManualClock` override threads through `boot_single` onto the module
/// store and is behaviour-neutral: the module boots, dispatches a block,
/// and stays alive as on the ambient clock.
#[cfg(feature = "test-utils")]
#[tokio::test]
async fn e2e_manual_clock_override_boots_and_dispatches() {
    use std::time::{Duration, UNIX_EPOCH};

    use crate::test_utils::clock::ManualClock;

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
    let (_dir, local_store) = temp_local_store();
    let components = test_components(local_store);
    let limits = ModuleLimits::default();

    let clock = ManualClock::new();
    clock.set(UNIX_EPOCH + Duration::from_secs(1_700_000_000));

    let mut supervisor = Supervisor::boot_single(
        &engine,
        &linker,
        &wasm,
        Some(&manifest),
        &components,
        &limits,
        &test_chains(),
        false,
        &core_extensions(),
        Some(clock.as_override()),
    )
    .await
    .expect("boot_single with a manual clock override");

    let block = nexum::host::types::Block {
        chain_id: 1,
        number: 19_000_000,
        hash: vec![0xab; 32],
        timestamp: 1_700_000_000_000,
    };
    let dispatched = supervisor.dispatch_block(block).await;
    assert_eq!(dispatched, 1, "the overridden-clock module dispatched");
    assert_eq!(supervisor.alive_count(), 1, "module must remain alive");

    // Advancing the shared handle is observable on the same source the store
    // reads; the boot path did not clone away from it.
    clock.advance(Duration::from_secs(1));
    assert_eq!(
        wasmtime_wasi::HostWallClock::now(&clock),
        Duration::from_secs(1_700_000_001),
    );
}

// ── Production module integration tests ────────────────────
//
// One test per module that goes through the real wit-bindgen +
// WitBindgenHost adapter + supervisor dispatch path, not just the
// module-level MockHost coverage. Mirrors the example-module e2e
// shape above; each test is guarded by `module_wasm_or_skip()` so
// local runs without a fresh `--target wasm32-wasip2 --release`
// build are skipped rather than failing.

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

/// End-to-end wasi:http path: http-probe fetches a loopback server on its
/// allowlist, then an off-list host that the gate denies before any
/// connection. The guest returns `Ok` only when both legs hold, so
/// `dispatched == 1` asserts the allow and deny paths together.
#[tokio::test]
async fn e2e_http_probe_allowlisted_fetch_and_denied_path() {
    let Some(wasm) = module_wasm_or_skip("http-probe") else {
        return;
    };
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/status"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        format!(
            r#"
[module]
name = "http-probe"

[capabilities]
required = ["logging", "http"]

[capabilities.http]
allow = ["127.0.0.1"]

[[subscription]]
kind     = "block"
chain_id = 1

[config]
probe_url  = "{}/status"
denied_url = "http://denied.invalid/"
"#,
            server.uri(),
        ),
    )
    .unwrap();

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_store_dir, store) = temp_local_store();

    let mut supervisor = boot_production_module(&engine, &linker, &store, &wasm, &manifest).await;
    let block = nexum::host::types::Block {
        chain_id: 1,
        number: 19_000_000,
        hash: vec![0xab; 32],
        timestamp: 1_700_000_000_000,
    };
    let dispatched = supervisor.dispatch_block(block).await;
    assert_eq!(
        dispatched, 1,
        "both http-probe legs (allowlisted fetch + denied off-list fetch) must succeed",
    );
    assert_eq!(supervisor.alive_count(), 1);
}

// ── Log pipeline ─────────────────────────────────────────────
//
// The typed pipeline captures from three points: the
// nexum:host/logging glue (HostInterface), the per-store
// stdout/stderr pipes (Stdout/Stderr), and the supervisor death
// path (Panic). These E2E tests prove a real run leaves retrievable
// records and that a dying run leaves a Panic record, both read back
// through the embedder-facing LogPipeline handle. Stdout/Stderr line
// splitting is covered at the unit level on the StdioStream writer.

/// Components plus a retained clone of the log pipeline so a test can
/// read runs and records back after dispatch.
fn components_with_logs(
    store: crate::host::local_store_redb::LocalStore,
) -> (Components<TestTypes>, crate::host::logs::LogPipeline) {
    let logs = crate::test_utils::in_memory_logs();
    let components = Components {
        chain: ProviderPool::empty(),
        store,
        ext: (),
        logs: logs.clone(),
    };
    (components, logs)
}

/// The example module logs via the host logging glue at init and on the
/// block, so its run holds retrievable HostInterface records after one
/// dispatch. Driven through the [`TestRuntime`] harness.
#[tokio::test]
async fn host_interface_records_are_retrievable_after_a_run() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };

    let mut rt = crate::test_utils::TestRuntime::builder(wasm)
        .manifest_inline(
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
        .launch()
        .await
        .expect("launch example over the harness");

    let mut header: alloy_rpc_types_eth::Header = alloy_rpc_types_eth::Header::default();
    header.inner.number = 19_000_000;
    rt.push_block(header);

    // The polled log read doubles as the dispatch barrier: the on_event line
    // only lands once the event loop has dispatched the injected block.
    rt.wait_for_log("example", "block 19000000")
        .await
        .expect("the on_event log line lands after dispatch");

    let runs = rt.logs().list_runs("example");
    assert_eq!(runs.len(), 1, "one run recorded for the example module");
    let run = runs[0].run.clone();
    assert_eq!(run.seq, 0, "the first run is sequence 0");
    let page = rt.logs().read(&run, 0);
    assert!(!page.records.is_empty(), "run left retrievable records");
    assert!(
        page.records
            .iter()
            .all(|r| r.source == LogSource::HostInterface),
        "the example module logs only through the host interface",
    );
    assert!(
        page.records
            .iter()
            .any(|r| r.message.contains("block 19000000")),
        "the on_event log line is retained",
    );

    rt.shutdown();
    rt.wait().await.expect("clean shutdown");
}

#[tokio::test]
async fn dying_run_leaves_a_panic_record() {
    let Some(wasm) = module_wasm_or_skip("fuel-bomb") else {
        return;
    };
    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, store) = temp_local_store();
    let (components, logs) = components_with_logs(store);
    let manifest = fixture_module_toml("modules/fixtures/fuel-bomb/module.toml");
    let limits = ModuleLimits::default();
    let mut supervisor = Supervisor::boot_single(
        &engine,
        &linker,
        &wasm,
        Some(&manifest),
        &components,
        &limits,
        &test_chains(),
        false,
        &core_extensions(),
        None,
    )
    .await
    .expect("boot_single");

    let block = nexum::host::types::Block {
        chain_id: 1,
        number: 1,
        hash: vec![0; 32],
        timestamp: 1_700_000_000_000,
    };
    // fuel-bomb traps on the first event; the supervisor synthesizes a
    // Panic record on the dead run.
    assert_eq!(
        supervisor.dispatch_block(block).await,
        0,
        "the bomb trapped"
    );

    let runs = logs.list_runs("fuel-bomb");
    assert_eq!(runs.len(), 1);
    let page = logs.read(&runs[0].run, 0);
    let panic = page
        .records
        .iter()
        .find(|r| r.source == LogSource::Panic)
        .expect("a panic record on the dead run");
    assert_eq!(panic.level, Level::ERROR);
    assert!(panic.message.contains("terminated"));
    assert_eq!(
        panic.message.lines().count(),
        1,
        "the panic record carries the trap's root cause, not the frame list",
    );
}

#[tokio::test]
async fn facade_panic_leaves_stderr_host_interface_and_panic_records() {
    let Some(wasm) = module_wasm_or_skip("panic-bomb") else {
        return;
    };
    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, store) = temp_local_store();
    let (components, logs) = components_with_logs(store);
    let manifest = fixture_module_toml("modules/fixtures/panic-bomb/module.toml");
    let limits = ModuleLimits::default();
    let mut supervisor = Supervisor::boot_single(
        &engine,
        &linker,
        &wasm,
        Some(&manifest),
        &components,
        &limits,
        &test_chains(),
        false,
        &core_extensions(),
        None,
    )
    .await
    .expect("boot_single");

    let block = nexum::host::types::Block {
        chain_id: 1,
        number: 1,
        hash: vec![0; 32],
        timestamp: 1_700_000_000_000,
    };
    assert_eq!(
        supervisor.dispatch_block(block).await,
        0,
        "the bomb panicked"
    );

    // The facade panic hook writes to stderr and reports over the host
    // logging call before the trap surfaces, and the supervisor
    // synthesizes the death record: one dead run, three capture points.
    let runs = logs.list_runs("panic-bomb");
    assert_eq!(runs.len(), 1);
    let page = logs.read(&runs[0].run, 0);
    let find = |source: LogSource, needle: &str| {
        page.records
            .iter()
            .find(|r| r.source == source && r.message.contains(needle))
    };
    let stderr = find(LogSource::Stderr, "detonated").expect("the hook's stderr line was captured");
    assert_eq!(stderr.level, Level::WARN, "stderr copy is warn");
    let host =
        find(LogSource::HostInterface, "detonated").expect("the hook's sink call was captured");
    assert_eq!(host.level, Level::ERROR, "sink copy is error");
    let death =
        find(LogSource::Panic, "terminated").expect("the supervisor synthesized the death record");
    assert_eq!(death.level, Level::ERROR, "death record is error");
}
