//! Lifecycle: init failure, traps, restart backoff, and poison quarantine.

use tokio::time::Instant;

use super::*;
use crate::supervisor::lifecycle::sweep;

/// price-alert's `[config]`; `not-a-number` makes `init` reject the
/// `threshold` with `fault.invalid-input`.
fn price_alert_config(threshold: &str) -> crate::bindings::Config {
    vec![
        (
            "oracle_address".into(),
            "0x694AA1769357215DE4FAC081bf1f309aDC325306".into(),
        ),
        ("decimals".into(), "8".into()),
        ("threshold".into(), threshold.to_owned()),
        ("direction".into(), "below".into()),
        ("every_n_blocks".into(), "1".into()),
    ]
}

fn price_alert(threshold: &str) -> TestManifest {
    let mut manifest = TestManifest::new("price-alert")
        .cap("logging")
        .cap("chain")
        .block_sub(SEPOLIA);
    for (key, value) in price_alert_config(threshold) {
        manifest = manifest.config(key, value);
    }
    manifest
}

/// Loaded but dead: no dispatch, no chain-facing subscription, and the
/// dropped subscriptions stay attributable.
#[tokio::test]
async fn init_failure_marks_module_dead_excluding_dispatch_and_subscriptions() {
    let Some(wasm) = module_wasm_or_skip("price-alert") else {
        return;
    };
    // Both a block and a filtered chain-log subscription, so both filter
    // paths are exercised.
    let mut booted = BootScenario::new()
        .wasm(wasm)
        .module(price_alert("not-a-number").chain_log_sub_filtered(
            SEPOLIA,
            Some("0xbA3cB449bD2B4ADddBc894D8697F5170800EAdeC"),
            Some("0xcf5f9de2984132265203b5c335b25727702ca77262ff622e136baa7362bf1da9"),
        ))
        .boot()
        .await
        .expect("the module loads; only init fails");

    assert_eq!(booted.supervisor.module_count(), 1, "module is loaded");
    assert_eq!(
        booted.supervisor.alive_count(),
        0,
        "init-failed module must be marked dead",
    );
    assert_eq!(
        booted.dispatch_block_on(SEPOLIA).await,
        0,
        "no live module is subscribed to chain 11155111 blocks",
    );
    let plan = booted.supervisor.subscription_plan();
    assert!(
        plan.block_chains.is_empty(),
        "dead module must not contribute block chains",
    );
    assert!(
        plan.chain_log_subs.is_empty(),
        "dead module must not contribute chain-log subscriptions",
    );
    assert_eq!(
        plan.viability(0),
        Viability::DeadHoldSubs,
        "the filtered-out subscriptions must be attributed to the dead module",
    );
}

/// Positive control: the alive module's subscriptions survive the filter.
#[tokio::test]
async fn alive_module_subscriptions_survive_alongside_dead_module() {
    let Some(price_alert_wasm) = module_wasm_or_skip("price-alert") else {
        return;
    };
    let Some(example_wasm) = example_wasm_or_skip() else {
        return;
    };
    let booted = BootScenario::new()
        .module(Entry::new(price_alert("not-a-number")).wasm(price_alert_wasm))
        .module(
            Entry::new(TestManifest::new("example").cap("logging").block_sub(1)).wasm(example_wasm),
        )
        .boot()
        .await
        .expect("boot");

    assert_eq!(booted.supervisor.module_count(), 2);
    assert_eq!(
        booted.supervisor.alive_count(),
        1,
        "only the example is alive"
    );
    let plan = booted.supervisor.subscription_plan();
    assert_eq!(
        plan.block_chains.iter().map(|c| c.id()).collect::<Vec<_>>(),
        vec![1],
        "the alive module's chain survives; the dead module's does not",
    );
    assert_eq!(
        plan.viability(0),
        Viability::Live,
        "one live subscription keeps the plan viable despite the dead module",
    );
}

/// Declares two subscription kinds and opens no event source for either.
struct Ticker;

impl Extension<CoreRuntime> for Ticker {
    fn namespace(&self) -> &'static str {
        "ticker"
    }
    fn capabilities(&self) -> manifest::NamespaceCaps {
        manifest::NamespaceCaps {
            prefix: "test:ticker/",
            ifaces: &[],
        }
    }
    fn link(&self, _linker: &mut Linker<HostState<CoreRuntime>>) -> anyhow::Result<()> {
        Ok(())
    }
    fn subscriptions(&self) -> &'static [&'static str] {
        &["alarms", "ticks"]
    }
}

/// One health filter covers extension kinds too: a dead module's kind opens
/// no extension event source, while a live module's survives.
#[tokio::test]
async fn dead_module_extension_kind_is_excluded_from_the_plan() {
    let Some(price_alert_wasm) = module_wasm_or_skip("price-alert") else {
        return;
    };
    let Some(example_wasm) = example_wasm_or_skip() else {
        return;
    };
    let booted = BootScenario::new()
        .extensions([Arc::new(Ticker) as Arc<dyn Extension<CoreRuntime>>])
        .module(
            Entry::new(price_alert("not-a-number").extension_sub("alarms", &[]))
                .wasm(price_alert_wasm),
        )
        .module(
            Entry::new(
                TestManifest::new("example")
                    .cap("logging")
                    .extension_sub("ticks", &[]),
            )
            .wasm(example_wasm),
        )
        .boot()
        .await
        .expect("both modules load; only price-alert's init fails");

    let plan = booted.supervisor.subscription_plan();
    assert_eq!(
        plan.extension_kinds
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["ticks"],
        "the dead module's kind is excluded; the alive module's survives",
    );
    assert_eq!(
        plan.viability(1),
        Viability::Live,
        "an opened extension source drives the engine",
    );
    assert_eq!(
        plan.viability(0),
        Viability::DeadHoldSubs,
        "with no source opened, the dead module's subscriptions are the only ones left",
    );
}

/// A declared kind is not a source: an extension that opens none leaves the
/// engine with nothing to drive, and no dead module to blame for it.
#[tokio::test]
async fn a_declared_extension_kind_alone_is_not_viable() {
    let Some(example_wasm) = example_wasm_or_skip() else {
        return;
    };
    let booted = BootScenario::new()
        .extensions([Arc::new(Ticker) as Arc<dyn Extension<CoreRuntime>>])
        .module(
            Entry::new(
                TestManifest::new("example")
                    .cap("logging")
                    .extension_sub("ticks", &[]),
            )
            .wasm(example_wasm),
        )
        .boot()
        .await
        .expect("the example boots alive");

    let plan = booted.supervisor.subscription_plan();
    assert_eq!(plan.extension_kinds.len(), 1, "the live kind is declared");
    assert_eq!(
        plan.viability(0),
        Viability::Nothing,
        "a declared but unopened kind must not park the engine on an empty select",
    );
}

/// Boot and restart share one instantiate-and-init helper, so the verdict on
/// the identical fault is the call sites' alone: dead forever, or deferred.
#[tokio::test]
async fn the_same_init_fault_kills_at_boot_and_only_defers_on_restart() {
    let Some(wasm) = module_wasm_or_skip("price-alert") else {
        return;
    };

    let booted = BootScenario::new()
        .wasm(wasm.clone())
        .module(price_alert("not-a-number"))
        .boot()
        .await
        .expect("the module loads; only init fails");
    let dead = &booted.supervisor.modules[0];
    assert!(!dead.health.dispatchable(), "a boot init fault loads dead");
    assert!(
        !dead
            .health
            .due_restart(Instant::now() + Duration::from_secs(3600)),
        "a boot init fault schedules no restart, ever",
    );

    let mut booted = BootScenario::new()
        .wasm(wasm)
        .module(price_alert("2500.50"))
        .boot()
        .await
        .expect("boot");
    assert!(
        booted.supervisor.modules[0].health.dispatchable(),
        "a parseable threshold loads alive",
    );
    // The revive re-runs `init` off the seed, so swapping the seed's config
    // reaches the restart path and nothing else.
    booted.supervisor.modules[0].seed.artifact.init_config = price_alert_config("not-a-number");
    let policy = booted.supervisor.policy;
    let died_at = Instant::now();
    booted.supervisor.modules[0]
        .health
        .record_trap(died_at, policy);
    let due = died_at + Duration::from_secs(5);
    sweep(
        &booted.supervisor.shared,
        std::slice::from_mut(&mut booted.supervisor.modules[0]),
        due,
    )
    .await;

    let module = &booted.supervisor.modules[0];
    assert_eq!(module.live.run.seq, 0, "a failed revive commits no run");
    assert_eq!(
        module.health.failure_count(),
        2,
        "one recorded trap plus one deferred restart",
    );
    assert!(
        module.health.due_restart(due + Duration::from_secs(3600)),
        "a restart init fault defers instead of killing",
    );
}

/// The host catches the bomb's trap without panicking, marks the module
/// dead, and never re-enters it.
async fn bomb_traps_and_marks_module_dead(module: &str) {
    let Some(wasm) = module_wasm_or_skip(module) else {
        return;
    };
    let mut booted = BootScenario::new()
        .wasm(wasm)
        .module(workspace_manifest(&format!(
            "modules/fixtures/{module}/component.toml"
        )))
        .boot()
        .await
        .expect("the bomb loads alive");
    assert_eq!(booted.supervisor.module_count(), 1);
    assert_eq!(booted.supervisor.alive_count(), 1, "{module} loads alive");

    assert_eq!(
        booted.dispatch_block_on(1).await,
        0,
        "{module} trapped, no module accepted the dispatch",
    );
    assert_eq!(
        booted.supervisor.alive_count(),
        0,
        "{module} is marked dead after the trap",
    );
    assert_eq!(
        booted.dispatch_block_on(1).await,
        0,
        "dead {module} excluded from the second dispatch",
    );
}

#[tokio::test]
async fn resource_limit_fuel_bomb_traps_and_marks_module_dead() {
    bomb_traps_and_marks_module_dead("fuel-bomb").await;
}

#[tokio::test]
async fn resource_limit_memory_bomb_traps_and_marks_module_dead() {
    bomb_traps_and_marks_module_dead("memory-bomb").await;
}

/// After the bomb traps, a healthy module beside it still receives every
/// dispatch on the shared chain.
#[tokio::test]
async fn resource_limit_dead_bomb_does_not_starve_healthy_module() {
    let Some(bomb_wasm) = module_wasm_or_skip("fuel-bomb") else {
        return;
    };
    let Some(example_wasm) = example_wasm_or_skip() else {
        return;
    };
    let mut booted = BootScenario::new()
        .module(
            Entry::new(workspace_manifest(
                "modules/fixtures/fuel-bomb/component.toml",
            ))
            .wasm(bomb_wasm),
        )
        .module(
            Entry::new(TestManifest::new("example").cap("logging").block_sub(1)).wasm(example_wasm),
        )
        .boot()
        .await
        .expect("boot");

    assert_eq!(booted.supervisor.module_count(), 2);
    assert_eq!(booted.supervisor.alive_count(), 2, "both load alive");

    // The bomb traps; the example dispatches normally on the same block.
    assert_eq!(
        booted.dispatch_block_on(1).await,
        1,
        "example module received the dispatch even though fuel-bomb trapped",
    );
    assert_eq!(
        booted.supervisor.alive_count(),
        1,
        "only the example is alive"
    );

    // Only the example accepts; the dead bomb is skipped.
    assert_eq!(booted.dispatch_block_on(1).await, 1);
    assert_eq!(booted.supervisor.alive_count(), 1);
}

/// Real wall-clock; `fail_first_n = 1` keeps it under 2 s.
#[tokio::test(start_paused = true)]
async fn restart_flaky_module_recovers_after_backoff() {
    let Some(wasm) = module_wasm_or_skip("flaky-bomb") else {
        return;
    };
    let mut booted = BootScenario::new()
        .wasm(wasm)
        .module(
            TestManifest::new("flaky-bomb")
                .cap("logging")
                .cap("local-store")
                .block_sub(1)
                .config("fail_first_n", "1"),
        )
        .boot()
        .await
        .expect("boot");
    assert_eq!(booted.supervisor.alive_count(), 1);

    // Dispatch 1: trap. Module marked dead with a +1s backoff.
    assert_eq!(
        booted.dispatch_block_on(1).await,
        0,
        "first dispatch trapped, no module accepted",
    );
    assert_eq!(booted.supervisor.alive_count(), 0, "module marked dead");

    // Immediate redispatch (under the 1s backoff): still skipped.
    assert_eq!(
        booted.dispatch_block_on(1).await,
        0,
        "in-backoff module not eligible for redispatch yet",
    );
    assert_eq!(booted.supervisor.alive_count(), 0);

    // Past the 1 s backoff; the paused clock makes this instant.
    tokio::time::sleep(Duration::from_millis(1100)).await;

    // Now eligible; fail_first_n=1 was satisfied on dispatch 1, so this
    // attempt succeeds and the failure count resets.
    assert_eq!(
        booted.dispatch_block_on(1).await,
        1,
        "module recovered after the backoff window",
    );
    assert_eq!(booted.supervisor.alive_count(), 1, "recovered + alive");

    // Dispatch 4: steady-state, no backoff in play.
    assert_eq!(booted.dispatch_block_on(1).await, 1);
}

/// Tight policy (3 failures / 60 s) inside ~4 s of wall clock. The 1.2 s
/// probe pins the asymmetry: a module restart keeps the count, so trap 2 earns 2 s.
#[tokio::test(start_paused = true)]
async fn poison_pill_quarantines_module_after_threshold() {
    let Some(wasm) = module_wasm_or_skip("fuel-bomb") else {
        return;
    };
    let mut booted = BootScenario::new()
        .limits(ModuleLimits {
            poison: crate::engine_config::PoisonLimitsSection {
                max_failures: Some(3),
                window_secs: Some(60),
            },
            ..Default::default()
        })
        .wasm(wasm)
        .module(workspace_manifest(
            "modules/fixtures/fuel-bomb/component.toml",
        ))
        .boot()
        .await
        .expect("boot");

    assert_eq!(booted.supervisor.module_count(), 1);
    assert_eq!(booted.supervisor.alive_count(), 1);
    assert_eq!(booted.supervisor.poisoned_count(), 0);

    // Trap 1.
    assert_eq!(booted.dispatch_block_on(1).await, 0);
    assert_eq!(booted.supervisor.alive_count(), 0);
    assert_eq!(booted.supervisor.poisoned_count(), 0, "1 trap < threshold");
    tokio::time::sleep(Duration::from_millis(1_100)).await;

    // Trap 2.
    assert_eq!(booted.dispatch_block_on(1).await, 0);
    assert_eq!(booted.supervisor.poisoned_count(), 0, "2 traps < threshold");

    // The restart before trap 2 kept the count, so the curve climbed to 2 s
    // and nothing is due here; a resetting restart would land trap 3 now.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(booted.dispatch_block_on(1).await, 0);
    assert_eq!(
        booted.supervisor.poisoned_count(),
        0,
        "no restart is due 1.2 s into a 2 s backoff",
    );
    tokio::time::sleep(Duration::from_millis(1_000)).await;

    // Trap 3: poisoned.
    assert_eq!(booted.dispatch_block_on(1).await, 0);
    assert_eq!(
        booted.supervisor.poisoned_count(),
        1,
        "3 traps inside window quarantine the module",
    );

    // A poisoned module is excluded regardless of elapsed time; the backoff
    // timer is no longer load-bearing.
    assert_eq!(
        booted.dispatch_block_on(1).await,
        0,
        "poisoned module excluded from dispatch forever",
    );
    assert_eq!(booted.supervisor.poisoned_count(), 1);
}
