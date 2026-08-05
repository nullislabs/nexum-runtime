//! Dispatch: limits resolution, deadlines, rate limiting, and per-chain
//! isolation.

use super::*;

#[test]
fn module_limits_default_to_engine_limits_when_unset() {
    let cfg = ModuleLimits::default();
    let resolved = resolve_module_limits(&ResourceSection::default(), &cfg);
    assert_eq!(resolved.fuel, cfg.fuel());
    assert_eq!(resolved.memory, cfg.memory());
    assert_eq!(resolved.state_bytes, cfg.state_bytes());
}

#[test]
fn manifest_resource_overrides_take_effect_and_are_field_local() {
    let cfg = ModuleLimits::default();
    // Only fuel is overridden; memory + state keep the engine defaults.
    let res = ResourceSection {
        max_memory_bytes: None,
        max_fuel_per_event: Some(100_000),
        max_state_bytes: Some(2048),
    };
    let resolved = resolve_module_limits(&res, &cfg);
    assert_eq!(resolved.fuel, 100_000);
    assert_eq!(resolved.memory, cfg.memory());
    assert_eq!(resolved.state_bytes, 2048);
}

// `with_dispatch_deadline` bounds a dispatch in wall-clock, covering
// host-call time fuel cannot meter.

/// `with_dispatch_deadline` cancels rather than awaits an over-long future:
/// a sleep far past the deadline is dropped, not run.
#[tokio::test]
async fn dispatch_deadline_interrupts_a_sleeping_host_call() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let ran_to_completion = Arc::new(AtomicBool::new(false));
    let flag = ran_to_completion.clone();
    // Models a guest whose host call parks for an hour (a hung RPC / a
    // server that never answers). Without the deadline this future would
    // hold the dispatch for the full hour.
    let dispatch = async move {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        flag.store(true, Ordering::SeqCst);
    };

    let result = with_dispatch_deadline(Duration::from_millis(50), dispatch).await;

    assert!(
        result.is_err(),
        "a host call sleeping 1h must be cut off by the 50ms deadline",
    );
    assert!(
        !ran_to_completion.load(Ordering::SeqCst),
        "the sleeping future must be cancelled, not left to run unbounded",
    );
}

/// The deadline does not punish a dispatch that finishes promptly: the
/// inner future's value is returned untouched.
#[tokio::test]
async fn dispatch_deadline_lets_a_prompt_call_finish() {
    let result = with_dispatch_deadline(Duration::from_secs(30), async { 7_u8 }).await;
    assert_eq!(result.expect("prompt call is well under the deadline"), 7);
}

/// The resolved deadline honours an override, falls back to the default
/// when unset, and saturates a degenerate `0` up to the 1s floor so it
/// cannot cut every dispatch off instantly.
#[test]
fn event_deadline_resolves_override_default_and_floor() {
    let default = ModuleLimits::default();
    assert_eq!(
        default.event_deadline(),
        Duration::from_secs(120),
        "unset resolves to the built-in default",
    );

    let overridden = ModuleLimits {
        event_deadline_secs: Some(5),
        ..ModuleLimits::default()
    };
    assert_eq!(overridden.event_deadline(), Duration::from_secs(5));

    let degenerate = ModuleLimits {
        event_deadline_secs: Some(0),
        ..ModuleLimits::default()
    };
    assert_eq!(
        degenerate.event_deadline(),
        Duration::from_secs(1),
        "a zero override saturates up to the 1s floor",
    );
}

/// A guest suspended inside a host call is cut off by the wall-clock
/// deadline and the module marked dead, then a later dispatch reinstantiates
/// it on a fresh store. The `slow-host` fixture parks its first
/// `chain::request` an hour past a 1s deadline, one-shot, so it recovers
/// after the backoff.
#[tokio::test]
async fn dispatch_deadline_cuts_off_a_blocked_host_call_and_recovers() {
    use std::time::Instant;

    let Some(wasm) = module_wasm_or_skip("slow-host") else {
        return;
    };

    let engine = make_wasmtime_engine();
    let linker = crate::supervisor::build_linker::<crate::test_utils::MockTypes>(&engine, &[])
        .expect("build_linker");

    // Program the chain backend: the first request parks for an hour (a
    // hung node), every request answers `eth_blockNumber` once it runs.
    // The park is consumed when the first request begins, so the request
    // dropped at the deadline leaves the next one prompt.
    let node = crate::test_utils::rpc::FakeNode::new();
    node.on_method(
        crate::host::component::ChainMethod::EthBlockNumber,
        "\"0x1\"",
    );
    node.delay_next_request(Duration::from_secs(3600));
    let components =
        crate::test_utils::mock_components_from(&node, crate::test_utils::MockStateStore::new());

    let manifest = fixture_module_toml("modules/fixtures/slow-host/module.toml");
    // 1s is the floor the resolver saturates up to; short enough to keep
    // the test quick, long enough to prove the call was cut off (the park
    // is an hour) rather than never started.
    let limits = ModuleLimits {
        event_deadline_secs: Some(1),
        ..ModuleLimits::default()
    };

    let mut supervisor = Supervisor::<crate::test_utils::MockTypes>::boot_single(
        &engine,
        &linker,
        &wasm,
        Some(&manifest),
        &components,
        &limits,
        &test_chains(),
        false,
        &[],
        None,
    )
    .await
    .expect("boot_single");
    assert_eq!(supervisor.alive_count(), 1, "slow-host loads alive");

    let block = nexum::host::types::Block {
        chain_id: 1,
        number: 1,
        hash: vec![0; 32],
        timestamp: 1_700_000_000_000,
    };

    // First dispatch: the guest suspends inside the parked host call and
    // the 1s deadline cuts it off. It resolves in ~deadline wall-time, not
    // the hour the mock would otherwise park for.
    let started = Instant::now();
    let dispatched = supervisor.dispatch_block(block.clone()).await;
    let elapsed = started.elapsed();
    assert_eq!(dispatched, 0, "the deadline cut the blocked host call off");
    assert!(
        elapsed < Duration::from_secs(30),
        "cut off in ~deadline wall-time ({elapsed:?}), not the 1h park",
    );
    assert_eq!(
        supervisor.alive_count(),
        0,
        "the module is marked dead after the deadline, like a trap",
    );

    // Wait out the 1s restart backoff, then dispatch again. Phase 1 of the
    // dispatch reinstantiates the dead module on a fresh store (proving the
    // store poisoned by the dropped fiber was correctly torn down and
    // rebuilt); the guest's next request is prompt, so it dispatches Ok.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let dispatched_again = supervisor.dispatch_block(block).await;
    assert_eq!(
        dispatched_again, 1,
        "after backoff the module restarts on a fresh store and dispatches",
    );
    assert_eq!(
        supervisor.alive_count(),
        1,
        "the recovered module is alive again",
    );
}

// ── Multi-chain isolation ───────────────────────────────────
//
// The supervisor's dispatch path is per-chain: `dispatch_block(block)`
// walks every module but only invokes those whose
// `[[subscription]] kind = "block"` matches `block.chain_id`. A
// module on chain A receives nothing when a chain-B block arrives,
// and vice versa. Combined with the per-module restart / poison
// state, this gives the engine multi-chain isolation by
// construction: a poisoned module on one chain cannot starve
// modules on any other chain.
//
// The WS reconnect tasks add the upstream symmetry: each
// chain owns its own subscription task + backoff timer, so a chain-A
// WS drop never blocks chain-B events.

#[tokio::test]
async fn multi_chain_dispatch_isolates_modules_by_chain() {
    // Two example modules on two different chains. Confirm dispatch
    // on chain A reaches only the chain-A module and vice versa.
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let chain_a_manifest = dir.path().join("a.toml");
    let chain_b_manifest = dir.path().join("b.toml");
    std::fs::write(
        &chain_a_manifest,
        r#"
[module]
name = "module-a"

[capabilities]
required = ["logging"]

[[subscription]]
kind     = "block"
chain_id = 1
"#,
    )
    .unwrap();
    std::fs::write(
        &chain_b_manifest,
        r#"
[module]
name = "module-b"

[capabilities]
required = ["logging"]

[[subscription]]
kind     = "block"
chain_id = 100
"#,
    )
    .unwrap();

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, local_store) = temp_local_store();
    let components = test_components(local_store);

    let engine_cfg = crate::engine_config::EngineConfig {
        engine: crate::engine_config::EngineSection {
            state_dir: dir.path().to_path_buf(),
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
                path: wasm.clone(),
                manifest: Some(chain_a_manifest),
            },
            crate::engine_config::ModuleEntry {
                path: wasm,
                manifest: Some(chain_b_manifest),
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
    assert_eq!(supervisor.alive_count(), 2);

    let block_a = nexum::host::types::Block {
        chain_id: 1,
        number: 1,
        hash: vec![0; 32],
        timestamp: 1_700_000_000_000,
    };
    let block_b = nexum::host::types::Block {
        chain_id: 100,
        number: 1,
        hash: vec![0; 32],
        timestamp: 1_700_000_000_000,
    };

    // Chain A block reaches only module-a.
    let dispatched = supervisor.dispatch_block(block_a).await;
    assert_eq!(dispatched, 1, "only module-a subscribed to chain 1");
    assert_eq!(supervisor.alive_count(), 2);

    // Chain B block reaches only module-b.
    let dispatched = supervisor.dispatch_block(block_b).await;
    assert_eq!(dispatched, 1, "only module-b subscribed to chain 100");
    assert_eq!(supervisor.alive_count(), 2);
}

/// Per-module dispatch rate limit: a source flooding one module is
/// throttled (over-rate events dropped) while a second module on another
/// chain still gets every dispatch. A tiny `[limits.dispatch]` (burst = 2,
/// refill = 1/s) drains the first bucket almost immediately.
#[tokio::test]
async fn dispatch_rate_limit_throttles_a_flood_without_starving_others() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let flood_manifest = dir.path().join("flood.toml");
    let calm_manifest = dir.path().join("calm.toml");
    std::fs::write(
        &flood_manifest,
        r#"
[module]
name = "flood"

[capabilities]
required = ["logging"]

[[subscription]]
kind     = "block"
chain_id = 1
"#,
    )
    .unwrap();
    std::fs::write(
        &calm_manifest,
        r#"
[module]
name = "calm"

[capabilities]
required = ["logging"]

[[subscription]]
kind     = "block"
chain_id = 100
"#,
    )
    .unwrap();

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, local_store) = temp_local_store();
    let components = test_components(local_store);

    let engine_cfg = crate::engine_config::EngineConfig {
        engine: crate::engine_config::EngineSection {
            state_dir: dir.path().to_path_buf(),
            log_level: "info".into(),
            metrics: crate::engine_config::MetricsSection::default(),
            ..Default::default()
        },
        limits: crate::engine_config::ModuleLimits {
            dispatch: crate::engine_config::DispatchLimitsSection {
                burst: Some(2),
                refill_per_sec: Some(1),
            },
            ..Default::default()
        },
        chains: crate::test_utils::test_chain_configs(),
        defaulted: false,
        extensions: std::collections::HashMap::new(),
        modules: vec![
            crate::engine_config::ModuleEntry {
                path: wasm.clone(),
                manifest: Some(flood_manifest),
            },
            crate::engine_config::ModuleEntry {
                path: wasm,
                manifest: Some(calm_manifest),
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
    assert_eq!(supervisor.alive_count(), 2);

    // Flood chain 1 with far more blocks than the burst allowance. The
    // loop runs in well under a second, so refill (1 token/s) adds at
    // most one or two tokens: the flood module is dispatched only a
    // handful of times and the rest are dropped.
    const FLOOD: u64 = 20;
    let mut flood_dispatched = 0;
    for number in 0..FLOOD {
        flood_dispatched += supervisor
            .dispatch_block(nexum::host::types::Block {
                chain_id: 1,
                number,
                hash: vec![0; 32],
                timestamp: 1_700_000_000_000,
            })
            .await;
    }
    assert!(
        flood_dispatched >= 2,
        "the burst allowance ({flood_dispatched}) must clear before throttling",
    );
    assert!(
        flood_dispatched < FLOOD as usize,
        "the flood must be throttled: {flood_dispatched} of {FLOOD} got through",
    );

    // The calm module on chain 100 has its own untouched bucket, so a
    // block on its chain still dispatches even though the flood module
    // is being throttled. This is the per-module fairness guarantee.
    let calm_dispatched = supervisor
        .dispatch_block(nexum::host::types::Block {
            chain_id: 100,
            number: 1,
            hash: vec![0; 32],
            timestamp: 1_700_000_000_000,
        })
        .await;
    assert_eq!(
        calm_dispatched, 1,
        "the calm module is served in full - a flood on another module never starves it",
    );

    // Neither module died: rate limiting is a benign drop, not a fault.
    assert_eq!(
        supervisor.alive_count(),
        2,
        "rate limiting must not kill modules"
    );
    assert_eq!(supervisor.poisoned_count(), 0);
}

#[tokio::test]
async fn multi_chain_poisoned_module_does_not_affect_other_chains() {
    // fuel-bomb (always-traps) on chain 1, example (healthy) on
    // chain 100. Trap the bomb a few times with a tight poison
    // policy so it gets quarantined; verify the example keeps
    // dispatching on chain 100 throughout.
    let Some(bomb_wasm) = module_wasm_or_skip("fuel-bomb") else {
        return;
    };
    let Some(example_wasm) = example_wasm_or_skip() else {
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let example_manifest = dir.path().join("example.toml");
    std::fs::write(
        &example_manifest,
        r#"
[module]
name = "example"

[capabilities]
required = ["logging"]

[[subscription]]
kind     = "block"
chain_id = 100
"#,
    )
    .unwrap();

    let engine = make_wasmtime_engine();
    let linker = make_linker(&engine);
    let (_dir, local_store) = temp_local_store();
    let components = test_components(local_store);

    let engine_cfg = crate::engine_config::EngineConfig {
        engine: crate::engine_config::EngineSection {
            state_dir: dir.path().to_path_buf(),
            log_level: "info".into(),
            metrics: crate::engine_config::MetricsSection::default(),
            ..Default::default()
        },
        // Tight policy: 2 failures in 60 s -> quarantine, set through
        // `[limits.poison]`.
        limits: crate::engine_config::ModuleLimits {
            poison: crate::engine_config::PoisonLimitsSection {
                max_failures: Some(2),
                window_secs: Some(60),
            },
            ..Default::default()
        },
        chains: crate::test_utils::test_chain_configs(),
        defaulted: false,
        extensions: std::collections::HashMap::new(),
        modules: vec![
            crate::engine_config::ModuleEntry {
                path: bomb_wasm,
                manifest: Some(fixture_module_toml(
                    "modules/fixtures/fuel-bomb/module.toml",
                )),
            },
            crate::engine_config::ModuleEntry {
                path: example_wasm,
                manifest: Some(example_manifest),
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
    assert_eq!(supervisor.alive_count(), 2);

    let block_bomb_chain = nexum::host::types::Block {
        chain_id: 1, // fuel-bomb's manifest declares chain 1
        number: 1,
        hash: vec![0; 32],
        timestamp: 1_700_000_000_000,
    };
    let block_healthy_chain = nexum::host::types::Block {
        chain_id: 100,
        number: 1,
        hash: vec![0; 32],
        timestamp: 1_700_000_000_000,
    };

    // Trap #1 on the bomb's chain: bomb dies, example untouched.
    supervisor.dispatch_block(block_bomb_chain.clone()).await;
    assert_eq!(supervisor.poisoned_count(), 0);

    // Example keeps dispatching on its own chain - confirm before
    // the bomb hits the poison threshold.
    let dispatched_b = supervisor.dispatch_block(block_healthy_chain.clone()).await;
    assert_eq!(dispatched_b, 1, "module-b receives chain-100 blocks");

    // Wait out the bomb's backoff so trap #2 can land.
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    supervisor.dispatch_block(block_bomb_chain).await;
    assert_eq!(
        supervisor.poisoned_count(),
        1,
        "bomb quarantined at 2 failures",
    );

    // POST-poison: bomb stays dead, example still healthy.
    let dispatched_after = supervisor.dispatch_block(block_healthy_chain).await;
    assert_eq!(
        dispatched_after, 1,
        "chain-100 module unaffected by chain-1 poison",
    );
    assert_eq!(supervisor.alive_count(), 1, "only example is alive");
    assert_eq!(supervisor.poisoned_count(), 1);
}
