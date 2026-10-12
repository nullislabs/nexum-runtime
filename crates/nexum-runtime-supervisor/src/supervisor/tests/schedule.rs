//! Schedule triggers: the load refusal, the source plan, and dispatch.

use std::collections::HashMap;

use super::*;
use crate::engine_config::{DispatchLimitsSection, PoisonLimitsSection};
use crate::test_utils::metrics_util::debugging::DebugValue;
use crate::test_utils::{Booted, capture_metrics, samples_named};
use nexum_primitives::module_id::ModuleId;

/// An arbitrary `fired-at`; dispatch carries it through untouched.
const FIRED_AT: u64 = 1_715_947_260_000;

fn module_id(name: &str) -> ModuleId {
    ModuleId::parse(name).expect("a plain name parses")
}

fn scheduled(name: &str, cron: &str) -> TestManifest {
    TestManifest::new(name)
        .cap("logging")
        .schedule_trigger(cron)
}

/// Every `schedule fired at` line `module` logged, as its `fired-at`.
fn ticks_seen(booted: &Booted<LocalTypes>, module: &str) -> Vec<String> {
    booted
        .records(module)
        .into_iter()
        .filter_map(|record| {
            record
                .message
                .strip_prefix("schedule fired at ")
                .map(str::to_owned)
        })
        .collect()
}

/// The refusal precedes compile, so no wasm needs to exist. It carries the
/// expression as written and counts under its own pinned label.
#[test]
fn boot_refuses_an_invalid_schedule_cron_under_its_own_label() {
    let (refusal, samples) = capture_metrics(|| {
        block_on_current_thread(
            scenario()
                .module(scheduled("example", "0 */5 * * * *"))
                .expect_refusal(),
        )
    });
    refusal
        .variant::<BootRefusal>(|e| {
            matches!(
                e,
                BootRefusal::Manifest(ParseError::InvalidScheduleCron { value, .. })
                    if value == "0 */5 * * * *"
            )
        })
        // Operator wording pin.
        .names("load module")
        .names("manifest: invalid schedule cron \"0 */5 * * * *\"")
        .names("expected 5 fields (minute hour day-of-month month day-of-week), found 6")
        .names("a seconds field, a year field and a timezone are not supported")
        .lacks("compile");
    let hits = samples_named(&samples, "nexum_runtime_boot_refusals_total");
    assert_eq!(hits.len(), 1, "one series: {samples:?}");
    assert!(
        hits[0].has_label("error_kind", "invalid_schedule_cron"),
        "{:?}",
        hits[0].labels,
    );
    assert!(
        matches!(hits[0].value, DebugValue::Counter(1)),
        "{:?}",
        hits[0].value,
    );
}

/// Each unsupported form is a refusal, never a schedule that fires at
/// some other time than the one the author wrote.
#[tokio::test]
async fn boot_refuses_every_unsupported_cron_form() {
    for cron in [
        "@daily",
        "* * * *",
        "0 0 L * *",
        "0 0 15W * *",
        "0 0 * * 5#2",
        "0 0 ? * *",
        "0 9 * * MON-FRI",
        "TZ=Europe/London 0 9 * * *",
        "60 * * * *",
        "0 0 30 2 *",
    ] {
        scenario()
            .module(scheduled("example", cron))
            .expect_refusal()
            .await
            .variant::<BootRefusal>(|e| {
                matches!(
                    e,
                    BootRefusal::Manifest(ParseError::InvalidScheduleCron { value, .. })
                        if value == cron
                )
            })
            .lacks("compile");
    }
}

/// A schedule is not chain-scoped: the prepass gate has nothing to check,
/// so neither an empty `[chains]` nor a run on defaults refuses it, and
/// its source alone keeps the engine live.
#[tokio::test]
async fn a_schedule_only_engine_is_viable_and_needs_no_chain() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    for chainless in [
        scenario().chains(HashMap::new()),
        scenario().defaulted_chains(),
    ] {
        let booted = chainless
            .wasm(&wasm)
            .module(scheduled("example", "*/5 * * * *"))
            .boot()
            .await
            .expect("a schedule trigger needs no [chains] entry");
        assert_eq!(booted.supervisor.alive_count(), 1);

        let plan = booted.supervisor.source_plan();
        assert!(plan.block_chains.is_empty());
        assert!(plan.event_sources.is_empty());
        assert_eq!(plan.schedule_sources.len(), 1);
        assert_eq!(plan.schedule_sources[0].module.as_str(), "example");
        assert_eq!(plan.schedule_sources[0].cron.expression(), "*/5 * * * *");
        assert_eq!(
            plan.viability(0),
            Viability::Live,
            "a schedule source alone drives the engine",
        );
    }
}

/// One source per trigger, in declaration order, so two expressions on one
/// module tick independently.
#[tokio::test]
async fn each_schedule_trigger_contributes_its_own_source() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let booted = scenario()
        .wasm(wasm)
        .module(
            scheduled("example", "*/5 * * * *")
                .schedule_trigger("0 0 * * 0")
                .block_trigger(1),
        )
        .boot()
        .await
        .expect("boot");
    let plan = booted.supervisor.source_plan();
    assert_eq!(
        plan.schedule_sources
            .iter()
            .map(|source| source.cron.expression())
            .collect::<Vec<_>>(),
        ["*/5 * * * *", "0 0 * * 0"],
    );
    assert_eq!(plan.block_chains.len(), 1);
}

/// A module whose boot-time `init` failed is dead for good, so its
/// schedule opens no source and the plan charges the trigger to it, as it
/// does a dead module's block trigger.
#[tokio::test]
async fn a_boot_dead_module_contributes_no_schedule_source() {
    let Some(wasm) = module_wasm_or_skip("price-alert") else {
        return;
    };
    let mut manifest = TestManifest::new("price-alert")
        .cap("logging")
        .cap("chain")
        .schedule_trigger("* * * * *");
    for (key, value) in [
        (
            "oracle_address",
            "0x694AA1769357215DE4FAC081bf1f309aDC325306",
        ),
        ("decimals", "8"),
        // `init` rejects this with `fault.invalid-input`.
        ("threshold", "not-a-number"),
        ("direction", "below"),
        ("every_n_blocks", "1"),
    ] {
        manifest = manifest.config(key, value);
    }
    let mut booted = scenario()
        .wasm(wasm)
        .module(manifest)
        .boot()
        .await
        .expect("the module loads; only init fails");
    assert_eq!(booted.supervisor.alive_count(), 0);

    let plan = booted.supervisor.source_plan();
    assert!(plan.schedule_sources.is_empty());
    assert_eq!(plan.viability(0), Viability::DeadHoldTriggers);
    assert!(
        !booted
            .supervisor
            .dispatch_schedule(&module_id("price-alert"), FIRED_AT)
            .await,
        "a tick never reaches a dead module",
    );
}

/// A tick is routed by module name, and the guest reads the `fired-at` the
/// source stamped.
#[tokio::test]
async fn a_schedule_tick_reaches_only_the_module_it_names() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let mut booted = scenario()
        .chains(HashMap::new())
        .wasm(wasm)
        .module(scheduled("first", "* * * * *"))
        .module(scheduled("second", "* * * * *"))
        .boot()
        .await
        .expect("boot");

    assert!(
        booted
            .supervisor
            .dispatch_schedule(&module_id("second"), FIRED_AT)
            .await
    );
    assert_eq!(ticks_seen(&booted, "second"), [format!("{FIRED_AT}ms")]);
    assert_eq!(ticks_seen(&booted, "first"), Vec::<String>::new());
    assert!(
        !booted
            .supervisor
            .dispatch_schedule(&module_id("ghost"), FIRED_AT)
            .await,
        "an unknown module drops the tick",
    );
}

/// The dispatch metrics carry `schedule` beside `block` and `event`, the
/// spelling the manifest and `nexum_world::TriggerKind` use.
#[test]
fn a_schedule_dispatch_records_latency_under_its_trigger_kind() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    assert_eq!(<&str>::from(nexum_world::TriggerKind::Schedule), "schedule");
    let (ok, samples) = capture_metrics(|| {
        block_on_current_thread(async {
            let mut booted = scenario()
                .wasm(wasm)
                .module(scheduled("example", "* * * * *"))
                .boot()
                .await
                .expect("boot");
            booted
                .supervisor
                .dispatch_schedule(&module_id("example"), FIRED_AT)
                .await
        })
    });
    assert!(ok);
    let latency = samples_named(&samples, "nexum_runtime_dispatch_latency_seconds");
    assert_eq!(latency.len(), 1, "one series: {samples:?}");
    assert!(
        latency[0].has_label("trigger_kind", "schedule")
            && latency[0].has_label("module", "example")
            && latency[0].has_label("outcome", "ok"),
        "{:?}",
        latency[0].labels,
    );
}

/// The per-module token bucket meters a tick like any other trigger, and
/// the drop is counted under the schedule kind.
#[test]
fn a_schedule_tick_over_the_dispatch_rate_is_dropped_and_counted() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let (delivered, samples) = capture_metrics(|| {
        block_on_current_thread(async {
            tokio::time::pause();
            let mut booted = scenario()
                .wasm(wasm)
                .limits(limits_with(|limits| {
                    limits.dispatch = DispatchLimitsSection {
                        burst: Some(1),
                        refill_per_sec: Some(1),
                        ..Default::default()
                    }
                }))
                .module(scheduled("example", "* * * * *"))
                .boot()
                .await
                .expect("boot");
            let module = module_id("example");
            let mut delivered = Vec::new();
            for _ in 0..3 {
                delivered.push(booted.supervisor.dispatch_schedule(&module, FIRED_AT).await);
            }
            delivered
        })
    });
    assert_eq!(delivered, [true, false, false], "the burst is one tick");
    let dropped = samples_named(&samples, "nexum_runtime_dispatch_dropped_total");
    assert_eq!(dropped.len(), 1, "one series: {samples:?}");
    assert!(
        dropped[0].has_label("trigger_kind", "schedule")
            && dropped[0].has_label("reason", "rate_limited")
            && dropped[0].has_label("module", "example"),
        "{:?}",
        dropped[0].labels,
    );
    assert!(
        matches!(dropped[0].value, DebugValue::Counter(2)),
        "{:?}",
        dropped[0].value,
    );
}

/// A fired stop drops the tick before the guest, as it does an event, and
/// counts it under the shutdown reason.
#[test]
fn a_fired_stop_drops_a_schedule_tick_and_counts_it() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let (delivered, samples) = capture_metrics(|| {
        block_on_current_thread(async {
            let mut booted = scenario()
                .wasm(wasm)
                .module(scheduled("example", "* * * * *"))
                .boot()
                .await
                .expect("boot");
            let module = module_id("example");
            let manager = nexum_tasks::TaskManager::new();
            booted.supervisor.stop_on(manager.subscribe());
            let before = booted.supervisor.dispatch_schedule(&module, FIRED_AT).await;
            manager.shutdown_signal().fire();
            let after = booted.supervisor.dispatch_schedule(&module, FIRED_AT).await;
            (before, after, ticks_seen(&booted, "example").len())
        })
    });
    assert_eq!(
        delivered,
        (true, false, 1),
        "an unfired stop changes nothing; a fired one keeps the guest out",
    );
    let dropped = samples_named(&samples, "nexum_runtime_dispatch_dropped_total");
    assert_eq!(dropped.len(), 1, "one series: {samples:?}");
    assert!(
        dropped[0].has_label("trigger_kind", "schedule")
            && dropped[0].has_label("reason", "shutdown")
            && dropped[0].has_label("module", "example"),
        "{:?}",
        dropped[0].labels,
    );
}

/// fuel-bomb traps on every trigger, so its schedule walks the same
/// restart and poison path a block trigger does: a tick inside the backoff
/// is dropped without a restart, a tick past it revives the module, and
/// the third trap inside the window quarantines it for good.
#[tokio::test(start_paused = true)]
async fn a_poisoned_modules_schedule_stops_like_its_other_triggers() {
    let Some(wasm) = module_wasm_or_skip("fuel-bomb") else {
        return;
    };
    let mut booted = scenario()
        .limits(limits_with(|limits| {
            limits.poison = PoisonLimitsSection {
                max_failures: Some(3),
                window_secs: Some(60),
            }
        }))
        .wasm(wasm)
        .module(scheduled("fuel-bomb", "* * * * *").block_trigger(1))
        .boot()
        .await
        .expect("boot");
    let module = module_id("fuel-bomb");
    let failures =
        |booted: &Booted<LocalTypes>| booted.supervisor.modules[0].health.failure_count();

    // Trap 1: fuel exhaustion on a schedule tick counts like any trap.
    assert!(!booted.supervisor.dispatch_schedule(&module, FIRED_AT).await);
    assert_eq!(booted.supervisor.alive_count(), 0);
    assert_eq!(failures(&booted), 1);

    // Inside the backoff: dropped, no restart, the guest is not entered.
    assert!(!booted.supervisor.dispatch_schedule(&module, FIRED_AT).await);
    assert_eq!(failures(&booted), 1, "a tick in backoff is not a failure");
    assert_eq!(booted.supervisor.poisoned_count(), 0);

    // Past the backoff: the tick revives its own module, which traps again.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert!(!booted.supervisor.dispatch_schedule(&module, FIRED_AT).await);
    assert_eq!(failures(&booted), 2, "the tick restarted the module");
    assert_eq!(booted.supervisor.poisoned_count(), 0, "2 traps < threshold");

    // Trap 3: poisoned.
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    assert!(!booted.supervisor.dispatch_schedule(&module, FIRED_AT).await);
    assert_eq!(failures(&booted), 3);
    assert_eq!(booted.supervisor.poisoned_count(), 1);

    // Quarantined: no tick and no block reaches it, and none restarts it,
    // however long the engine waits.
    tokio::time::sleep(Duration::from_secs(3_600)).await;
    assert!(!booted.supervisor.dispatch_schedule(&module, FIRED_AT).await);
    assert_eq!(booted.dispatch_block_on(1).await, 0);
    assert_eq!(failures(&booted), 3, "a poisoned module is never entered");
    assert_eq!(booted.supervisor.poisoned_count(), 1);
    assert!(
        booted.supervisor.source_plan().schedule_sources.is_empty(),
        "a poisoned module holds no schedule source, as it holds no block chain",
    );
}

/// The source-terminal quarantine stops a schedule exactly like the trap
/// threshold does.
#[tokio::test(start_paused = true)]
async fn a_source_poisoned_module_receives_no_schedule_tick() {
    let Some(wasm) = example_wasm_or_skip() else {
        return;
    };
    let mut booted = scenario()
        .wasm(wasm)
        .module(scheduled("example", "* * * * *").event_trigger(1))
        .boot()
        .await
        .expect("boot");
    let module = module_id("example");
    assert!(booted.supervisor.dispatch_schedule(&module, FIRED_AT).await);

    booted
        .supervisor
        .poison_source("example", 1, "unrecoverable");
    tokio::time::sleep(Duration::from_secs(3_600)).await;
    assert!(!booted.supervisor.dispatch_schedule(&module, FIRED_AT).await);
    assert_eq!(ticks_seen(&booted, "example").len(), 1);
    assert_eq!(booted.supervisor.poisoned_count(), 1);
}
