//! Dispatch: limits resolution, deadlines, rate limiting, per-chain isolation.

use super::*;
use crate::engine_config::DispatchLimitsSection;

fn deadline_secs(secs: u64) -> DispatchLimitsSection {
    DispatchLimitsSection {
        deadline_secs: Some(secs),
        ..Default::default()
    }
}

#[test]
fn module_limits_default_to_policy_ceilings_when_unset() {
    let cfg = PolicyCeilings::default();
    let resolved = resolve_module_limits("m", &ResourceSection::default(), &cfg);
    assert_eq!(resolved.fuel, cfg.max_fuel_per_dispatch.get());
    assert_eq!(resolved.memory, cfg.max_memory_bytes.get());
    assert_eq!(resolved.state_bytes, cfg.max_state_bytes);
}

#[test]
fn manifest_resource_overrides_take_effect_and_are_field_local() {
    let cfg = PolicyCeilings::default();
    // Only fuel is overridden; memory + state keep the policy defaults.
    let res = ResourceSection {
        max_memory_bytes: None,
        max_fuel_per_dispatch: Some(100_000),
        max_state_bytes: Some(2048),
    };
    let resolved = resolve_module_limits("m", &res, &cfg);
    assert_eq!(resolved.fuel, 100_000);
    assert_eq!(resolved.memory, cfg.max_memory_bytes.get());
    assert_eq!(resolved.state_bytes, 2048);
}

/// The manifest is author-supplied, so a field above the policy ceiling is
/// capped rather than granted. Each field is raised alone, so a clamp that
/// only covered one of the three would fail here.
#[test]
fn manifest_resources_cannot_widen_the_policy_ceiling() {
    let cfg = PolicyCeilings::default();

    let fuel_grab = ResourceSection {
        max_fuel_per_dispatch: Some(u64::MAX),
        ..ResourceSection::default()
    };
    assert_eq!(
        resolve_module_limits("m", &fuel_grab, &cfg).fuel,
        cfg.max_fuel_per_dispatch.get()
    );

    let memory_grab = ResourceSection {
        max_memory_bytes: Some(usize::MAX),
        ..ResourceSection::default()
    };
    assert_eq!(
        resolve_module_limits("m", &memory_grab, &cfg).memory,
        cfg.max_memory_bytes.get()
    );

    let state_grab = ResourceSection {
        max_state_bytes: Some(u64::MAX),
        ..ResourceSection::default()
    };
    assert_eq!(
        resolve_module_limits("m", &state_grab, &cfg).state_bytes,
        cfg.max_state_bytes,
    );
}

/// Clamping must not cost a module the narrower budget it asked for.
#[test]
fn a_narrower_manifest_value_still_wins() {
    let cfg = PolicyCeilings::default();
    let res = ResourceSection {
        max_memory_bytes: Some(cfg.max_memory_bytes.get() / 2),
        max_fuel_per_dispatch: Some(cfg.max_fuel_per_dispatch.get() / 2),
        max_state_bytes: Some(cfg.max_state_bytes / 2),
    };
    let resolved = resolve_module_limits("m", &res, &cfg);
    assert_eq!(resolved.fuel, cfg.max_fuel_per_dispatch.get() / 2);
    assert_eq!(resolved.memory, cfg.max_memory_bytes.get() / 2);
    assert_eq!(resolved.state_bytes, cfg.max_state_bytes / 2);
}

/// A `[policy.component]` row is the ceiling for its component alone; the
/// clamp direction holds against the row exactly as against the default.
#[test]
fn a_component_row_rebases_the_ceiling_without_widening() {
    let policy = PolicySection {
        component: [(
            "wallet".to_owned(),
            ComponentPolicy {
                max_memory_bytes: std::num::NonZeroUsize::new(1024),
                ..ComponentPolicy::default()
            },
        )]
        .into(),
        ..PolicySection::default()
    };
    let row = policy.for_component("wallet").ceilings;
    let grab = ResourceSection {
        max_memory_bytes: Some(usize::MAX),
        ..ResourceSection::default()
    };
    assert_eq!(resolve_module_limits("m", &grab, &row).memory, 1024);
    // An unnamed component clamps against the [policy] default instead.
    let other = policy.for_component("tracker").ceilings;
    assert_eq!(
        resolve_module_limits("m", &grab, &other).memory,
        PolicyCeilings::default().max_memory_bytes.get()
    );
}

/// The aggregate check refuses on the entry that crosses the cap and
/// admits a set that exactly fills it.
#[test]
fn total_reservation_refuses_the_crossing_component() {
    let policy = PolicySection {
        total: TotalPolicy {
            max_memory_bytes: std::num::NonZeroUsize::new(1000),
        },
        ..PolicySection::default()
    };
    assert!(enforce_total_reservation(&policy, [("a", 500), ("b", 500)]).is_ok());
    let err = enforce_total_reservation(&policy, [("a", 500), ("b", 500), ("c", 1)])
        .expect_err("the third entry crosses the cap");
    assert!(
        matches!(&err, BootRefusal::TotalMemoryExceeded { id, sum, total }
            if id == "c" && *sum == 1001 && *total == 1000),
        "{err:?}",
    );
    // No cap, no bound.
    assert!(enforce_total_reservation(&PolicySection::default(), [("a", usize::MAX)]).is_ok());
}

/// An over-long future is dropped at the deadline, not awaited out.
#[tokio::test]
async fn dispatch_deadline_interrupts_a_sleeping_host_call() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let ran_to_completion = Arc::new(AtomicBool::new(false));
    let flag = ran_to_completion.clone();
    // Models a host call parked for an hour; without the deadline this
    // future would hold the dispatch for the full hour.
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

/// The inner future's value is returned untouched.
#[tokio::test]
async fn dispatch_deadline_lets_a_prompt_call_finish() {
    let result = with_dispatch_deadline(Duration::from_secs(30), async { 7_u8 }).await;
    assert_eq!(result.expect("prompt call is well under the deadline"), 7);
}

/// A `0` deadline would cut every dispatch off instantly, so it refuses
/// at load instead of resolving.
#[test]
fn dispatch_deadline_resolves_override_and_default_and_refuses_zero() {
    let default = ResolvedModuleLimits::default();
    assert_eq!(
        default.dispatch_deadline,
        Duration::from_secs(120),
        "unset resolves to the built-in default",
    );

    let overridden =
        ResolvedModuleLimits::try_from(limits_with(|limits| limits.dispatch = deadline_secs(5)))
            .expect("a non-zero override resolves");
    assert_eq!(overridden.dispatch_deadline, Duration::from_secs(5));

    let degenerate =
        ResolvedModuleLimits::try_from(limits_with(|limits| limits.dispatch = deadline_secs(0)));
    assert!(
        degenerate.is_err(),
        "a zero deadline must refuse at load, not saturate",
    );
}

/// The `slow-host` fixture parks its first `chain::request` an hour past a
/// 1 s deadline, one-shot, so the module recovers after the backoff.
#[tokio::test(start_paused = true)]
async fn dispatch_deadline_cuts_off_a_blocked_host_call_and_recovers() {
    use std::time::Instant;

    let Some(wasm) = module_wasm_or_skip("slow-host") else {
        return;
    };

    // The park is consumed when the first request begins, so the request
    // dropped at the deadline leaves the next one prompt.
    let node = crate::test_utils::FakeNode::new();
    node.on_method(nexum_world::ChainMethod::EthBlockNumber, "\"0x1\"");
    node.delay_next_request(Duration::from_secs(3600));

    // 1 s is the resolver floor; long enough to prove the call was cut off
    // (the park is an hour) rather than never started.
    let mut booted = BootScenario::over(crate::test_utils::mock_components_from(
        &node,
        crate::test_utils::MockStateStore::new(),
    ))
    .limits(limits_with(|limits| limits.dispatch = deadline_secs(1)))
    .wasm(wasm)
    .module(workspace_manifest(
        "modules/fixtures/slow-host/component.toml",
    ))
    .boot()
    .await
    .expect("slow-host boots");
    assert_eq!(booted.supervisor.alive_count(), 1, "slow-host loads alive");

    // Resolves in ~deadline wall-time, not the hour the mock parks for.
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

    // Past the backoff the dispatch reinstantiates the dead module on a
    // fresh store; the guest's next request is prompt, so it dispatches Ok.
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

/// A block a module accepted records the height under its `chain_id` label.
#[test]
fn a_delivered_block_sets_the_last_delivered_gauge() {
    use crate::test_utils::metrics_util::debugging::DebugValue;
    use crate::test_utils::{capture_metrics, samples_named};

    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let (dispatched, samples) = capture_metrics(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime")
            .block_on(async {
                let mut booted = scenario()
                    .wasm(wasm)
                    .module(
                        TestManifest::new("module-a")
                            .cap("logging")
                            .block_trigger(1),
                    )
                    .boot()
                    .await
                    .expect("boot");
                booted.dispatch_block_on(1).await
            })
    });
    assert_eq!(dispatched, 1);
    let hits = samples_named(&samples, "nexum_runtime_chain_last_delivered_height");
    assert_eq!(hits.len(), 1, "one series: {samples:?}");
    assert!(hits[0].has_label("chain_id", "1"), "{:?}", hits[0].labels);
    assert!(
        matches!(hits[0].value, DebugValue::Gauge(v) if v.0 == 19_000_000.0),
        "{:?}",
        hits[0].value,
    );
}

/// A backfilling module or a retracted log delivers below the frontier.
#[test]
fn an_older_delivery_does_not_lower_the_last_delivered_gauge() {
    use crate::test_utils::metrics_util::debugging::DebugValue;
    use crate::test_utils::{capture_metrics, samples_named};

    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let (dispatched, samples) = capture_metrics(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime")
            .block_on(async {
                let mut booted = scenario()
                    .wasm(wasm)
                    .module(
                        TestManifest::new("module-a")
                            .cap("logging")
                            .block_trigger(1),
                    )
                    .boot()
                    .await
                    .expect("boot");
                let newer = booted.dispatch_block_on(1).await;
                let older = booted
                    .supervisor
                    .dispatch_block(nexum::host::types::Block {
                        chain_id: 1,
                        number: 18_000_000,
                        hash: vec![0xcd; 32],
                        timestamp: 1_700_000_000_000,
                    })
                    .await;
                newer + older
            })
    });
    assert_eq!(dispatched, 2, "both blocks reached the module");
    let hits = samples_named(&samples, "nexum_runtime_chain_last_delivered_height");
    assert_eq!(hits.len(), 1, "one series: {samples:?}");
    assert!(
        matches!(hits[0].value, DebugValue::Gauge(v) if v.0 == 19_000_000.0),
        "the older block leaves the frontier alone: {:?}",
        hits[0].value,
    );
}

/// A module on chain A receives nothing when a chain-B block arrives, and
/// vice versa.
#[tokio::test]
async fn multi_chain_dispatch_isolates_modules_by_chain() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let mut booted = scenario()
        .wasm(wasm)
        .module(
            TestManifest::new("module-a")
                .cap("logging")
                .block_trigger(1),
        )
        .module(
            TestManifest::new("module-b")
                .cap("logging")
                .block_trigger(100),
        )
        .boot()
        .await
        .expect("boot");
    assert_eq!(booted.supervisor.module_count(), 2);
    assert_eq!(booted.supervisor.alive_count(), 2);

    assert_eq!(
        booted.dispatch_block_on(1).await,
        1,
        "only module-a declares chain 1",
    );
    assert_eq!(booted.supervisor.alive_count(), 2);
    assert_eq!(
        booted.dispatch_block_on(100).await,
        1,
        "only module-b declares chain 100",
    );
    assert_eq!(booted.supervisor.alive_count(), 2);
}

/// A shutdown drain must cover at most one guest call, so a fired stop
/// halts the block fan-out before the next call while an unfired one
/// changes nothing.
#[tokio::test]
async fn a_fired_stop_halts_the_block_fan_out() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let mut booted = scenario()
        .wasm(wasm)
        .module(
            TestManifest::new("module-a")
                .cap("logging")
                .block_trigger(1),
        )
        .module(
            TestManifest::new("module-b")
                .cap("logging")
                .block_trigger(1),
        )
        .boot()
        .await
        .expect("boot");

    let manager = nexum_tasks::TaskManager::new();
    booted.supervisor.stop_on(manager.subscribe());
    assert_eq!(
        booted.dispatch_block_on(1).await,
        2,
        "an unfired stop leaves the fan-out whole",
    );

    manager.shutdown_signal().fire();
    assert_eq!(
        booted.dispatch_block_on(1).await,
        0,
        "a fired stop halts the fan-out before the next guest call",
    );
}

/// A tiny `[limits.dispatch]` (burst = 2, refill = 1/s) drains the flooded
/// bucket almost immediately; the calm module's bucket is untouched.
#[tokio::test]
async fn dispatch_rate_limit_throttles_a_flood_without_starving_others() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let mut booted = scenario()
        .wasm(wasm)
        .limits(limits_with(|limits| {
            limits.dispatch = DispatchLimitsSection {
                burst: Some(2),
                refill_per_sec: Some(1),
                ..Default::default()
            }
        }))
        .module(TestManifest::new("flood").cap("logging").block_trigger(1))
        .module(TestManifest::new("calm").cap("logging").block_trigger(100))
        .boot()
        .await
        .expect("boot");
    assert_eq!(booted.supervisor.alive_count(), 2);

    // The flood loop runs in well under a second, so refill (1 token/s)
    // adds at most a token or two.
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

    // The calm module's own bucket is untouched, so its chain still dispatches.
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

/// fuel-bomb (always-traps) on chain 1, example (healthy) on chain 100: the
/// example keeps dispatching throughout the bomb's quarantine.
#[tokio::test(start_paused = true)]
async fn multi_chain_poisoned_module_does_not_affect_other_chains() {
    let Some(bomb_wasm) = module_wasm_or_skip("fuel-bomb") else {
        return;
    };
    let Some(example_wasm) = example_wasm_or_skip() else {
        return;
    };
    // Tight `[limits.poison]`: 2 failures in 60 s quarantines.
    let mut booted = scenario()
        .limits(limits_with(|limits| {
            limits.poison = crate::engine_config::PoisonLimitsSection {
                max_failures: Some(2),
                window_secs: Some(60),
            }
        }))
        .module(
            Entry::new(workspace_manifest(
                "modules/fixtures/fuel-bomb/component.toml",
            ))
            .wasm(bomb_wasm),
        )
        .module(
            Entry::new(
                TestManifest::new("example")
                    .cap("logging")
                    .block_trigger(100),
            )
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

    // Confirmed before the bomb hits the poison threshold.
    assert_eq!(
        booted.dispatch_block_on(100).await,
        1,
        "the example receives chain-100 blocks",
    );

    // Past the bomb's backoff so trap #2 can land.
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

#[tokio::test]
async fn a_dispatch_renders_its_module_on_the_span() {
    use tracing::instrument::WithSubscriber as _;

    use crate::test_utils::{JsonLogs, json_collector};

    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let mut booted = scenario()
        .wasm(wasm)
        .module(
            TestManifest::new("module-a")
                .cap("logging")
                .block_trigger(1),
        )
        .boot()
        .await
        .expect("boot");

    let sink = JsonLogs::default();
    let dispatched = booted
        .dispatch_block_on(1)
        .with_subscriber(json_collector(sink.clone(), Level::DEBUG))
        .await;

    assert_eq!(dispatched, 1);
    let line = sink.line("dispatch ok");
    assert_eq!(line["span"]["module"], "module-a");
    assert_eq!(line["span"]["name"], "dispatch");

    // The span earns its keep on the lines the guest provokes, which the
    // dispatch site never sees and so cannot label itself.
    let guest = sink.line("on chain 1");
    assert_eq!(guest["channel"], "host_interface");
    assert_eq!(guest["span"]["module"], "module-a");
    assert_eq!(guest["span"]["name"], "dispatch");
}
