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

    // 1s is the floor the resolver saturates up to; short enough to keep
    // the test quick, long enough to prove the call was cut off (the park
    // is an hour) rather than never started.
    let mut booted = BootScenario::over(crate::test_utils::mock_components_from(
        &node,
        crate::test_utils::MockStateStore::new(),
    ))
    .limits(ModuleLimits {
        event_deadline_secs: Some(1),
        ..ModuleLimits::default()
    })
    .wasm(wasm)
    .module(workspace_manifest("modules/fixtures/slow-host/module.toml"))
    .boot()
    .await
    .expect("slow-host boots");
    assert_eq!(booted.supervisor.alive_count(), 1, "slow-host loads alive");

    // First dispatch: the guest suspends inside the parked host call and
    // the 1s deadline cuts it off. It resolves in ~deadline wall-time, not
    // the hour the mock would otherwise park for.
    let started = Instant::now();
    let dispatched = booted.dispatch_block_on(1).await;
    let elapsed = started.elapsed();
    assert_eq!(dispatched, 0, "the deadline cut the blocked host call off");
    assert!(
        elapsed < Duration::from_secs(30),
        "cut off in ~deadline wall-time ({elapsed:?}), not the 1h park",
    );
    assert_eq!(
        booted.supervisor.alive_count(),
        0,
        "the module is marked dead after the deadline, like a trap",
    );

    // Wait out the 1s restart backoff, then dispatch again. Phase 1 of the
    // dispatch reinstantiates the dead module on a fresh store (proving the
    // store poisoned by the dropped fiber was correctly torn down and
    // rebuilt); the guest's next request is prompt, so it dispatches Ok.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(
        booted.dispatch_block_on(1).await,
        1,
        "after backoff the module restarts on a fresh store and dispatches",
    );
    assert_eq!(
        booted.supervisor.alive_count(),
        1,
        "the recovered module is alive again",
    );
}

/// The dispatch path is per-chain: a module on chain A receives nothing
/// when a chain-B block arrives, and vice versa. Combined with per-module
/// restart and poison state this isolates chains by construction.
#[tokio::test]
async fn multi_chain_dispatch_isolates_modules_by_chain() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let mut booted = BootScenario::new()
        .wasm(wasm)
        .module(TestManifest::new("module-a").cap("logging").block_sub(1))
        .module(TestManifest::new("module-b").cap("logging").block_sub(100))
        .boot()
        .await
        .expect("boot");
    assert_eq!(booted.supervisor.module_count(), 2);
    assert_eq!(booted.supervisor.alive_count(), 2);

    assert_eq!(
        booted.dispatch_block_on(1).await,
        1,
        "only module-a subscribed to chain 1",
    );
    assert_eq!(
        booted.dispatch_block_on(100).await,
        1,
        "only module-b subscribed to chain 100",
    );
    assert_eq!(booted.supervisor.alive_count(), 2);
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
    let mut booted = BootScenario::new()
        .wasm(wasm)
        .limits(ModuleLimits {
            dispatch: crate::engine_config::DispatchLimitsSection {
                burst: Some(2),
                refill_per_sec: Some(1),
            },
            ..Default::default()
        })
        .module(TestManifest::new("flood").cap("logging").block_sub(1))
        .module(TestManifest::new("calm").cap("logging").block_sub(100))
        .boot()
        .await
        .expect("boot");
    assert_eq!(booted.supervisor.alive_count(), 2);

    // Flood chain 1 with far more blocks than the burst allowance. The
    // loop runs in well under a second, so refill (1 token/s) adds at
    // most one or two tokens: the flood module is dispatched only a
    // handful of times and the rest are dropped.
    const FLOOD: usize = 20;
    let mut flood_dispatched = 0;
    for _ in 0..FLOOD {
        flood_dispatched += booted.dispatch_block_on(1).await;
    }
    assert!(
        flood_dispatched >= 2,
        "the burst allowance ({flood_dispatched}) must clear before throttling",
    );
    assert!(
        flood_dispatched < FLOOD,
        "the flood must be throttled: {flood_dispatched} of {FLOOD} got through",
    );

    // The calm module on chain 100 has its own untouched bucket, so a
    // block on its chain still dispatches even though the flood module
    // is being throttled. This is the per-module fairness guarantee.
    assert_eq!(
        booted.dispatch_block_on(100).await,
        1,
        "the calm module is served in full - a flood on another module never starves it",
    );

    // Neither module died: rate limiting is a benign drop, not a fault.
    assert_eq!(
        booted.supervisor.alive_count(),
        2,
        "rate limiting must not kill modules"
    );
    assert_eq!(booted.supervisor.poisoned_count(), 0);
}

/// fuel-bomb (always-traps) on chain 1, example (healthy) on chain 100:
/// the bomb is quarantined under a tight poison policy while the example
/// keeps dispatching on its own chain throughout.
#[tokio::test]
async fn multi_chain_poisoned_module_does_not_affect_other_chains() {
    let Some(bomb_wasm) = module_wasm_or_skip("fuel-bomb") else {
        return;
    };
    let Some(example_wasm) = example_wasm_or_skip() else {
        return;
    };
    // Tight policy: 2 failures in 60 s -> quarantine, set through
    // `[limits.poison]`.
    let mut booted = BootScenario::new()
        .limits(ModuleLimits {
            poison: crate::engine_config::PoisonLimitsSection {
                max_failures: Some(2),
                window_secs: Some(60),
            },
            ..Default::default()
        })
        .module(
            Entry::new(workspace_manifest("modules/fixtures/fuel-bomb/module.toml"))
                .wasm(bomb_wasm),
        )
        .module(
            Entry::new(TestManifest::new("example").cap("logging").block_sub(100))
                .wasm(example_wasm),
        )
        .boot()
        .await
        .expect("boot");
    assert_eq!(booted.supervisor.module_count(), 2);
    assert_eq!(booted.supervisor.alive_count(), 2);

    // Trap #1 on the bomb's chain: bomb dies, example untouched.
    booted.dispatch_block_on(1).await;
    assert_eq!(booted.supervisor.poisoned_count(), 0);

    // Example keeps dispatching on its own chain - confirm before
    // the bomb hits the poison threshold.
    assert_eq!(
        booted.dispatch_block_on(100).await,
        1,
        "the example receives chain-100 blocks",
    );

    // Wait out the bomb's backoff so trap #2 can land.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    booted.dispatch_block_on(1).await;
    assert_eq!(
        booted.supervisor.poisoned_count(),
        1,
        "bomb quarantined at 2 failures",
    );

    // POST-poison: bomb stays dead, example still healthy.
    assert_eq!(
        booted.dispatch_block_on(100).await,
        1,
        "chain-100 module unaffected by chain-1 poison",
    );
    assert_eq!(booted.supervisor.alive_count(), 1, "only example is alive");
    assert_eq!(booted.supervisor.poisoned_count(), 1);
}
