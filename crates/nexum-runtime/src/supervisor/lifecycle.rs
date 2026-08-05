//! Restart, backoff, and poison machinery for modules and providers.
//! [`Health`] is the single lifecycle authority for both roles; a trapped
//! instance recovers only via a fresh store plus re-instantiation and
//! `init`, and a poisoned component stays quarantined until an operator
//! removes it and restarts the engine.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Error, Result, anyhow};
use tracing::{error, info, warn};

use super::Supervisor;
use super::load::{LoadedProvider, run_init};
use super::store::{self, build_linker, build_provider_linker};
use crate::bindings::EventModule;
use crate::host::component::RuntimeTypes;
use crate::host::extension::{HostServices, Installed, ProviderInstance};
use crate::host::logs::RunId;
use crate::runtime::poison_policy::{PoisonPolicy, should_poison};
use crate::runtime::restart_policy::backoff_for;

/// Lifecycle state of one supervised component. `Poisoned` is terminal for
/// the process; recovery needs an operator-driven engine restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr)]
pub(crate) enum LifecycleState {
    /// Callable; the failure count beside it may still be nonzero.
    Alive,
    /// Dead pending a restart once `until` passes.
    Backoff { until: Instant },
    /// Dead with no scheduled restart (a failed boot-time `init`).
    Dead,
    /// Quarantined by the poison policy; never dispatched or restarted.
    Poisoned,
}

/// Single lifecycle authority for a module or provider: state, the
/// consecutive-failure count driving the backoff curve, and the sliding
/// failure window driving the poison threshold. The count sits beside the
/// state because alive-with-failures is a real baseline: a restart does not
/// reset the curve unless the caller says so, only a successful dispatch
/// does. Every method takes its instants explicitly; nothing here samples
/// the clock.
pub(crate) struct Health {
    state: LifecycleState,
    failure_count: u32,
    window: VecDeque<Instant>,
}

/// What one recorded trap did to a component's health, for the caller's
/// telemetry. `poisoned` carries the recent-failure count only on the
/// transition into quarantine.
pub(super) struct TrapVerdict {
    pub(super) failure_count: u32,
    pub(super) backoff: Duration,
    pub(super) poisoned: Option<u32>,
}

/// A deferred restart: the bumped count and the backoff it produced.
pub(super) struct Deferral {
    pub(super) failure_count: u32,
    pub(super) backoff: Duration,
}

impl Health {
    /// Health of a component that loaded and initialised.
    pub(super) fn alive() -> Self {
        Self {
            state: LifecycleState::Alive,
            failure_count: 0,
            window: VecDeque::new(),
        }
    }

    /// Health of a component whose boot-time `init` failed: dead with no
    /// scheduled restart.
    pub(super) fn dead() -> Self {
        Self {
            state: LifecycleState::Dead,
            ..Self::alive()
        }
    }

    /// Whether the component may be dispatched to right now.
    pub(super) fn dispatchable(&self) -> bool {
        matches!(self.state, LifecycleState::Alive)
    }

    /// Whether the component is quarantined.
    pub(super) fn is_poisoned(&self) -> bool {
        matches!(self.state, LifecycleState::Poisoned)
    }

    /// Whether a scheduled restart is due at `now`.
    pub(super) fn due_restart(&self, now: Instant) -> bool {
        matches!(self.state, LifecycleState::Backoff { until } if until <= now)
    }

    /// Consecutive failures since the last reset.
    pub(super) fn failure_count(&self) -> u32 {
        self.failure_count
    }

    /// Record one trap: bump the count, push `now` into the poison window,
    /// and enter backoff counted from `died_at` (a provider's death can
    /// predate the sweep that notices it) or quarantine past the threshold.
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

    /// A restart attempt failed: bump the count and slide the backoff out
    /// from `now`. Restart failures never feed the poison window.
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

    /// A restart succeeded. The failure count survives unless the caller's
    /// role resets it, so a still-crashing component keeps climbing the
    /// backoff curve.
    pub(super) fn restart_succeeded(&mut self, reset_failures: bool) {
        self.state = LifecycleState::Alive;
        if reset_failures {
            self.failure_count = 0;
        }
    }

    /// A dispatch succeeded: the component is back in steady state, so the
    /// failure count resets.
    pub(super) fn dispatch_succeeded(&mut self) {
        self.failure_count = 0;
    }
}

impl<T: RuntimeTypes> Supervisor<T> {
    /// Rebuild a trapped module from its cached seed on a fresh `Store`
    /// (the trapped instance is poisoned) and re-run `init`, preserving
    /// name and subscriptions. On success the caller marks its health
    /// alive; on failure the module stays dead and its failure count keeps
    /// climbing.
    pub(super) async fn reinstantiate_one(&mut self, idx: usize) -> Result<()> {
        // Re-build the linker: core interfaces plus every extension hook,
        // identical to the boot-time linker. Cheap `add_to_linker` calls
        // against the cached `Engine`.
        let linker = build_linker::<T>(&self.shared.engine, &self.shared.extensions)?;

        // Disjoint borrows: the shared backends stay borrowed while the
        // module slot is rebuilt in place.
        let Self {
            shared, modules, ..
        } = self;
        let module = &mut modules[idx];
        // A restart is a new run: bump the sequence so its logs key
        // apart from the dead run's, which stays readable until evicted.
        let run = RunId::new(module.name.clone(), module.live.run.seq + 1);
        let mut store = store::build(
            shared,
            &module.seed.spec,
            run.clone(),
            shared.services.clone(),
        )?;
        let bindings =
            EventModule::instantiate_async(&mut store, &module.seed.artifact.component, &linker)
                .await
                .map_err(Error::from)
                .with_context(|| format!("reinstantiate {}", module.name))?;
        // Restart policy: an init fault defers the restart (backoff slides)
        // rather than loading the module permanently dead like boot does.
        match run_init(
            &bindings,
            &mut store,
            &module.seed.artifact.init_config,
            module.seed.event_deadline,
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
        module.live.bindings = bindings;
        module.live.store = store;
        module.live.run = run;
        Ok(())
    }

    /// Re-instantiate a dead module in place. On success mark it alive
    /// (keeping the failure count); on failure defer with a slid backoff.
    pub(super) async fn try_restart(&mut self, idx: usize, now: Instant) {
        let name = self.modules[idx].name.clone();
        let failure_count = self.modules[idx].health.failure_count();
        // Restarts reuse the cached component, so the boot-time digest holds.
        info!(
            module = %name,
            failure_count,
            digest = %self.modules[idx].seed.artifact.digest,
            "restart attempt",
        );
        metrics::counter!(
            "shepherd_module_restarts_total",
            "module" => name.clone(),
        )
        .increment(1);
        match self.reinstantiate_one(idx).await {
            Ok(()) => {
                self.modules[idx].health.restart_succeeded(false);
                info!(module = %name, "restart succeeded");
            }
            Err(e) => {
                let deferral = self.modules[idx].health.defer_restart(now);
                error!(
                    module = %name,
                    failure_count = deferral.failure_count,
                    backoff_ms = deferral.backoff.as_millis() as u64,
                    error = %e,
                    "restart failed - will retry after backoff",
                );
            }
        }
    }

    /// Fold providers into recovery: record any trap the shared liveness
    /// reports (backoff plus poison), then reinstall dead, unpoisoned
    /// providers past their backoff. Runs at the head of every dispatch.
    pub(super) async fn sweep_providers(&mut self, now: Instant) {
        let policy = self.policy;
        for idx in 0..self.providers.len() {
            let provider = &mut self.providers[idx];
            if provider.health.dispatchable()
                && let Some(died_at) = provider.liveness.dead_since()
            {
                // Backoff counts from the death, not from this sweep, so a
                // trap whose backoff already elapsed restarts right below.
                let verdict = provider.health.record_trap(died_at, now, policy);
                warn!(
                    adapter = %provider.name,
                    failure_count = verdict.failure_count,
                    backoff_ms = verdict.backoff.as_millis() as u64,
                    "adapter trapped - marked dead; will restart after backoff",
                );
                metrics::counter!(
                    "shepherd_adapter_errors_total",
                    "adapter" => provider.name.clone(),
                    "error_kind" => "trap",
                )
                .increment(1);
                if let Some(recent) = verdict.poisoned {
                    warn!(
                        adapter = %provider.name,
                        recent_failures = recent,
                        window_secs = policy.window.as_secs(),
                        "adapter poisoned - quarantined; remove from engine.toml + restart to clear",
                    );
                    metrics::gauge!(
                        "shepherd_adapter_poisoned",
                        "adapter" => provider.name.clone(),
                    )
                    .set(1.0);
                }
            }
            if self.providers[idx].health.due_restart(now) {
                self.try_restart_provider(idx, now).await;
            }
        }
    }

    /// Reinstall a dead provider in place (fresh store, instance, `init`,
    /// re-install). On success revive the shared liveness and reset the
    /// failure count; on failure defer with a slid backoff.
    pub(super) async fn try_restart_provider(&mut self, idx: usize, now: Instant) {
        let name = self.providers[idx].name.clone();
        let failure_count = self.providers[idx].health.failure_count();
        info!(
            adapter = %name,
            failure_count,
            digest = %self.providers[idx].seed.artifact.digest,
            "adapter restart attempt",
        );
        metrics::counter!(
            "shepherd_adapter_restarts_total",
            "adapter" => name.clone(),
        )
        .increment(1);
        let outcome = self.reinstall_provider(idx).await;
        let provider = &mut self.providers[idx];
        match outcome {
            Ok(Installed::Live) => {
                provider.run_seq += 1;
                provider.liveness.mark_alive();
                provider.health.restart_succeeded(true);
                info!(adapter = %name, "adapter restart succeeded");
            }
            Ok(Installed::Dead) => {
                defer_provider_restart(provider, now, "init returned fault on restart");
            }
            Err(e) => defer_provider_restart(provider, now, &format!("{e:#}")),
        }
    }

    /// Rebuild a provider from its cached seed and reinstall it over the
    /// dead slot.
    async fn reinstall_provider(&mut self, idx: usize) -> Result<Installed> {
        let provider = &self.providers[idx];
        let (kind, service) = self
            .shared
            .kinds
            .get(provider.kind)
            .ok_or_else(|| anyhow!("provider kind {} is not registered", provider.kind))?;
        let linker = build_provider_linker::<T>(&self.shared.engine, kind.as_ref())?;
        // A restart is a new run, like a module's.
        let run = RunId::new(provider.name.clone(), provider.run_seq + 1);
        let store = store::build(
            &self.shared,
            &provider.seed.spec,
            run,
            HostServices::default(),
        )?;
        kind.install(
            ProviderInstance {
                component: &provider.seed.artifact.component,
                linker: &linker,
                store,
                config: provider.seed.artifact.init_config.clone(),
                sections: &provider.sections,
                fuel_per_call: provider.seed.spec.fuel,
                liveness: provider.liveness.clone(),
            },
            service,
        )
        .await
    }
}

/// Slide a failed provider restart's next attempt further out.
fn defer_provider_restart(provider: &mut LoadedProvider, now: Instant, error: &str) {
    let deferral = provider.health.defer_restart(now);
    error!(
        adapter = %provider.name,
        failure_count = deferral.failure_count,
        backoff_ms = deferral.backoff.as_millis() as u64,
        error,
        "adapter restart failed - will retry after backoff",
    );
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
}
