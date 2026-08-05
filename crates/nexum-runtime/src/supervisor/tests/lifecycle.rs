//! Lifecycle: init failure, traps, restart backoff, and poison quarantine.

use super::*;

// ── Init-failed modules must be marked dead ────────────────

/// A module whose `[config]` carries a malformed `threshold` fails `init`
/// with `fault.invalid-input`; the supervisor marks it `alive = false` so
/// it receives no dispatches.
#[tokio::test]
async fn init_failure_marks_module_dead_and_excludes_from_dispatch() {
    let Some(wasm) = module_wasm_or_skip("price-alert") else {
        return;
    };

    // Synthesise a manifest with the same shape as the real
    // price-alert module but with a `threshold` that the module
    // rejects in `parse_config`.
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("module.toml");
    std::fs::write(
        &manifest,
        r#"
[module]
name = "price-alert"

[capabilities]
required = ["logging", "chain"]

[[subscription]]
kind     = "block"
chain_id = 11155111

[config]
oracle_address = "0x694AA1769357215DE4FAC081bf1f309aDC325306"
decimals       = "8"
threshold      = "not-a-number"
direction      = "below"
every_n_blocks = "1"
"#,
    )
    .unwrap();

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, store) = temp_local_store();

    let mut supervisor = boot_production_module(&engine, &linker, &store, &wasm, &manifest).await;

    // The module loaded successfully (wasm compiled, capabilities
    // matched, manifest parsed) but `init` returned InvalidInput.
    assert_eq!(supervisor.module_count(), 1, "module is loaded");
    assert_eq!(
        supervisor.alive_count(),
        0,
        "init-failed module must be marked dead",
    );

    // Dispatch the synthetic block. The init-failed module must
    // not be reached by the dispatcher.
    let dispatched = supervisor.dispatch_block(synthetic_sepolia_block()).await;
    assert_eq!(
        dispatched, 0,
        "no live module is subscribed to chain 11155111 blocks",
    );
}

/// An init-failed (dead) module must not contribute its chain to
/// `block_chains()` or `chain_log_subscriptions()`.
#[tokio::test]
async fn dead_modules_excluded_from_subscription_lists() {
    let Some(wasm) = module_wasm_or_skip("price-alert") else {
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("module.toml");
    // Manifest declares both a block and a chain-log subscription so the
    // test genuinely exercises both filter paths — not just the trivially
    // empty chain_log case of a block-only module.
    std::fs::write(
        &manifest,
        r#"
[module]
name = "price-alert"

[capabilities]
required = ["logging", "chain"]

[[subscription]]
kind     = "block"
chain_id = 11155111

[[subscription]]
kind             = "chain-log"
chain_id         = 11155111
address          = "0xbA3cB449bD2B4ADddBc894D8697F5170800EAdeC"
event_signature  = "0xcf5f9de2984132265203b5c335b25727702ca77262ff622e136baa7362bf1da9"

[config]
oracle_address = "0x694AA1769357215DE4FAC081bf1f309aDC325306"
decimals       = "8"
threshold      = "not-a-number"
direction      = "below"
every_n_blocks = "1"
"#,
    )
    .unwrap();

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, store) = temp_local_store();
    let supervisor = boot_production_module(&engine, &linker, &store, &wasm, &manifest).await;

    assert_eq!(supervisor.alive_count(), 0, "init-failed module is dead");
    assert!(
        supervisor.block_chains().is_empty(),
        "dead module must not contribute to block_chains()",
    );
    assert!(
        supervisor.chain_log_subscriptions().is_empty(),
        "dead module must not contribute to chain_log_subscriptions()",
    );
    assert!(
        supervisor.dead_modules_hold_subscriptions(),
        "the filtered-out subscriptions must be attributed to the dead module",
    );
}

/// Positive control for the alive filter: with one dead and one alive
/// module, the alive module's subscriptions survive the filter.
#[tokio::test]
async fn alive_module_subscriptions_survive_alongside_dead_module() {
    let Some(price_alert_wasm) = module_wasm_or_skip("price-alert") else {
        return;
    };
    let Some(example_wasm) = example_wasm_or_skip() else {
        return;
    };

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, local_store) = temp_local_store();
    let components = test_components(local_store);

    let tmp = tempfile::tempdir().unwrap();
    // price-alert with an unparseable threshold: loads, then init fails.
    let dead_manifest = tmp.path().join("price-alert.toml");
    std::fs::write(
        &dead_manifest,
        r#"
[module]
name = "price-alert"

[capabilities]
required = ["logging", "chain"]

[[subscription]]
kind     = "block"
chain_id = 11155111

[config]
oracle_address = "0x694AA1769357215DE4FAC081bf1f309aDC325306"
decimals       = "8"
threshold      = "not-a-number"
direction      = "below"
every_n_blocks = "1"
"#,
    )
    .unwrap();
    // example module inits fine and subscribes to chain 1 blocks.
    let alive_manifest = tmp.path().join("example.toml");
    std::fs::write(
        &alive_manifest,
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

    let engine_cfg = crate::engine_config::EngineConfig {
        engine: crate::engine_config::EngineSection {
            state_dir: tmp.path().to_path_buf(),
            log_level: "info".into(),
            metrics: crate::engine_config::MetricsSection::default(),
            ..Default::default()
        },
        limits: crate::engine_config::ModuleLimits::default(),
        chains: crate::test_utils::test_chain_configs(),
        defaulted: false,
        extensions: std::collections::HashMap::new(),
        modules: vec![
            crate::engine_config::ModuleEntry {
                path: price_alert_wasm,
                manifest: Some(dead_manifest),
            },
            crate::engine_config::ModuleEntry {
                path: example_wasm,
                manifest: Some(alive_manifest),
            },
        ],
        adapters: Vec::new(),
    };

    let supervisor = Supervisor::boot(
        &engine,
        &linker,
        &engine_cfg,
        &components,
        &core_extensions(),
        None,
    )
    .await
    .expect("boot");

    assert_eq!(supervisor.module_count(), 2);
    assert_eq!(supervisor.alive_count(), 1, "only the example is alive");
    let chains = supervisor.block_chains();
    assert_eq!(
        chains.iter().map(|c| c.id()).collect::<Vec<_>>(),
        vec![1],
        "the alive module's chain survives; the dead module's does not",
    );
    assert!(
        supervisor.dead_modules_hold_subscriptions(),
        "the dead module's dropped subscription is attributable",
    );
}

// ── Resource-limit enforcement tests ───────────────────────
//
// Two evil-by-design fixtures under `modules/fixtures/` exercise the
// per-module fuel + memory caps (DEFAULT_FUEL_PER_EVENT
// + DEFAULT_MEMORY_LIMIT). The tests assert:
//
// 1. The host catches the trap (OutOfFuel / memory-grow rejection)
//    without panicking the supervisor.
// 2. The trapping module is marked dead (alive_count drops to 0 for a
//    single-module supervisor).
// 3. A subsequent dispatch does not re-enter the dead module + the
//    engine itself remains alive (dispatched count is 0, no crash).
//
// Locks the M1 fuel/memory wiring against regression so future
// changes to the supervisor cannot silently bypass the limits.

/// Boot a single fixture (`.wasm` + `module.toml`) under the supervisor.
async fn boot_fixture(wasm: &Path, manifest_relative: &str) -> DefaultSupervisor {
    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, local_store) = temp_local_store();
    let components = test_components(local_store);
    let manifest = fixture_module_toml(manifest_relative);
    let limits = crate::engine_config::ModuleLimits::default();
    Supervisor::boot_single(
        &engine,
        &linker,
        wasm,
        Some(&manifest),
        &components,
        &limits,
        &test_chains(),
        false,
        &core_extensions(),
        None,
    )
    .await
    .expect("boot_single")
}

#[tokio::test]
async fn resource_limit_fuel_bomb_traps_and_marks_module_dead() {
    let Some(wasm) = module_wasm_or_skip("fuel-bomb") else {
        return;
    };
    let mut supervisor = boot_fixture(&wasm, "modules/fixtures/fuel-bomb/module.toml").await;
    assert_eq!(supervisor.module_count(), 1);
    assert_eq!(supervisor.alive_count(), 1, "loads alive");

    // First dispatch enters the fuel-bomb's unbounded loop. wasmtime
    // burns through the per-event fuel budget; the call returns Err
    // (a trap), the supervisor catches it and marks the module dead.
    let block = nexum::host::types::Block {
        chain_id: 1,
        number: 1,
        hash: vec![0; 32],
        timestamp: 1_700_000_000_000,
    };
    let dispatched = supervisor.dispatch_block(block.clone()).await;
    assert_eq!(
        dispatched, 0,
        "fuel-bomb trapped, no module accepted the dispatch",
    );
    assert_eq!(
        supervisor.alive_count(),
        0,
        "fuel-bomb is marked dead after the trap",
    );

    // Engine is still healthy for further dispatches.
    let dispatched_again = supervisor.dispatch_block(block).await;
    assert_eq!(
        dispatched_again, 0,
        "dead module excluded from second dispatch",
    );
}

#[tokio::test]
async fn resource_limit_dead_bomb_does_not_starve_healthy_module() {
    // Strongest assertion of the isolation invariant: load fuel-bomb
    // + the M1 example module side-by-side. After the bomb traps,
    // dispatch a second block and confirm the example module still
    // receives it (dispatched == 1, alive_count == 1 because only
    // one of the two is alive).
    let Some(bomb_wasm) = module_wasm_or_skip("fuel-bomb") else {
        return;
    };
    let Some(example_wasm) = example_wasm_or_skip() else {
        return;
    };

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, local_store) = temp_local_store();
    let components = test_components(local_store);

    // Hand-build an EngineConfig with both modules subscribed to
    // chain 1 blocks. fuel-bomb's manifest already declares the
    // block subscription; the example module needs a synthesised
    // manifest because its on-disk manifest does not subscribe to
    // blocks by default.
    let tmp = tempfile::tempdir().unwrap();
    let example_manifest = tmp.path().join("example.toml");
    std::fs::write(
        &example_manifest,
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

    let engine_cfg = crate::engine_config::EngineConfig {
        engine: crate::engine_config::EngineSection {
            state_dir: tmp.path().to_path_buf(),
            log_level: "info".into(),
            metrics: crate::engine_config::MetricsSection::default(),
            ..Default::default()
        },
        limits: crate::engine_config::ModuleLimits::default(),
        chains: crate::test_utils::test_chain_configs(),
        defaulted: false,
        extensions: std::collections::HashMap::new(),
        modules: vec![
            crate::engine_config::ModuleEntry {
                path: bomb_wasm.clone(),
                manifest: Some(fixture_module_toml(
                    "modules/fixtures/fuel-bomb/module.toml",
                )),
            },
            crate::engine_config::ModuleEntry {
                path: example_wasm.clone(),
                manifest: Some(example_manifest.clone()),
            },
        ],
        adapters: Vec::new(),
    };

    let mut supervisor = Supervisor::boot(
        &engine,
        &linker,
        &engine_cfg,
        &components,
        &core_extensions(),
        None,
    )
    .await
    .expect("boot");

    assert_eq!(supervisor.module_count(), 2);
    assert_eq!(supervisor.alive_count(), 2, "both load alive");

    // First dispatch: fuel-bomb burns through its budget + traps.
    // The example module dispatches normally on the same block. The
    // bomb is now dead.
    let block = nexum::host::types::Block {
        chain_id: 1,
        number: 1,
        hash: vec![0; 32],
        timestamp: 1_700_000_000_000,
    };
    let dispatched = supervisor.dispatch_block(block.clone()).await;
    assert_eq!(
        dispatched, 1,
        "example module received the dispatch even though fuel-bomb trapped",
    );
    assert_eq!(supervisor.alive_count(), 1, "only the example is alive");

    // Second dispatch: only the example accepts; the dead bomb is
    // skipped by the dispatch fast-path.
    let dispatched_again = supervisor.dispatch_block(block).await;
    assert_eq!(dispatched_again, 1);
    assert_eq!(supervisor.alive_count(), 1);
}

#[tokio::test]
async fn resource_limit_memory_bomb_traps_and_marks_module_dead() {
    let Some(wasm) = module_wasm_or_skip("memory-bomb") else {
        return;
    };
    let mut supervisor = boot_fixture(&wasm, "modules/fixtures/memory-bomb/module.toml").await;
    assert_eq!(supervisor.module_count(), 1);
    assert_eq!(supervisor.alive_count(), 1);

    // memory-bomb's on_event allocates 128 MiB which exceeds the
    // 64 MiB DEFAULT_MEMORY_LIMIT; wasmtime rejects the memory.grow
    // and propagates a trap.
    let block = nexum::host::types::Block {
        chain_id: 1,
        number: 1,
        hash: vec![0; 32],
        timestamp: 1_700_000_000_000,
    };
    let dispatched = supervisor.dispatch_block(block.clone()).await;
    assert_eq!(dispatched, 0);
    assert_eq!(supervisor.alive_count(), 0);

    let dispatched_again = supervisor.dispatch_block(block).await;
    assert_eq!(dispatched_again, 0);
}

// ── Supervisor auto-restart with exponential backoff ───────
//
// flaky-bomb traps on the first N events (via wasm `unreachable!`)
// and recovers on event N+1. Exercises the full restart lifecycle:
//
// 1. Dispatch 1: trap -> alive=false, failure_count=1, next_attempt=+1s.
// 2. Immediate redispatch: skipped (next_attempt in the future).
// 3. After 1.1s: alive flipped back on, dispatch retried.
// 4. With fail_first_n=1, the second attempt succeeds -> failure_count
//    resets to 0, next_attempt = None.
//
// Asserts the schedule shape end-to-end with real wall-clock.

#[tokio::test]
async fn restart_flaky_module_recovers_after_backoff() {
    let Some(wasm) = module_wasm_or_skip("flaky-bomb") else {
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("module.toml");
    // fail_first_n = 1 so the module traps once and recovers on the
    // second dispatch attempt. Keeps the test wall-clock under 2 s.
    std::fs::write(
        &manifest,
        r#"
[module]
name = "flaky-bomb"

[capabilities]
required = ["logging", "local-store"]

[[subscription]]
kind     = "block"
chain_id = 1

[config]
fail_first_n = "1"
"#,
    )
    .unwrap();

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, store) = temp_local_store();
    let components = test_components(store);
    let limits = crate::engine_config::ModuleLimits::default();
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
    assert_eq!(supervisor.alive_count(), 1);

    let block = nexum::host::types::Block {
        chain_id: 1,
        number: 1,
        hash: vec![0; 32],
        timestamp: 1_700_000_000_000,
    };

    // Dispatch 1: trap. Module marked dead with a +1s backoff.
    let dispatched = supervisor.dispatch_block(block.clone()).await;
    assert_eq!(dispatched, 0, "first dispatch trapped, no module accepted");
    assert_eq!(supervisor.alive_count(), 0, "module marked dead");

    // Immediate redispatch (under the 1s backoff): still skipped.
    let dispatched_immediate = supervisor.dispatch_block(block.clone()).await;
    assert_eq!(
        dispatched_immediate, 0,
        "in-backoff module not eligible for redispatch yet",
    );
    assert_eq!(supervisor.alive_count(), 0);

    // Wait for the 1s backoff window to elapse (+ a small fudge for
    // scheduler jitter).
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    // Dispatch 3: now eligible. fail_first_n=1 was satisfied on
    // dispatch 1, so this attempt succeeds. The supervisor flips
    // alive back on, dispatch lands, failure_count resets.
    let dispatched_after_backoff = supervisor.dispatch_block(block.clone()).await;
    assert_eq!(
        dispatched_after_backoff, 1,
        "module recovered after the backoff window",
    );
    assert_eq!(supervisor.alive_count(), 1, "recovered + alive");

    // Dispatch 4: steady-state, no backoff in play. Module is happy.
    let dispatched_steady = supervisor.dispatch_block(block).await;
    assert_eq!(dispatched_steady, 1);
}

// ── Poison-pill quarantine ──────────────────────────────────
//
// fuel-bomb traps on every dispatch. With a
// tight poison policy (3 failures / 60 s) we can observe the
// supervisor escalate from "retry" to "permanent quarantine" inside
// ~4 s of wall clock:
//
//   trap 1: failure_count=1, next_attempt=+1s
//   sleep 1.1s
//   trap 2: failure_count=2, next_attempt=+2s
//   sleep 2.1s
//   trap 3: failure_count=3 -> POISONED. Recent failures hit the
//           window threshold; the supervisor stops attempting
//           restarts entirely. Subsequent dispatches skip the
//           module silently.
//
// Tests assert each transition + the post-quarantine no-op semantic.

#[tokio::test]
async fn poison_pill_quarantines_module_after_threshold() {
    let Some(wasm) = module_wasm_or_skip("fuel-bomb") else {
        return;
    };
    let manifest = production_module_toml("modules/fixtures/fuel-bomb/module.toml");
    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, store) = temp_local_store();
    let components = test_components(store);

    // Tight policy: 3 failures in 60 s -> quarantine. Keeps the
    // test wall-clock under 4 s. Set through `[limits.poison]`.
    let limits = crate::engine_config::ModuleLimits {
        poison: crate::engine_config::PoisonLimitsSection {
            max_failures: Some(3),
            window_secs: Some(60),
        },
        ..Default::default()
    };
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

    assert_eq!(supervisor.module_count(), 1);
    assert_eq!(supervisor.alive_count(), 1);
    assert_eq!(supervisor.poisoned_count(), 0);

    let block = nexum::host::types::Block {
        chain_id: 1,
        number: 1,
        hash: vec![0; 32],
        timestamp: 1_700_000_000_000,
    };

    // Trap 1.
    let dispatched = supervisor.dispatch_block(block.clone()).await;
    assert_eq!(dispatched, 0);
    assert_eq!(supervisor.alive_count(), 0);
    assert_eq!(supervisor.poisoned_count(), 0, "1 trap < threshold");
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;

    // Trap 2.
    let dispatched = supervisor.dispatch_block(block.clone()).await;
    assert_eq!(dispatched, 0);
    assert_eq!(supervisor.poisoned_count(), 0, "2 traps < threshold");
    tokio::time::sleep(std::time::Duration::from_millis(2_100)).await;

    // Trap 3 -> POISONED.
    let dispatched = supervisor.dispatch_block(block.clone()).await;
    assert_eq!(dispatched, 0);
    assert_eq!(
        supervisor.poisoned_count(),
        1,
        "3 traps inside window -> module quarantined",
    );

    // Post-quarantine: immediately re-dispatch. A poisoned module
    // is excluded regardless of how much time has passed; the
    // backoff timer is no longer load-bearing. We do NOT wait for
    // the would-be next_attempt because the test just needs to
    // observe the "skipped silently" semantic, not the timing.
    let dispatched = supervisor.dispatch_block(block).await;
    assert_eq!(
        dispatched, 0,
        "poisoned module excluded from dispatch forever",
    );
    assert_eq!(supervisor.poisoned_count(), 1);
}
