//! Restart, backoff, and poison machinery for modules.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

use anyhow::{Error, Result, anyhow};
use nexum_tasks::Shutdown;
use tracing::{error, info};

use super::Shared;
use super::load::{LoadedModule, instantiate_module};
use super::store::{build_linker, fresh_run_store};
use crate::engine_config::{PoisonPolicy, should_poison};
use crate::runtime::restart_policy::{backoff_for, jitter_seed};
use nexum_primitives::digest::ContentDigest;
use nexum_primitives::module_id::ModuleId;
use nexum_runtime_api::RuntimeTypes;
use nexum_runtime_wasm::HostState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LifecycleState {
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

/// The failure count survives a restart; only a successful dispatch clears
/// it. Methods take instants; nothing samples the clock.
pub(super) struct Health {
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

    /// The boot verdict: a failed `init` loads the item dead, permanently.
    pub(super) fn from_init(ok: bool) -> Self {
        if ok { Self::alive() } else { Self::dead() }
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

    /// `now` is the death instant the dispatch stamped; `seed` decorrelates
    /// the backoff across modules.
    pub(super) fn record_trap(
        &mut self,
        now: Instant,
        policy: PoisonPolicy,
        seed: u64,
    ) -> TrapVerdict {
        self.failure_count = self.failure_count.saturating_add(1);
        let backoff = backoff_for(self.failure_count, seed);
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
                until: now.checked_add(backoff).unwrap_or(now),
            }
        };
        TrapVerdict {
            failure_count: self.failure_count,
            backoff,
            poisoned: (crossed && !already_poisoned).then_some(recent),
        }
    }

    /// Backoff slides from `now`; restart failures never feed the poison window.
    pub(super) fn defer_restart(&mut self, now: Instant, seed: u64) -> Deferral {
        self.failure_count = self.failure_count.saturating_add(1);
        let backoff = backoff_for(self.failure_count, seed);
        self.state = LifecycleState::Backoff {
            until: now.checked_add(backoff).unwrap_or(now),
        };
        Deferral {
            failure_count: self.failure_count,
            backoff,
        }
    }

    /// A module recovers in place, so the failure curve keeps climbing;
    /// only a successful dispatch resets it.
    fn restart_succeeded(&mut self) {
        self.state = LifecycleState::Alive;
    }

    pub(super) fn dispatch_succeeded(&mut self) {
        self.failure_count = 0;
    }
}

/// Run identity mints and commits only inside a successful [`Sweepable::revive`];
/// a failed attempt never advances the run sequence.
pub(super) trait Sweepable<T: RuntimeTypes> {
    fn name(&self) -> &ModuleId;
    fn health(&self) -> &Health;
    fn health_mut(&mut self) -> &mut Health;
    fn digest(&self) -> ContentDigest;
    /// Must rebuild on a fresh store; the trapped one is poisoned.
    async fn revive(&mut self, shared: &Shared<T>) -> Result<()>;
}

impl<T: RuntimeTypes<State = HostState<T>>> Sweepable<T> for LoadedModule<T> {
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

    /// Bindings, store, and run commit only on success.
    async fn revive(&mut self, shared: &Shared<T>) -> Result<()> {
        // Must match the boot-time linker: core interfaces plus every extension hook.
        let linker = build_linker::<T>(&shared.engine, &shared.extensions)?;
        // A restart is a new run; the dead run's logs stay readable until evicted.
        let (run, mut store) =
            fresh_run_store(shared, &self.name, self.live.run.seq + 1, &self.seed.spec)?;
        let (bindings, init) =
            instantiate_module(&linker, &self.seed, &self.name, &mut store).await?;
        // An init fault defers the restart; only at boot is it permanent.
        if let Err(e) = init {
            return Err(anyhow!(
                "init returned fault on restart: {} ({})",
                nexum_runtime_wasm::fault_message(&e),
                nexum_runtime_wasm::fault_label(&e),
            ));
        }
        self.live.bindings = bindings;
        self.live.store = store;
        self.live.run = run;
        Ok(())
    }
}

pub(super) async fn sweep<T: RuntimeTypes, S: Sweepable<T>>(
    shared: &Shared<T>,
    items: &mut [S],
    now: Instant,
    stop: Option<&Shutdown>,
) {
    for item in items.iter_mut() {
        // Each revive runs a deadline-bounded `init`; a fired stop must not
        // start another.
        if stop.is_some_and(Shutdown::is_fired) {
            return;
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
    // Revives reuse the cached component, so the boot-time digest holds.
    report_restart_attempt(item.name(), item.health().failure_count(), item.digest());
    match item.revive(shared).await {
        Ok(()) => {
            item.health_mut().restart_succeeded();
            report_restart_outcome(item.name(), Ok(()));
        }
        Err(e) => {
            let seed = jitter_seed(item.name().as_str());
            let deferral = item.health_mut().defer_restart(now, seed);
            report_restart_outcome(item.name(), Err((deferral, e)));
        }
    }
}

fn report_restart_attempt(name: &ModuleId, failure_count: u32, digest: ContentDigest) {
    info!(
        module = %name,
        failure_count,
        digest = %digest,
        "restart attempt",
    );
    metrics::counter!(
        "nexum_runtime_module_restarts_total",
        "module" => name.clone(),
    )
    .increment(1);
}

fn report_restart_outcome(name: &ModuleId, outcome: Result<(), (Deferral, Error)>) {
    match outcome {
        Ok(()) => info!(module = %name, "restart succeeded"),
        Err((deferral, e)) => {
            // A string field, not a `Display` one: it carries the full
            // context chain through the string visitor.
            let error = format!("{e:#}");
            error!(
                module = %name,
                failure_count = deferral.failure_count,
                backoff_ms = deferral.backoff.as_millis() as u64,
                error,
                "restart failed - will retry after backoff",
            );
        }
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;

    fn policy(max_failures: u32, window_secs: u64) -> PoisonPolicy {
        PoisonPolicy::new(
            std::num::NonZeroU32::new(max_failures).unwrap(),
            Duration::from_secs(window_secs),
        )
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    const SEED: u64 = 0x5eed;

    fn assert_backoff_base(backoff: Duration, base: Duration) {
        assert!(
            (base / 2..=base).contains(&backoff),
            "{backoff:?} outside [{:?}, {base:?}]",
            base / 2,
        );
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
        let verdict = health.record_trap(t0, policy(5, 600), SEED);
        assert_eq!(verdict.failure_count, 1);
        assert_backoff_base(verdict.backoff, secs(1));
        assert!(verdict.poisoned.is_none());
        assert!(!health.dispatchable());
        assert!(!health.due_restart(t0 + verdict.backoff - Duration::from_millis(1)));
        assert!(health.due_restart(t0 + verdict.backoff));
    }

    #[test]
    fn consecutive_traps_climb_the_backoff_curve() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        assert_backoff_base(
            health.record_trap(t0, policy(9, 600), SEED).backoff,
            secs(1),
        );
        assert_backoff_base(
            health
                .record_trap(t0 + secs(2), policy(9, 600), SEED)
                .backoff,
            secs(2),
        );
        assert_backoff_base(
            health
                .record_trap(t0 + secs(6), policy(9, 600), SEED)
                .backoff,
            secs(4),
        );
    }

    #[test]
    fn a_restart_keeps_the_failure_curve() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        health.record_trap(t0, policy(9, 600), SEED);
        health.record_trap(t0 + secs(2), policy(9, 600), SEED);
        health.restart_succeeded();
        assert!(health.dispatchable());
        assert_eq!(health.failure_count(), 2);
        let verdict = health.record_trap(t0 + secs(9), policy(9, 600), SEED);
        assert_eq!(verdict.failure_count, 3);
        assert_backoff_base(verdict.backoff, secs(4));
    }

    #[test]
    fn dispatch_success_resets_the_count() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        health.record_trap(t0, policy(9, 600), SEED);
        health.restart_succeeded();
        health.dispatch_succeeded();
        assert_eq!(health.failure_count(), 0);
    }

    #[test]
    fn defer_slides_backoff_from_now_and_skips_the_poison_window() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        health.record_trap(t0, policy(2, 600), SEED);
        let deferral = health.defer_restart(t0 + secs(1), SEED);
        assert_eq!(deferral.failure_count, 2);
        assert_backoff_base(deferral.backoff, secs(2));
        assert!(!health.due_restart(t0 + secs(1) + deferral.backoff - Duration::from_millis(1)));
        assert!(health.due_restart(t0 + secs(1) + deferral.backoff));
        assert!(
            !health.is_poisoned(),
            "restart failures never feed the poison window",
        );
        let verdict = health.record_trap(t0 + secs(4), policy(2, 600), SEED);
        assert_eq!(verdict.poisoned, Some(2), "the second trap crosses");
    }

    #[test]
    fn poison_crosses_at_the_threshold_within_the_window() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        assert!(
            health
                .record_trap(t0, policy(3, 600), SEED)
                .poisoned
                .is_none()
        );
        assert!(
            health
                .record_trap(t0 + secs(1), policy(3, 600), SEED)
                .poisoned
                .is_none()
        );
        let verdict = health.record_trap(t0 + secs(2), policy(3, 600), SEED);
        assert_eq!(verdict.poisoned, Some(3));
        assert!(health.is_poisoned());
        assert!(!health.dispatchable());
        assert!(!health.due_restart(t0 + secs(3600)));
    }

    #[test]
    fn old_failures_age_out_of_the_window() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        health.record_trap(t0, policy(2, 10), SEED);
        let verdict = health.record_trap(t0 + secs(11), policy(2, 10), SEED);
        assert!(
            verdict.poisoned.is_none(),
            "the first failure aged out before the second landed",
        );
        let verdict = health.record_trap(t0 + secs(12), policy(2, 10), SEED);
        assert_eq!(verdict.poisoned, Some(2));
    }

    #[test]
    fn poisoned_is_terminal() {
        let t0 = Instant::now();
        let mut health = Health::alive();
        health.record_trap(t0, policy(1, 600), SEED);
        assert!(health.is_poisoned());
        let verdict = health.record_trap(t0 + secs(1), policy(1, 600), SEED);
        assert!(
            verdict.poisoned.is_none(),
            "only the transition reports the crossing",
        );
        assert!(health.is_poisoned());
        assert!(!health.due_restart(t0 + secs(3600)));
    }
}
