//! Restart, backoff, and poison machinery for modules and providers. A
//! trapped instance is poisoned, so recovery is always a fresh store plus
//! re-instantiation and `init`; poisoned components stay quarantined until
//! an operator removes them and restarts the engine.

use anyhow::{Context, Error, Result, anyhow};
use tracing::{error, info, warn};

use super::Supervisor;
use super::load::{LoadedModule, LoadedProvider, run_init};
use super::store::{self, build_linker, build_provider_linker};
use crate::bindings::EventModule;
use crate::host::component::RuntimeTypes;
use crate::host::extension::{HostServices, Installed, ProviderInstance};
use crate::host::logs::RunId;

impl<T: RuntimeTypes> Supervisor<T> {
    /// Rebuild a trapped module from its cached seed on a fresh `Store`
    /// (the trapped instance is poisoned) and re-run `init`, preserving
    /// name and subscriptions. On success the caller flips `alive`; on
    /// failure the module stays dead and its failure count keeps climbing.
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

    /// Re-instantiate a dead module in place. On success mark it `alive`;
    /// on failure bump the counter and slide `next_attempt` per the backoff.
    pub(super) async fn try_restart(&mut self, idx: usize) {
        let name = self.modules[idx].name.clone();
        let failure_count = self.modules[idx].failure_count;
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
                self.modules[idx].alive = true;
                info!(module = %name, "restart succeeded");
            }
            Err(e) => {
                // Re-instantiation failed: bump the backoff again so
                // the next attempt is further out.
                let m = &mut self.modules[idx];
                m.failure_count = m.failure_count.saturating_add(1);
                let backoff = crate::runtime::restart_policy::backoff_for(m.failure_count);
                m.next_attempt = Some(std::time::Instant::now() + backoff);
                error!(
                    module = %name,
                    failure_count = m.failure_count,
                    backoff_ms = backoff.as_millis() as u64,
                    error = %e,
                    "restart failed - will retry after backoff",
                );
            }
        }
    }

    /// Fold providers into recovery: record any trap the shared liveness
    /// reports (backoff plus poison), then reinstall dead, unpoisoned
    /// providers past their backoff. Runs at the head of every dispatch.
    pub(super) async fn sweep_providers(&mut self) {
        let now = std::time::Instant::now();
        let policy = self.policy;
        for idx in 0..self.providers.len() {
            let provider = &mut self.providers[idx];
            if provider.alive
                && let Some(died_at) = provider.liveness.dead_since()
            {
                provider.alive = false;
                provider.failure_count = provider.failure_count.saturating_add(1);
                let backoff = crate::runtime::restart_policy::backoff_for(provider.failure_count);
                // Backoff counts from the death, not from this sweep, so a
                // trap whose backoff already elapsed restarts right below.
                provider.next_attempt = Some(died_at.checked_add(backoff).unwrap_or(now));
                warn!(
                    adapter = %provider.name,
                    failure_count = provider.failure_count,
                    backoff_ms = backoff.as_millis() as u64,
                    "adapter trapped - marked dead; will restart after backoff",
                );
                metrics::counter!(
                    "shepherd_adapter_errors_total",
                    "adapter" => provider.name.clone(),
                    "error_kind" => "trap",
                )
                .increment(1);
                if let Some(recent) = poison_crossed(&mut provider.failure_timestamps, policy)
                    && !provider.poisoned
                {
                    provider.poisoned = true;
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
            let provider = &self.providers[idx];
            if !provider.poisoned
                && !provider.alive
                && provider.next_attempt.is_some_and(|t| t <= now)
            {
                self.try_restart_provider(idx).await;
            }
        }
    }

    /// Reinstall a dead provider in place (fresh store, instance, `init`,
    /// re-install). On success revive the shared liveness; on failure slide
    /// the backoff.
    pub(super) async fn try_restart_provider(&mut self, idx: usize) {
        let name = self.providers[idx].name.clone();
        let failure_count = self.providers[idx].failure_count;
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
                provider.alive = true;
                provider.failure_count = 0;
                provider.next_attempt = None;
                info!(adapter = %name, "adapter restart succeeded");
            }
            Ok(Installed::Dead) => {
                defer_provider_restart(provider, "init returned fault on restart");
            }
            Err(e) => defer_provider_restart(provider, &format!("{e:#}")),
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

/// Push the current trap timestamp into a component's failure-window ring,
/// drop entries older than the window, and report the recent count once it
/// crosses `policy.max_failures`.
fn poison_crossed(
    failure_timestamps: &mut std::collections::VecDeque<std::time::Instant>,
    policy: crate::runtime::poison_policy::PoisonPolicy,
) -> Option<u32> {
    let now = std::time::Instant::now();
    while let Some(&front) = failure_timestamps.front() {
        if now.duration_since(front) > policy.window {
            failure_timestamps.pop_front();
        } else {
            break;
        }
    }
    failure_timestamps.push_back(now);
    let recent = failure_timestamps.len() as u32;
    crate::runtime::poison_policy::should_poison(policy, recent).then_some(recent)
}

/// Flip `poisoned` once the module's failure window crosses the threshold;
/// the first transition emits the gauge and a WARN.
pub(super) fn record_failure_and_maybe_poison<T: RuntimeTypes>(
    module: &mut LoadedModule<T>,
    policy: crate::runtime::poison_policy::PoisonPolicy,
    last_error: &str,
) {
    if let Some(recent) = poison_crossed(&mut module.failure_timestamps, policy)
        && !module.poisoned
    {
        module.poisoned = true;
        warn!(
            module = %module.name,
            recent_failures = recent,
            window_secs = policy.window.as_secs(),
            last_error,
            "module poisoned - quarantined; remove from engine.toml + restart to clear",
        );
        metrics::gauge!(
            "shepherd_module_poisoned",
            "module" => module.name.clone(),
        )
        .set(1.0);
    }
}

/// Slide a failed provider restart's next attempt further out.
fn defer_provider_restart(provider: &mut LoadedProvider, error: &str) {
    provider.failure_count = provider.failure_count.saturating_add(1);
    let backoff = crate::runtime::restart_policy::backoff_for(provider.failure_count);
    provider.next_attempt = Some(std::time::Instant::now() + backoff);
    error!(
        adapter = %provider.name,
        failure_count = provider.failure_count,
        backoff_ms = backoff.as_millis() as u64,
        error,
        "adapter restart failed - will retry after backoff",
    );
}
