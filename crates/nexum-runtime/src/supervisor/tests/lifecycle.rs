//! Lifecycle: init failure, traps, restart backoff, and poison quarantine.

use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::supervisor::lifecycle::sweep;

/// price-alert loads cleanly, then `init` rejects the unparseable
/// `threshold` with `fault.invalid-input`.
fn bad_threshold_price_alert() -> TestManifest {
    TestManifest::new("price-alert")
        .cap("logging")
        .cap("chain")
        .block_sub(SEPOLIA)
        .config(
            "oracle_address",
            "0x694AA1769357215DE4FAC081bf1f309aDC325306",
        )
        .config("decimals", "8")
        .config("threshold", "not-a-number")
        .config("direction", "below")
        .config("every_n_blocks", "1")
}

/// A module whose `init` fails is loaded but marked dead: it takes no
/// dispatch, neither its block nor its chain-log subscription reaches the
/// chain-facing lists, and the dropped subscriptions stay attributable.
#[tokio::test]
async fn init_failure_marks_module_dead_excluding_dispatch_and_subscriptions() {
    let Some(wasm) = module_wasm_or_skip("price-alert") else {
        return;
    };
    // Both a block and a filtered chain-log subscription, so the test
    // exercises both filter paths rather than a trivially empty chain-log
    // list.
    let mut booted = BootScenario::new()
        .wasm(wasm)
        .module(bad_threshold_price_alert().chain_log_sub_filtered(
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
    assert!(
        booted.supervisor.block_chains().is_empty(),
        "dead module must not contribute to block_chains()",
    );
    assert!(
        booted.supervisor.chain_log_subscriptions().is_empty(),
        "dead module must not contribute to chain_log_subscriptions()",
    );
    assert!(
        booted.supervisor.dead_modules_hold_subscriptions(),
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
    let booted = BootScenario::new()
        .module(Entry::new(bad_threshold_price_alert()).wasm(price_alert_wasm))
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
    let chains = booted.supervisor.block_chains();
    assert_eq!(
        chains.iter().map(|c| c.id()).collect::<Vec<_>>(),
        vec![1],
        "the alive module's chain survives; the dead module's does not",
    );
    assert!(
        booted.supervisor.dead_modules_hold_subscriptions(),
        "the dead module's dropped subscription is attributable",
    );
}

/// The bomb fixture traps on its first dispatch (fuel exhaustion or a
/// rejected `memory.grow`): the host catches the trap without panicking,
/// marks the module dead, and never re-enters it.
async fn bomb_traps_and_marks_module_dead(module: &str) {
    let Some(wasm) = module_wasm_or_skip(module) else {
        return;
    };
    let mut booted = BootScenario::new()
        .wasm(wasm)
        .module(workspace_manifest(&format!(
            "modules/fixtures/{module}/module.toml"
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

/// Isolation invariant: after the bomb traps, a healthy module beside it
/// still receives every dispatch on the shared chain.
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
            Entry::new(workspace_manifest("modules/fixtures/fuel-bomb/module.toml"))
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

    // First dispatch: fuel-bomb burns through its budget and traps; the
    // example module dispatches normally on the same block.
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

    // Second dispatch: only the example accepts; the dead bomb is
    // skipped by the dispatch fast-path.
    assert_eq!(booted.dispatch_block_on(1).await, 1);
    assert_eq!(booted.supervisor.alive_count(), 1);
}

/// Full restart lifecycle with real wall-clock: trap, in-backoff skip,
/// restart after the window, then steady state. `fail_first_n = 1` keeps
/// the wall-clock under 2 s.
#[tokio::test]
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

    // Wait for the 1s backoff window to elapse (+ a small fudge for
    // scheduler jitter).
    tokio::time::sleep(Duration::from_millis(1100)).await;

    // Dispatch 3: now eligible. fail_first_n=1 was satisfied on
    // dispatch 1, so this attempt succeeds. The supervisor flips
    // alive back on, dispatch lands, failure_count resets.
    assert_eq!(
        booted.dispatch_block_on(1).await,
        1,
        "module recovered after the backoff window",
    );
    assert_eq!(booted.supervisor.alive_count(), 1, "recovered + alive");

    // Dispatch 4: steady-state, no backoff in play.
    assert_eq!(booted.dispatch_block_on(1).await, 1);
}

/// Escalation from retry to permanent quarantine under a tight poison
/// policy (3 failures / 60 s), inside ~4 s of wall clock:
///
///   trap 1: failure_count=1, backoff +1s
///   sleep 1.1s
///   trap 2: failure_count=2, backoff +2s
///   sleep 1.2s, probe: no restart is due yet
///   sleep 1.0s
///   trap 3: failure_count=3, poisoned; restarts stop entirely and
///           subsequent dispatches skip the module silently.
///
/// The 1.2 s probe pins the module asymmetry: a successful restart keeps
/// the failure count, so trap 2 earns 2 s rather than another 1 s.
#[tokio::test]
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
        .module(workspace_manifest("modules/fixtures/fuel-bomb/module.toml"))
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

    // Probe inside trap 2's backoff. The restart that preceded trap 2
    // kept the failure count, so the curve climbed to 2 s and nothing is
    // due here. A restart that reset the count would leave a 1 s backoff,
    // land trap 3 in this dispatch, and quarantine the module early.
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

    // Post-quarantine: a poisoned module is excluded regardless of how
    // much time has passed; the backoff timer is no longer load-bearing.
    assert_eq!(
        booted.dispatch_block_on(1).await,
        0,
        "poisoned module excluded from dispatch forever",
    );
    assert_eq!(booted.supervisor.poisoned_count(), 1);
}

/// A provider kind whose `install` outcome the test flips, so one sweep
/// sees a dead reinstall and the next a live one.
struct ScriptedKind(Arc<AtomicBool>);

#[async_trait::async_trait]
impl ProviderKind<TestTypes> for ScriptedKind {
    fn kind(&self) -> &'static str {
        "scripted-adapter"
    }

    fn link(&self, _linker: &mut Linker<HostState<TestTypes>>) -> anyhow::Result<()> {
        Ok(())
    }

    async fn install(
        &self,
        _instance: ProviderInstance<'_, TestTypes>,
        _service: &Arc<dyn HostService>,
    ) -> anyhow::Result<Installed> {
        Ok(if self.0.load(Ordering::SeqCst) {
            Installed::Live
        } else {
            Installed::Dead
        })
    }
}

struct ScriptedService;
impl HostService for ScriptedService {}

struct ScriptedExtension(Arc<AtomicBool>);

impl Extension<TestTypes> for ScriptedExtension {
    fn namespace(&self) -> &'static str {
        "scripted"
    }

    fn capabilities(&self) -> manifest::NamespaceCaps {
        manifest::NamespaceCaps {
            prefix: "test:scripted/",
            ifaces: &[],
        }
    }

    fn link(&self, _linker: &mut Linker<HostState<TestTypes>>) -> anyhow::Result<()> {
        Ok(())
    }

    fn service(&self) -> Option<Arc<dyn HostService>> {
        Some(Arc::new(ScriptedService))
    }

    fn provider(&self) -> Option<Box<dyn ProviderKind<TestTypes>>> {
        Some(Box::new(ScriptedKind(self.0.clone())))
    }
}

/// The shared wiring the sweep reinstalls through, with the scripted kind
/// registered. The `TempDir` keeps the local store alive.
fn scripted_shared(live: &Arc<AtomicBool>) -> (tempfile::TempDir, Shared<TestTypes>) {
    let (dir, store) = temp_local_store();
    let extensions: Vec<Arc<dyn Extension<TestTypes>>> =
        vec![Arc::new(ScriptedExtension(live.clone()))];
    let services = HostServices::from_extensions(&extensions).expect("services");
    let kinds =
        crate::supervisor::admission::provider_kinds(&extensions, &services).expect("kinds");
    let shared = Shared {
        engine: test_wasmtime_engine(),
        components: test_components(store),
        extensions,
        services,
        kinds,
        clocks: None,
    };
    (dir, shared)
}

/// A booted provider at run 0. The component is an empty valid one: the
/// scripted kind never instantiates it.
fn scripted_provider(engine: &wasmtime::Engine) -> crate::supervisor::load::LoadedProvider {
    const EMPTY_COMPONENT: &[u8] = b"(component)";
    let limits = ModuleLimits::default();
    crate::supervisor::load::LoadedProvider {
        name: "scripted".into(),
        kind: "scripted-adapter",
        sections: manifest::ExtensionSections::default(),
        seed: crate::supervisor::load::ProviderSeed {
            artifact: crate::supervisor::load::CachedArtifact {
                component: wasmtime::component::Component::new(engine, EMPTY_COMPONENT)
                    .expect("empty component"),
                digest: ContentDigest::of_bytes(EMPTY_COMPONENT),
                init_config: Vec::new(),
            },
            spec: crate::supervisor::store::StoreSpec {
                http_allowlist: Vec::new(),
                http_limits: limits.http(),
                messaging_topics: Vec::new(),
                memory_limit: limits.memory(),
                fuel: limits.fuel(),
                chain_response_max_bytes: limits.chain_response_max_bytes(),
                state_quota: limits.state_bytes(),
            },
        },
        liveness: crate::host::actor::Liveness::default(),
        run: crate::host::logs::RunId::new("scripted", 0),
        health: crate::supervisor::lifecycle::Health::alive(),
    }
}

/// The sweep mints a provider's successor run inside the reinstall and
/// commits it only when the install comes back live: a dead reinstall
/// defers with the run, the liveness, and the failure curve untouched,
/// so the next attempt reuses the sequence rather than burning it.
#[tokio::test]
async fn a_dead_provider_reinstall_defers_without_committing_a_run() {
    let live = Arc::new(AtomicBool::new(false));
    let (_dir, shared) = scripted_shared(&live);
    let mut provider = scripted_provider(&shared.engine);
    let policy = crate::runtime::poison_policy::PoisonPolicy::new(9, Duration::from_secs(600));

    // The actor trapped: the sweep discovers the death through the shared
    // liveness and counts the 1 s backoff from the death instant.
    provider.liveness.mark_dead();
    let died_at = provider.liveness.dead_since().expect("marked dead");
    let first = died_at + Duration::from_secs(5);
    sweep(&shared, std::slice::from_mut(&mut provider), policy, first).await;

    assert_eq!(provider.run.seq, 0, "a dead reinstall commits no run");
    assert!(
        !provider.liveness.is_alive(),
        "a dead reinstall leaves the liveness dead",
    );
    assert_eq!(
        provider.health.failure_count(),
        2,
        "one recorded trap plus one deferred restart",
    );

    live.store(true, Ordering::SeqCst);
    let second = first + Duration::from_secs(10);
    sweep(&shared, std::slice::from_mut(&mut provider), policy, second).await;

    assert_eq!(
        provider.run.seq, 1,
        "the live reinstall commits the successor the failed attempt minted",
    );
    assert!(provider.liveness.is_alive());
    assert!(provider.health.dispatchable());
    assert_eq!(
        provider.health.failure_count(),
        0,
        "a reinstall is a fresh instance, so the curve resets",
    );
}
