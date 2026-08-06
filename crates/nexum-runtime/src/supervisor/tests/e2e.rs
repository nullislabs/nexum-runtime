//! End-to-end runs of real wasm modules through boot and dispatch.

use super::*;

#[tokio::test]
async fn e2e_supervisor_boots_example_module() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let booted = BootScenario::new()
        .wasm(wasm)
        .module(workspace_manifest("modules/example/module.toml"))
        .boot()
        .await
        .expect("boot");
    assert_eq!(booted.supervisor.module_count(), 1);
    assert_eq!(booted.supervisor.alive_count(), 1);
}

/// The example component's capability-bearing imports are exactly what its
/// manifest declares (`logging`).
#[test]
fn e2e_example_component_imports_equal_declared_capabilities() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let engine = test_wasmtime_engine();
    let component = wasmtime::component::Component::from_file(&engine, &wasm).expect("compile");
    let imports: Vec<String> = component
        .component_type()
        .imports(&engine)
        .map(|(name, _)| name.to_owned())
        .collect();

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

    // No extension interface leaks in either.
    assert!(
        imports
            .iter()
            .all(|name| name.starts_with("nexum:host/") || name.starts_with("wasi:")),
        "imports were: {imports:?}"
    );
}

#[tokio::test]
async fn e2e_block_subscription_dispatched() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let mut booted = BootScenario::new()
        .wasm(wasm)
        .module(TestManifest::new("example").cap("logging").block_sub(1))
        .boot()
        .await
        .expect("boot");

    assert_eq!(
        booted.dispatch_block_on(1).await,
        1,
        "one module subscribed to chain 1 blocks",
    );
    assert_eq!(
        booted.supervisor.alive_count(),
        1,
        "module must remain alive"
    );
}

/// The override is behaviour-neutral here; guest observation of the pinned
/// time is covered by the scenario clock test.
#[cfg(feature = "test-utils")]
#[tokio::test]
async fn e2e_manual_clock_override_boots_and_dispatches() {
    use std::time::{Duration, UNIX_EPOCH};

    use crate::test_utils::clock::ManualClock;

    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = TestManifest::new("example")
        .cap("logging")
        .block_sub(1)
        .write_to(dir.path());

    let clock = ManualClock::new();
    clock.set(UNIX_EPOCH + Duration::from_secs(1_700_000_000));

    let (_store, result) =
        try_boot_single(&wasm, Some(&manifest), false, Some(clock.as_override())).await;
    let mut supervisor = result.expect("boot_single with a manual clock override");
    assert_eq!(
        supervisor.dispatch_block(block_on(1)).await,
        1,
        "the overridden-clock module dispatched",
    );
    assert_eq!(supervisor.alive_count(), 1, "module must remain alive");
}

async fn production_module_dispatches(module: &str, manifest: &str) {
    let Some(wasm) = module_wasm_or_skip(module) else {
        return;
    };
    let mut booted = BootScenario::new()
        .wasm(wasm)
        .module(workspace_manifest(manifest))
        .boot()
        .await
        .expect("boot");
    assert_eq!(
        booted.dispatch_block_on(SEPOLIA).await,
        1,
        "{module} took the dispatch",
    );
    assert_eq!(booted.supervisor.alive_count(), 1);
}

#[tokio::test]
async fn e2e_price_alert_block_dispatch() {
    production_module_dispatches("price-alert", "modules/examples/price-alert/module.toml").await;
}

#[tokio::test]
async fn e2e_balance_tracker_block_dispatch() {
    production_module_dispatches(
        "balance-tracker",
        "modules/examples/balance-tracker/module.toml",
    )
    .await;
}

/// The guest returns `Ok` only when both the allow and deny legs hold, so
/// `dispatched == 1` asserts both paths together.
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

    let mut booted = BootScenario::new()
        .wasm(wasm)
        .module(
            TestManifest::new("http-probe")
                .cap("logging")
                .cap("http")
                .http_allow("127.0.0.1")
                .block_sub(1)
                .config("probe_url", format!("{}/status", server.uri()))
                .config("denied_url", "http://denied.invalid/"),
        )
        .boot()
        .await
        .expect("boot");

    assert_eq!(
        booted.dispatch_block_on(1).await,
        1,
        "both http-probe legs (allowlisted fetch + denied off-list fetch) must succeed",
    );
    assert_eq!(booted.supervisor.alive_count(), 1);
}

/// The module logs at init and on the block; stdout/stderr line splitting
/// is covered at the unit level on the StdioStream writer.
#[tokio::test]
async fn host_interface_records_are_retrievable_after_a_run() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };

    let mut rt = crate::test_utils::TestRuntime::builder(wasm)
        .manifest_inline(
            TestManifest::new("example")
                .cap("logging")
                .block_sub(1)
                .to_toml(),
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

/// A trapping run leaves a supervisor-synthesized Panic record carrying
/// the trap's root cause.
#[tokio::test]
async fn dying_run_leaves_a_panic_record() {
    let Some(wasm) = module_wasm_or_skip("fuel-bomb") else {
        return;
    };
    let mut booted = BootScenario::new()
        .wasm(wasm)
        .module(workspace_manifest("modules/fixtures/fuel-bomb/module.toml"))
        .boot()
        .await
        .expect("boot");

    assert_eq!(booted.dispatch_block_on(1).await, 0, "the bomb trapped");

    let runs = booted.logs().list_runs("fuel-bomb");
    assert_eq!(runs.len(), 1);
    let page = booted.logs().read(&runs[0].run, 0);
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
    let mut booted = BootScenario::new()
        .wasm(wasm)
        .module(workspace_manifest(
            "modules/fixtures/panic-bomb/module.toml",
        ))
        .boot()
        .await
        .expect("boot");

    assert_eq!(booted.dispatch_block_on(1).await, 0, "the bomb panicked");

    // One dead run, three capture points: the facade hook's stderr line,
    // its host logging call, and the synthesized death record.
    let runs = booted.logs().list_runs("panic-bomb");
    assert_eq!(runs.len(), 1);
    let page = booted.logs().read(&runs[0].run, 0);
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
