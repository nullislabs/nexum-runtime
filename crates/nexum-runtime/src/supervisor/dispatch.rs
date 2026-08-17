//! Trigger dispatch: rate limit, refuel, invoke `on_trigger` under the
//! wall-clock deadline, and record the outcome.

use std::time::Duration;

use tokio::time::Instant;

use alloy_chains::Chain;
use tracing::{debug, error, warn};
use tracing_core::Level;

use super::Supervisor;
use super::cursors::{commit_chain_log_cursor, persist_progress_marker};
use super::lifecycle::{revive_one, sweep};
use crate::bindings::nexum;
use crate::host::component::RuntimeTypes;
use crate::host::extension::ExtensionDelivery;
use crate::host::logs::{LogChannel, LogRecord};
use crate::manifest::Trigger;
use crate::module_id::ModuleId;

impl<T: RuntimeTypes> Supervisor<T> {
    /// The restart sweep runs first; returns the number of modules invoked.
    pub async fn dispatch_block(&mut self, block: nexum::host::types::Block) -> usize {
        let chain = Chain::from_id(block.chain_id);
        let chain_id = chain.id();
        let block_number = block.number;
        let trigger = nexum::host::types::Trigger::Block(block);
        let now = Instant::now();
        sweep(&self.shared, &mut self.modules, now, self.stop.as_ref()).await;

        let mut dispatched = 0;
        let candidate_indices: Vec<usize> = (0..self.modules.len())
            .filter(|&i| {
                let m = &self.modules[i];
                if !m.health.dispatchable() {
                    return false;
                }
                m.triggers
                    .iter()
                    .any(|t| matches!(t, Trigger::Block { chain_id: cid } if chain == *cid))
            })
            .collect();
        for (position, idx) in candidate_indices.iter().copied().enumerate() {
            // A stop drops the block for the modules after this one. The
            // fan-out order is `[[modules]]` order, so the same trailing
            // modules lose it at every stop: count and name them, or the
            // bias is invisible.
            if self.stop_requested() {
                let skipped = &candidate_indices[position..];
                for &i in skipped {
                    metrics::counter!(
                        "nexum_runtime_dispatch_dropped_total",
                        "module" => self.modules[i].name.to_string(),
                        "trigger_kind" => "block",
                        "reason" => "shutdown",
                    )
                    .increment(1);
                }
                warn!(
                    chain_id,
                    block_number,
                    skipped = skipped.len(),
                    modules = %skipped
                        .iter()
                        .map(|&i| self.modules[i].name.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                    "stop requested mid fan-out; these modules do not see this block",
                );
                break;
            }
            if matches!(
                self.dispatch_to(idx, chain_id, "block", block_number, &trigger, now)
                    .await,
                DispatchOutcome::Ok,
            ) {
                persist_progress_marker(
                    &self.shared.components.store,
                    self.modules[idx].name.as_str(),
                    chain,
                    block_number,
                );
                dispatched += 1;
            }
        }
        dispatched
    }

    /// Returns `true` only when the module accepted the log; the resume
    /// cursor persists only after a successful dispatch.
    pub async fn dispatch_event(
        &mut self,
        module_name: &ModuleId,
        chain: Chain,
        log: alloy_rpc_types_eth::Log,
        cursor_key: Option<&str>,
    ) -> bool {
        let now = Instant::now();
        // Skipped: the cursor stays put, so `resume = true` replays the
        // log at the next start. Counted anyway, so the shutdown reason is
        // one series rather than three behaviours.
        if self.stop_requested() {
            metrics::counter!(
                "nexum_runtime_dispatch_dropped_total",
                "module" => module_name.to_string(),
                "trigger_kind" => "event",
                "reason" => "shutdown",
            )
            .increment(1);
            return false;
        }
        let Some(idx) = self.modules.iter().position(|m| m.name == *module_name) else {
            warn!(module = %module_name, "no such module - dropping event");
            return false;
        };

        // Poison check first: a poisoned module never triggers a restart attempt.
        if self.modules[idx].health.is_poisoned() {
            return false;
        }

        // The event hot path revives only its own module, never the rest.
        if self.modules[idx].health.due_restart(now) {
            revive_one(&self.shared, &mut self.modules[idx], now).await;
        }

        // The revive above is deadline-bounded; re-probe so a stop during it
        // does not start a second bounded call.
        if self.stop_requested() || !self.modules[idx].health.dispatchable() {
            return false;
        }

        let block_number = log.block_number;
        let removed = log.removed;
        let trigger = nexum::host::types::Trigger::Event(super::sources::wit_log(&log, chain));
        let ok = matches!(
            self.dispatch_to(
                idx,
                chain.id(),
                "event",
                block_number.unwrap_or_default(),
                &trigger,
                now,
            )
            .await,
            DispatchOutcome::Ok,
        );
        if ok && let (Some(key), Some(block)) = (cursor_key, block_number) {
            commit_chain_log_cursor(
                &self.shared.components.store,
                &mut self.chain_log_cursors,
                module_name.as_str(),
                key,
                block,
                removed,
            );
        }
        ok
    }

    /// The restart sweep runs first; returns the number of modules invoked.
    pub async fn dispatch_extension_trigger(&mut self, delivery: ExtensionDelivery) -> usize {
        let now = Instant::now();
        sweep(&self.shared, &mut self.modules, now, self.stop.as_ref()).await;

        let candidate_indices: Vec<usize> = (0..self.modules.len())
            .filter(|&i| {
                let m = &self.modules[i];
                if !m.health.dispatchable() {
                    return false;
                }
                m.triggers.iter().any(|t| {
                    matches!(
                        t,
                        Trigger::Extension { extension_kind, filters }
                            if extension_kind == delivery.extension_kind
                                && filters.iter().all(|(fk, fv)| {
                                    delivery.attrs.iter().any(|(ak, av)| ak == fk && av == fv)
                                })
                    )
                })
            })
            .collect();
        let mut dispatched = 0;
        for (position, idx) in candidate_indices.iter().copied().enumerate() {
            if self.stop_requested() {
                for &i in &candidate_indices[position..] {
                    metrics::counter!(
                        "nexum_runtime_dispatch_dropped_total",
                        "module" => self.modules[i].name.to_string(),
                        "trigger_kind" => delivery.extension_kind,
                        "reason" => "shutdown",
                    )
                    .increment(1);
                }
                break;
            }
            // Extension deliveries are not chain-scoped; telemetry carries the 0 sentinel.
            if matches!(
                self.dispatch_to(idx, 0, delivery.extension_kind, 0, &delivery.trigger, now)
                    .await,
                DispatchOutcome::Ok,
            ) {
                dispatched += 1;
            }
        }
        dispatched
    }

    /// `chain_id` is telemetry only; chain-less kinds pass 0.
    async fn dispatch_to(
        &mut self,
        idx: usize,
        chain_id: u64,
        trigger_kind: &'static str,
        block_number: u64,
        trigger: &nexum::host::types::Trigger,
        now: Instant,
    ) -> DispatchOutcome {
        let poison_policy = self.policy;
        // Hoisted before the per-module borrow so the trap arm can
        // synthesize a panic record without re-borrowing `self`.
        let router = self.shared.components.logs.router();
        let module = &mut self.modules[idx];
        // Throttle before spending fuel or entering the guest; the bucket is
        // per-module, so a throttled module never starves the others.
        if !module.live.dispatch_bucket.try_acquire(now) {
            debug!(
                module = %module.name,
                chain_id,
                trigger_kind,
                block_number,
                "dispatch rate limit exceeded - dropping trigger",
            );
            metrics::counter!(
                "nexum_runtime_dispatch_dropped_total",
                "module" => module.name.to_string(),
                "trigger_kind" => trigger_kind,
                "reason" => "rate_limited",
            )
            .increment(1);
            return DispatchOutcome::RateLimited;
        }
        if let Err(e) = module.live.store.set_fuel(module.seed.spec.fuel) {
            error!(
                module = %module.name,
                chain_id,
                trigger_kind,
                error = %e,
                "set_fuel failed - skipping"
            );
            return DispatchOutcome::FuelSetFailed;
        }
        let start = Instant::now();
        // A deadline hit is fatal like a trap: cancellation leaves the store
        // unusable, so the trap arm must mark the module dead.
        let deadline = module.seed.dispatch_deadline;
        let call = module
            .live
            .bindings
            .call_on_trigger(&mut module.live.store, trigger);
        let outcome = with_dispatch_deadline(deadline, call)
            .await
            .unwrap_or_else(|exceeded| Err(wasmtime::Error::from(exceeded)));
        // One post-call sample: the trap instant is start plus elapsed, not
        // the pre-dispatch `now`. This is the same clock the lifecycle
        // reads, so under `start_paused` the latency histogram records the
        // virtual elapsed time: do not assert on it from a paused test.
        let elapsed = start.elapsed();
        let latency_ms = elapsed.as_millis() as u64;
        match outcome {
            Ok(Ok(())) => {
                debug!(
                    module = %module.name,
                    chain_id,
                    trigger_kind,
                    block_number,
                    latency_ms,
                    "dispatch ok"
                );
                metrics::histogram!(
                    "nexum_runtime_event_latency_seconds",
                    "module" => module.name.to_string(),
                    "trigger_kind" => trigger_kind,
                )
                .record(elapsed.as_secs_f64());
                module.health.dispatch_succeeded();
                DispatchOutcome::Ok
            }
            Ok(Err(fault)) => {
                let kind = crate::host::fault::fault_label(&fault);
                warn!(
                    module = %module.name,
                    chain_id,
                    trigger_kind,
                    block_number,
                    latency_ms,
                    kind,
                    message = %crate::host::fault::fault_message(&fault),
                    "on-trigger returned fault",
                );
                metrics::counter!(
                    "nexum_runtime_module_errors_total",
                    "module" => module.name.clone(),
                    "error_kind" => kind,
                )
                .increment(1);
                DispatchOutcome::Fault
            }
            Err(trap) => {
                // The module died when the call ended, not at entry.
                let died_at = start + elapsed;
                let seed = crate::runtime::restart_policy::jitter_seed(module.name.as_str());
                let verdict = module.health.record_trap(died_at, poison_policy, seed);
                error!(
                    module = %module.name,
                    chain_id,
                    trigger_kind,
                    block_number,
                    latency_ms,
                    failure_count = verdict.failure_count,
                    backoff_ms = verdict.backoff.as_millis() as u64,
                    error = %trap,
                    "on-trigger trapped - module marked dead; will retry after backoff",
                );
                metrics::counter!(
                    "nexum_runtime_module_errors_total",
                    "module" => module.name.clone(),
                    "error_kind" => "trap",
                )
                .increment(1);
                // Leave a retrievable panic record on the dead run; the full
                // trap already went to host tracing.
                router.record(LogRecord::now(
                    module.live.run.clone(),
                    LogChannel::Panic,
                    Level::ERROR,
                    format!("run terminated abnormally: {}", trap.root_cause()),
                ));
                if let Some(recent) = verdict.poisoned {
                    report_poison(&module.name, recent, poison_policy.window, trap.to_string());
                }
                DispatchOutcome::Trapped
            }
        }
    }
}

/// The poison-transition trio: quarantine warn plus gauge, with the trap
/// that crossed the threshold.
fn report_poison(name: &ModuleId, recent_failures: u32, window: Duration, last_error: String) {
    warn!(
        module = %name,
        recent_failures,
        window_secs = window.as_secs(),
        last_error,
        "module poisoned - quarantined; remove from engine.toml + restart to clear",
    );
    metrics::gauge!(
        "nexum_runtime_module_poisoned",
        "module" => name.clone(),
    )
    .set(1.0);
}

/// Distinct from a fuel trap, which bounds guest instructions; this bounds
/// the whole dispatch, host calls included, in wall-clock.
#[derive(Debug, thiserror::Error)]
#[error(
    "dispatch exceeded its {0:?} wall-clock deadline \
     (a host call blocked or ran too long)"
)]
pub(super) struct DeadlineExceeded(Duration);

/// Cancellation lands at the future's next await point, so pure guest
/// spinning stays fuel's job (see [`crate::engine_config::ModuleLimits`]).
pub(super) async fn with_dispatch_deadline<F: std::future::Future>(
    deadline: Duration,
    fut: F,
) -> Result<F::Output, DeadlineExceeded> {
    tokio::time::timeout(deadline, fut)
        .await
        .map_err(|_elapsed| DeadlineExceeded(deadline))
}

#[derive(Debug)]
pub(super) enum DispatchOutcome {
    Ok,
    /// Guest returned a typed `fault` via WIT, not a trap; the module stays alive.
    Fault,
    /// Marked dead, maybe quarantined per the poison policy.
    Trapped,
    /// `set_fuel` failed before the call; the module stays alive, the trigger skips.
    FuelSetFailed,
    /// Dropped before the guest runs; liveness untouched.
    RateLimited,
}
