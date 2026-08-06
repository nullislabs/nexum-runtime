//! Restart, backoff, and poison machinery for modules and providers.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Error, Result, anyhow};
use tracing::{error, info, warn};

use super::Shared;
use super::load::{LoadedModule, LoadedProvider, run_init};
use super::store::{self, build_linker, build_provider_linker};
use crate::bindings::EventModule;
use crate::digest::ContentDigest;
use crate::host::actor::Liveness;
use crate::host::component::RuntimeTypes;
use crate::host::extension::{HostServices, Installed, ProviderInstance};
use crate::host::logs::RunId;
use crate::module_id::ModuleId;
use crate::runtime::poison_policy::{PoisonPolicy, should_poison};
use crate::runtime::restart_policy::backoff_for;

#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr)]
pub(crate) enum LifecycleState {
    /// Callable; the failure count beside it may still be nonzero.
    Alive,
    /// Dead pending a restart once `until` passes.
    Backoff { until: Instant },
    /// Dead with no scheduled restart (a failed boot-time `init`).
    Dead,
    /// Terminal for the process; never dispatched or restarted, cleared only
    /// by an operator-driven engine restart.
    Poisoned,
}

/// The failure count survives a restart unless the caller resets it; only a
/// successful dispatch clears it. Methods take instants; nothing samples the clock.
pub(crate) struct Health {
    state: LifecycleState,
    failure_count: u32,
    window: VecDeque<Instant>,
}

/// `poisoned` carries the recent-failure count only on the transition into
/// quarantine.
pub(super) struct TrapVerdict {
    pub(super) failure_count: u32,
    pub(super) backoff: Duration,
    pub(super) poisoned: Option<u32>,
}

pub(super) struct Deferral {
    pub(super) failure_count: u32,
    pub(super) backoff: Duration,
}

impl Health {
    pub(super) fn alive() -> Self {
        Self {
            state: LifecycleState::Alive,
            failure_count: 0,
            window: VecDeque::new(),
        }
    }

    pub(super) fn dead() -> Self {
        Self {
            state: LifecycleState::Dead,
            ..Self::alive()
        }
    }

    pub(super) fn dispatchable(&self) -> bool {
        matches!(self.state, LifecycleState::Alive)
    }

    pub(super) fn is_poisoned(&self) -> bool {
        matches!(self.state, LifecycleState::Poisoned)
    }

    pub(super) fn due_restart(&self, now: Instant) -> bool {
        matches!(self.state, LifecycleState::Backoff { until } if until <= now)
    }

    pub(super) fn failure_count(&self) -> u32 {
        self.failure_count
    }

    /// Backoff counts from `died_at`, not `now`: a provider's death can
    /// predate the sweep that notices it.
    pub(super) fn record_trap(
        &mut self,
        died_at: Instant,
        now: Instant,
        policy: PoisonPolicy,
    ) -> TrapVerdict {
        self.failure_count = self.failure_count.saturating_add(1);
        let backoff = backoff_for(self.failure_count);
        while let Some(&front) = self.window.front() {
            if now.duration_since(front) > policy.window {
                self.window.pop_front();
            } else {
                break;
            }
        }
        self.window.push_back(now);
        let recent = self.window.len() as u32;
        let already_poisoned = self.is_poisoned();
        let crossed = should_poison(policy, recent);
        self.state = if crossed || already_poisoned {
            LifecycleState::Poisoned
        } else {
            LifecycleState::Backoff {
                until: died_at.checked_add(backoff).unwrap_or(now),
            }
        };
        TrapVerdict {
            failure_count: self.failure_count,
            backoff,
            poisoned: (crossed && !already_poisoned).then_some(recent),
        }
    }

    /// Backoff slides from `now`; restart failures never feed the poison window.
    pub(super) fn defer_restart(&mut self, now: Instant) -> Deferral {
        self.failure_count = self.failure_count.saturating_add(1);
        let backoff = backoff_for(self.failure_count);
        self.state = LifecycleState::Backoff {
            until: now.checked_add(backoff).unwrap_or(now),
        };
        Deferral {
            failure_count: self.failure_count,
            backoff,
        }
    }

    /// The reset choice comes only from [`Role::resets_failure_count`].
    fn restart_succeeded(&mut self, reset_failures: bool) {
        self.state = LifecycleState::Alive;
        if reset_failures {
            self.failure_count = 0;
        }
    }

    pub(super) fn dispatch_succeeded(&mut self) {
        self.failure_count = 0;
    }
}

/// Keys the per-role metric names and the compile-time tracing field keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr)]
pub(super) enum Role {
    Module,
    Adapter,
}

impl Role {
    const fn label(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Adapter => "adapter",
        }
    }

    const fn errors_total(self) -> &'static str {
        match self {
            Self::Module => "nexum_runtime_module_errors_total",
            Self::Adapter => "nexum_runtime_adapter_errors_total",
        }
    }

    const fn restarts_total(self) -> &'static str {
        match self {
            Self::Module => "nexum_runtime_module_restarts_total",
            Self::Adapter => "nexum_runtime_adapter_restarts_total",
        }
    }

    const fn poisoned_gauge(self) -> &'static str {
        match self {
            Self::Module => "nexum_runtime_module_poisoned",
            Self::Adapter => "nexum_runtime_adapter_poisoned",
        }
    }

    /// A provider reinstall is a fresh instance, so its curve resets; a
    /// module recovers in place and keeps climbing.
    const fn resets_failure_count(self) -> bool {
        matches!(self, Self::Adapter)
    }
}

/// Run identity mints and commits only inside a successful [`Sweepable::revive`];
/// a failed attempt never advances the run sequence.
pub(super) trait Sweepable<T: RuntimeTypes> {
    const ROLE: Role;
    fn name(&self) -> &ModuleId;
    fn health(&self) -> &Health;
    fn health_mut(&mut self) -> &mut Health;
    fn digest(&self) -> ContentDigest;
    /// A death `health` has not recorded yet, with its instant.
    fn detect_death(&self) -> Option<Instant>;
    fn poison_policy(&self, engine_default: PoisonPolicy) -> PoisonPolicy {
        engine_default
    }
    /// Must rebuild on a fresh store; the trapped one is poisoned.
    async fn revive(&mut self, shared: &Shared<T>) -> Result<()>;
}

impl<T: RuntimeTypes> Sweepable<T> for LoadedModule<T> {
    const ROLE: Role = Role::Module;

    fn name(&self) -> &ModuleId {
        &self.name
    }

    fn health(&self) -> &Health {
        &self.health
    }

    fn health_mut(&mut self) -> &mut Health {
        &mut self.health
    }

    fn digest(&self) -> ContentDigest {
        self.seed.artifact.digest
    }

    /// Module traps are recorded eagerly by the dispatch trap arm, so the
    /// sweep never discovers one.
    fn detect_death(&self) -> Option<Instant> {
        None
    }

    /// Bindings, store, and run commit only on success.
    async fn revive(&mut self, shared: &Shared<T>) -> Result<()> {
        // Must match the boot-time linker: core interfaces plus every extension hook.
        let linker = build_linker::<T>(&shared.engine, &shared.extensions)?;
        // A restart is a new run; the dead run's logs stay readable until evicted.
        let run = RunId::new(self.name.clone(), self.live.run.seq + 1);
        let mut store = store::build(
            shared,
            &self.seed.spec,
            run.clone(),
            shared.services.clone(),
        )?;
        let bindings =
            EventModule::instantiate_async(&mut store, &self.seed.artifact.component, &linker)
                .await
                .map_err(Error::from)
                .with_context(|| format!("reinstantiate {}", self.name))?;
        // An init fault defers the restart; only at boot is it permanent.
        match run_init(
            &bindings,
            &mut store,
            &self.seed.artifact.init_config,
            self.seed.event_deadline,
        )
        .await?
        {
            Ok(()) => {}
            Err(e) => {
                return Err(anyhow!(
                    "init returned fault on restart: {} ({})",
                    crate::host::error::fault_message(&e),
                    crate::host::error::fault_label(&e),
                ));
            }
        }
        self.live.bindings = bindings;
        self.live.store = store;
        self.live.run = run;
        Ok(())
    }
}

impl<T: RuntimeTypes> Sweepable<T> for LoadedProvider {
    const ROLE: Role = Role::Adapter;

    fn name(&self) -> &ModuleId {
        &self.name
    }

    fn health(&self) -> &Health {
        &self.health
    }

    fn health_mut(&mut self) -> &mut Health {
        &mut self.health
    }

    fn digest(&self) -> ContentDigest {
        self.seed.artifact.digest
    }

    /// The shared liveness reports deaths the dispatch path cannot see;
    /// the health gate records each one exactly once.
    fn detect_death(&self) -> Option<Instant> {
        provider_death(&self.health, &self.liveness)
    }

    /// Run and liveness commit only on a live install.
    async fn revive(&mut self, shared: &Shared<T>) -> Result<()> {
        let (kind, service) = shared
            .kinds
            .get(self.kind)
            .ok_or_else(|| anyhow!("provider kind {} is not registered", self.kind))?;
        let linker = build_provider_linker::<T>(&shared.engine, kind.as_ref())?;
        let run = RunId::new(self.name.clone(), self.run.seq + 1);
        let store = store::build(
            shared,
            &self.seed.spec,
            run.clone(),
            HostServices::default(),
        )?;
        match kind
            .install(
                ProviderInstance {
                    component: &self.seed.artifact.component,
                    linker: &linker,
                    store,
                    config: self.seed.artifact.init_config.clone(),
                    sections: &self.sections,
                    fuel_per_call: self.seed.spec.fuel,
                    liveness: self.liveness.clone(),
                },
                service,
            )
            .await?
        {
            Installed::Live => {
                self.run = run;
                self.liveness.mark_alive();
                Ok(())
            }
            Installed::Dead => Err(anyhow!("init returned fault on restart")),
        }
    }
}

/// A provider's unrecorded death: liveness dead while health still says alive,
/// timed from the death instant rather than the sweep that noticed it.
fn provider_death(health: &Health, liveness: &Liveness) -> Option<Instant> {
    if !health.dispatchable() {
        return None;
    }
    liveness.dead_since()
}

/// Deaths are recorded before revival, so an already-elapsed backoff revives
/// in the same pass.
pub(super) async fn sweep<T: RuntimeTypes, S: Sweepable<T>>(
    shared: &Shared<T>,
    items: &mut [S],
    engine_default: PoisonPolicy,
    now: Instant,
) {
    for item in items.iter_mut() {
        let policy = item.poison_policy(engine_default);
        if let Some(died_at) = item.detect_death() {
            let verdict = item.health_mut().record_trap(died_at, now, policy);
            match S::ROLE {
                Role::Module => warn!(
                    module = %item.name(),
                    failure_count = verdict.failure_count,
                    backoff_ms = verdict.backoff.as_millis() as u64,
                    "module trapped - marked dead; will restart after backoff",
                ),
                Role::Adapter => warn!(
                    adapter = %item.name(),
                    failure_count = verdict.failure_count,
                    backoff_ms = verdict.backoff.as_millis() as u64,
                    "adapter trapped - marked dead; will restart after backoff",
                ),
            }
            metrics::counter!(
                S::ROLE.errors_total(),
                S::ROLE.label() => item.name().clone(),
                "error_kind" => "trap",
            )
            .increment(1);
            if let Some(recent) = verdict.poisoned {
                match S::ROLE {
                    Role::Module => warn!(
                        module = %item.name(),
                        recent_failures = recent,
                        window_secs = policy.window.as_secs(),
                        "module poisoned - quarantined; remove from engine.toml + restart to clear",
                    ),
                    Role::Adapter => warn!(
                        adapter = %item.name(),
                        recent_failures = recent,
                        window_secs = policy.window.as_secs(),
                        "adapter poisoned - quarantined; remove from engine.toml + restart to clear",
                    ),
                }
                metrics::gauge!(
                    S::ROLE.poisoned_gauge(),
                    S::ROLE.label() => item.name().clone(),
                )
                .set(1.0);
            }
        }
        if item.health().due_restart(now) {
            revive_one(shared, item, now).await;
        }
    }
}

pub(super) async fn revive_one<T: RuntimeTypes, S: Sweepable<T>>(
    shared: &Shared<T>,
    item: &mut S,
    now: Instant,
) {
    let failure_count = item.health().failure_count();
    // Revives reuse the cached component, so the boot-time digest holds.
    match S::ROLE {
        Role::Module => info!(
            module = %item.name(),
            failure_count,
            digest = %item.digest(),
            "restart attempt",
        ),
        Role::Adapter => info!(
            adapter = %item.name(),
            failure_count,
            digest = %item.digest(),
            "adapter restart attempt",
        ),
    }
    metrics::counter!(
        S::ROLE.restarts_total(),
        S::ROLE.label() => item.name().clone(),
    )
    .increment(1);
    match item.revive(shared).await {
        Ok(()) => {
            item.health_mut()
                .restart_succeeded(S::ROLE.resets_failure_count());
            match S::ROLE {
                Role::Module => info!(module = %item.name(), "restart succeeded"),
                Role::Adapter => info!(adapter = %item.name(), "adapter restart succeeded"),
            }
        }
        Err(e) => {
            let deferral = item.health_mut().defer_restart(now);
            match S::ROLE {
                Role::Module => error!(
                    module = %item.name(),
                    failure_count = deferral.failure_count,
                    backoff_ms = deferral.backoff.as_millis() as u64,
                    error = %e,
                    "restart failed - will retry after backoff",
                ),
                Role::Adapter => {
                    // A string field, not a `Display` one: the two record
                    // through different visitor methods and print differently.
                    let error = format!("{e:#}");
                    error!(
                        adapter = %item.name(),
                        failure_count = deferral.failure_count,
                        backoff_ms = deferral.backoff.as_millis() as u64,
                        error,
                        "adapter restart failed - will retry after backoff",
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;

    fn policy(max_failures: u32, window_secs: u64) -> PoisonPolicy {
        PoisonPolicy::new(max_failures, Duration::from_secs(window_secs))
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn alive_is_dispatchable_and_never_due() {
        let t0 = Instant::now();
        let health = Health::alive();
        assert!(health.dispatchable());
        assert!(!health.is_poisoned());
        assert!(!health.due_restart(t0 + secs(3600)));
        assert_eq!(health.failure_count(), 0);
    }

    #[test]
    fn dead_is_permanent() {
        let t0 = Instant::now();
        let health = Health::dead();
        assert!(!health.dispatchable());
        assert!(!health.is_poisoned());
        assert!(!health.due_restart(t0 + secs(3600)));
    }

    #[test]
    fn trap_enters_backoff_from_death_instant() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        let verdict = health.record_trap(t0, t0, policy(5, 600));
        assert_eq!(verdict.failure_count, 1);
        assert_eq!(verdict.backoff, secs(1));
        assert!(verdict.poisoned.is_none());
        assert!(!health.dispatchable());
        assert!(!health.due_restart(t0 + Duration::from_millis(999)));
        assert!(health.due_restart(t0 + secs(1)));
    }

    #[test]
    fn backoff_counts_from_death_not_from_the_sweep() {
        let t0 = Instant::now();
        let now = t0 + secs(5);
        let mut health = Health::alive();
        health.record_trap(t0, now, policy(5, 600));
        assert!(
            health.due_restart(now),
            "a death whose backoff already elapsed is due immediately",
        );
    }

    #[test]
    fn consecutive_traps_climb_the_backoff_curve() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        assert_eq!(health.record_trap(t0, t0, policy(9, 600)).backoff, secs(1));
        assert_eq!(
            health
                .record_trap(t0 + secs(2), t0 + secs(2), policy(9, 600))
                .backoff,
            secs(2),
        );
        assert_eq!(
            health
                .record_trap(t0 + secs(6), t0 + secs(6), policy(9, 600))
                .backoff,
            secs(4),
        );
    }

    #[test]
    fn restart_without_reset_keeps_the_curve() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        health.record_trap(t0, t0, policy(9, 600));
        health.record_trap(t0 + secs(2), t0 + secs(2), policy(9, 600));
        health.restart_succeeded(false);
        assert!(health.dispatchable());
        assert_eq!(health.failure_count(), 2);
        let verdict = health.record_trap(t0 + secs(9), t0 + secs(9), policy(9, 600));
        assert_eq!(verdict.failure_count, 3);
        assert_eq!(verdict.backoff, secs(4), "the curve kept climbing");
    }

    #[test]
    fn restart_with_reset_clears_the_curve() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        health.record_trap(t0, t0, policy(9, 600));
        health.record_trap(t0 + secs(2), t0 + secs(2), policy(9, 600));
        health.restart_succeeded(true);
        assert!(health.dispatchable());
        assert_eq!(health.failure_count(), 0);
        let verdict = health.record_trap(t0 + secs(9), t0 + secs(9), policy(9, 600));
        assert_eq!(verdict.failure_count, 1);
        assert_eq!(verdict.backoff, secs(1));
    }

    #[test]
    fn dispatch_success_resets_the_count() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        health.record_trap(t0, t0, policy(9, 600));
        health.restart_succeeded(false);
        health.dispatch_succeeded();
        assert_eq!(health.failure_count(), 0);
    }

    #[test]
    fn defer_slides_backoff_from_now_and_skips_the_poison_window() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        health.record_trap(t0, t0, policy(2, 600));
        let deferral = health.defer_restart(t0 + secs(1));
        assert_eq!(deferral.failure_count, 2);
        assert_eq!(deferral.backoff, secs(2));
        assert!(!health.due_restart(t0 + secs(2)));
        assert!(health.due_restart(t0 + secs(3)));
        assert!(
            !health.is_poisoned(),
            "restart failures never feed the poison window",
        );
        let verdict = health.record_trap(t0 + secs(4), t0 + secs(4), policy(2, 600));
        assert_eq!(verdict.poisoned, Some(2), "the second trap crosses");
    }

    #[test]
    fn poison_crosses_at_the_threshold_within_the_window() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        assert!(
            health
                .record_trap(t0, t0, policy(3, 600))
                .poisoned
                .is_none()
        );
        assert!(
            health
                .record_trap(t0 + secs(1), t0 + secs(1), policy(3, 600))
                .poisoned
                .is_none()
        );
        let verdict = health.record_trap(t0 + secs(2), t0 + secs(2), policy(3, 600));
        assert_eq!(verdict.poisoned, Some(3));
        assert!(health.is_poisoned());
        assert!(!health.dispatchable());
        assert!(!health.due_restart(t0 + secs(3600)));
    }

    #[test]
    fn old_failures_age_out_of_the_window() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        health.record_trap(t0, t0, policy(2, 10));
        let verdict = health.record_trap(t0 + secs(11), t0 + secs(11), policy(2, 10));
        assert!(
            verdict.poisoned.is_none(),
            "the first failure aged out before the second landed",
        );
        let verdict = health.record_trap(t0 + secs(12), t0 + secs(12), policy(2, 10));
        assert_eq!(verdict.poisoned, Some(2));
    }

    #[test]
    fn poisoned_is_terminal() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        health.record_trap(t0, t0, policy(1, 600));
        assert!(health.is_poisoned());
        let verdict = health.record_trap(t0 + secs(1), t0 + secs(1), policy(1, 600));
        assert!(
            verdict.poisoned.is_none(),
            "only the transition reports the crossing",
        );
        assert!(health.is_poisoned());
        assert!(!health.due_restart(t0 + secs(3600)));
    }

    #[test]
    fn a_provider_death_is_recorded_from_the_death_instant() {
        let liveness = Liveness::default();
        liveness.mark_dead();
        let died_at = liveness.dead_since().expect("marked dead");
        // Five seconds is well past the one-second backoff the first trap earns.
        let sweep = died_at + secs(5);
        let mut health = Health::alive();
        let died = provider_death(&health, &liveness).expect("an unrecorded death is a trap");
        let verdict = health.record_trap(died, sweep, policy(5, 600));
        assert_eq!(verdict.failure_count, 1);
        assert_eq!(verdict.backoff, secs(1));
        assert!(
            health.due_restart(sweep),
            "backoff runs from the death, not from the sweep that noticed it",
        );
    }

    #[test]
    fn a_provider_death_is_recorded_once() {
        let liveness = Liveness::default();
        let mut health = Health::alive();
        let now = Instant::now();
        assert!(
            provider_death(&health, &liveness).is_none(),
            "a live provider has nothing to record",
        );
        liveness.mark_dead();
        let died = provider_death(&health, &liveness).expect("marked dead");
        health.record_trap(died, now, policy(5, 600));
        assert!(
            provider_death(&health, &liveness).is_none(),
            "the liveness stays dead until the reinstall, so the gate is health",
        );
        assert_eq!(health.failure_count(), 1);
    }

    #[test]
    fn a_module_restart_keeps_the_curve_and_a_provider_restart_resets_it() {
        let t0 = Instant::now();
        let mut module = Health::alive();
        module.record_trap(t0, t0, policy(9, 600));
        module.record_trap(t0 + secs(2), t0 + secs(2), policy(9, 600));
        module.restart_succeeded(Role::Module.resets_failure_count());
        assert!(module.dispatchable());
        assert_eq!(module.failure_count(), 2);

        let mut provider = Health::alive();
        provider.record_trap(t0, t0, policy(9, 600));
        provider.record_trap(t0 + secs(2), t0 + secs(2), policy(9, 600));
        provider.restart_succeeded(Role::Adapter.resets_failure_count());
        assert!(provider.dispatchable());
        assert_eq!(provider.failure_count(), 0);
    }
}
