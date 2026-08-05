//! Event dispatch: rate limit, refuel, invoke `on_event` under the
//! wall-clock deadline, and record the outcome. Restart sweeps run at the
//! head of every entry point; progress and cursors persist only after a
//! successful dispatch.

use std::time::{Duration, Instant};

use alloy_chains::Chain;
use tracing::{debug, error, warn};
use tracing_core::Level;

use super::Supervisor;
use super::cursors::{commit_chain_log_cursor, persist_progress_marker};
use super::lifecycle::{revive_one, sweep};
use crate::bindings::nexum;
use crate::host::component::RuntimeTypes;
use crate::host::extension::ExtensionEvent;
use crate::host::logs::{LogRecord, LogSource};
use crate::manifest::Subscription;
use crate::module_id::ModuleId;

impl<T: RuntimeTypes> Supervisor<T> {
    /// Revive providers before modules at every multi-target entry point:
    /// a module revived first would re-run `init` against possibly-dead
    /// providers, so the sweep mirrors boot's providers-first install order.
    async fn sweep_all(&mut self, now: Instant) {
        sweep(&self.shared, &mut self.providers, self.policy, now).await;
        sweep(&self.shared, &mut self.modules, self.policy, now).await;
    }

    /// Dispatch one block to every alive module subscribed to its chain,
    /// restarting eligible dead components first. Returns the number
    /// invoked.
    pub async fn dispatch_block(&mut self, block: nexum::host::types::Block) -> usize {
        let chain = Chain::from_id(block.chain_id);
        let chain_id = chain.id();
        let block_number = block.number;
        let event = nexum::host::types::Event::Block(block);
        let now = Instant::now();
        self.sweep_all(now).await;

        let mut dispatched = 0;
        let candidate_indices: Vec<usize> = (0..self.modules.len())
            .filter(|&i| {
                let m = &self.modules[i];
                if !m.health.dispatchable() {
                    return false;
                }
                m.subscriptions
                    .iter()
                    .any(|s| matches!(s, Subscription::Block { chain_id: cid } if chain == *cid))
            })
            .collect();
        for idx in candidate_indices {
            if matches!(
                self.dispatch_to(idx, chain_id, "block", block_number, &event, now)
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

    /// Dispatch a chain-log event to the module that opened the
    /// subscription. Returns `true` when accepted; `false` when the module
    /// is dead, missing, or its callback failed. A trap marks it dead. The
    /// resume cursor persists only after a successful dispatch, so a block
    /// is never recorded as done before the module processed it.
    pub async fn dispatch_chain_log(
        &mut self,
        module_name: &ModuleId,
        chain: Chain,
        log: alloy_rpc_types_eth::Log,
        cursor_key: Option<&str>,
    ) -> bool {
        let now = Instant::now();
        sweep(&self.shared, &mut self.providers, self.policy, now).await;
        let Some(idx) = self.modules.iter().position(|m| m.name == *module_name) else {
            warn!(module = %module_name, "no such module - dropping chain-log");
            return false;
        };

        // Poison-pill check first, so a poisoned module never triggers a
        // restart attempt.
        if self.modules[idx].health.is_poisoned() {
            return false;
        }

        // Restart-on-trap, single-target: the chain-log hot path revives
        // only its own module, never sweeping the rest.
        if self.modules[idx].health.due_restart(now) {
            revive_one(&self.shared, &mut self.modules[idx], now).await;
        }

        if !self.modules[idx].health.dispatchable() {
            return false;
        }

        let block_number = log.block_number;
        let removed = log.removed;
        let event = nexum::host::types::Event::ChainLogs(nexum::host::types::ChainLogs {
            chain_id: chain.id(),
            logs: vec![nexum::host::types::ChainLog::from(&log)],
        });
        let ok = matches!(
            self.dispatch_to(
                idx,
                chain.id(),
                "chain-log",
                block_number.unwrap_or_default(),
                &event,
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

    /// Dispatch one extension event to every module whose subscription kind
    /// and filters match. Returns the number invoked. Like `dispatch_block`:
    /// dead modules past backoff restart first, poisoned modules skip.
    pub async fn dispatch_extension_event(&mut self, event: ExtensionEvent) -> usize {
        let now = Instant::now();
        self.sweep_all(now).await;

        let candidate_indices: Vec<usize> = (0..self.modules.len())
            .filter(|&i| {
                let m = &self.modules[i];
                if !m.health.dispatchable() {
                    return false;
                }
                m.subscriptions.iter().any(|s| {
                    matches!(
                        s,
                        Subscription::Extension { kind, filters }
                            if kind == event.kind && filters.iter().all(|(fk, fv)| {
                                event.attrs.iter().any(|(ak, av)| ak == fk && av == fv)
                            })
                    )
                })
            })
            .collect();
        let mut dispatched = 0;
        for idx in candidate_indices {
            // Extension events are not chain-scoped: the telemetry chain
            // id and block number carry the 0 sentinel.
            if matches!(
                self.dispatch_to(idx, 0, event.kind, 0, &event.event, now)
                    .await,
                DispatchOutcome::Ok,
            ) {
                dispatched += 1;
            }
        }
        dispatched
    }

    /// Shared per-module dispatch: refuel, call `on_event`, and record the
    /// outcome with the same telemetry and lifecycle bookkeeping. Only
    /// `Ok` counts as accepted; `RateLimited` and `FuelSetFailed` drop the
    /// event with the module left alive. `chain_id` is telemetry only;
    /// chain-less kinds pass 0.
    async fn dispatch_to(
        &mut self,
        idx: usize,
        chain_id: u64,
        event_kind: &'static str,
        block_number: u64,
        event: &nexum::host::types::Event,
        now: Instant,
    ) -> DispatchOutcome {
        let poison_policy = self.policy;
        // Hoisted before the per-module borrow so the trap arm can
        // synthesize a panic record without re-borrowing `self`.
        let router = self.shared.components.logs.router();
        let module = &mut self.modules[idx];
        // Dispatch-boundary rate limit: throttle before spending any fuel
        // or entering the guest. The bucket is per-module, so a throttled
        // module never starves the others; over-rate events are dropped
        // and counted with liveness untouched.
        if !module.live.dispatch_bucket.try_acquire(now) {
            debug!(
                module = %module.name,
                chain_id,
                event_kind,
                block_number,
                "dispatch rate limit exceeded - dropping event",
            );
            metrics::counter!(
                "nexum_runtime_dispatch_dropped_total",
                "module" => module.name.to_string(),
                "event_kind" => event_kind,
            )
            .increment(1);
            return DispatchOutcome::RateLimited;
        }
        if let Err(e) = module.live.store.set_fuel(module.seed.spec.fuel) {
            error!(
                module = %module.name,
                chain_id,
                event_kind,
                error = %e,
                "set_fuel failed - skipping"
            );
            return DispatchOutcome::FuelSetFailed;
        }
        let start = Instant::now();
        // Fuel bounds only guest instructions; time spent inside a host
        // call (chain RPC, redb, HTTP) is unmetered, so bound the whole
        // dispatch, guest plus every host call it awaits, in wall-clock.
        // A deadline hit is fatal like a trap: cancelling the call leaves
        // the store unusable, and the trap arm marks the module dead so
        // the restart sweep reinstantiates it on a fresh store.
        let deadline = module.seed.event_deadline;
        let call = module
            .live
            .bindings
            .call_on_event(&mut module.live.store, event);
        let outcome = with_dispatch_deadline(deadline, call)
            .await
            .unwrap_or_else(|exceeded| Err(wasmtime::Error::from(exceeded)));
        // One post-call sample, shared by latency telemetry and, in the
        // trap arm, the health transition (the trap instant is start +
        // elapsed, not the pre-dispatch `now`).
        let elapsed = start.elapsed();
        let latency_ms = elapsed.as_millis() as u64;
        match outcome {
            Ok(Ok(())) => {
                debug!(
                    module = %module.name,
                    chain_id,
                    event_kind,
                    block_number,
                    latency_ms,
                    "dispatch ok"
                );
                metrics::histogram!(
                    "nexum_runtime_event_latency_seconds",
                    "module" => module.name.to_string(),
                    "event_kind" => event_kind,
                )
                .record(elapsed.as_secs_f64());
                // Successful dispatch clears the failure history: a module
                // that recovered lands back in the steady-state schedule.
                module.health.dispatch_succeeded();
                DispatchOutcome::Ok
            }
            Ok(Err(fault)) => {
                let kind = crate::host::error::fault_label(&fault);
                warn!(
                    module = %module.name,
                    chain_id,
                    event_kind,
                    block_number,
                    latency_ms,
                    kind,
                    message = %crate::host::error::fault_message(&fault),
                    "on-event returned fault",
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
                let verdict = module.health.record_trap(died_at, died_at, poison_policy);
                error!(
                    module = %module.name,
                    chain_id,
                    event_kind,
                    block_number,
                    latency_ms,
                    failure_count = verdict.failure_count,
                    backoff_ms = verdict.backoff.as_millis() as u64,
                    error = %trap,
                    "on-event trapped - module marked dead; will retry after backoff",
                );
                metrics::counter!(
                    "nexum_runtime_module_errors_total",
                    "module" => module.name.clone(),
                    "error_kind" => "trap",
                )
                .increment(1);
                // Death diagnosis: leave a retrievable panic record on the
                // dead run carrying the trap's root cause; the full trap
                // with its wasm frame list already went to host tracing.
                router.record(LogRecord::now(
                    module.live.run.clone(),
                    LogSource::Panic,
                    Level::ERROR,
                    format!("run terminated abnormally: {}", trap.root_cause()),
                ));
                if let Some(recent) = verdict.poisoned {
                    // A string field, not a `Display` one: the two record
                    // through different visitor methods and print differently.
                    let last_error = trap.to_string();
                    warn!(
                        module = %module.name,
                        recent_failures = recent,
                        window_secs = poison_policy.window.as_secs(),
                        last_error,
                        "module poisoned - quarantined; remove from engine.toml + restart to clear",
                    );
                    metrics::gauge!(
                        "nexum_runtime_module_poisoned",
                        "module" => module.name.clone(),
                    )
                    .set(1.0);
                }
                DispatchOutcome::Trapped
            }
        }
    }
}

/// A dispatch (guest plus every host call it awaited) outlived its
/// wall-clock deadline and was cancelled. Distinct from a fuel trap, which
/// bounds guest instructions.
#[derive(Debug, thiserror::Error)]
#[error(
    "dispatch exceeded its {0:?} wall-clock deadline \
     (a host call blocked or ran too long)"
)]
pub(super) struct DeadlineExceeded(Duration);

/// Run a guest dispatch future under a wall-clock `deadline`. Fuel bounds
/// only guest instructions, so this bounds time in host calls (see
/// [`crate::runtime::limits`]). Returns `Err(DeadlineExceeded)` once the
/// future outlives `deadline`; dropping it cancels the in-flight host call
/// at its next await point. Pure guest spinning stays fuel's job.
pub(super) async fn with_dispatch_deadline<F: std::future::Future>(
    deadline: Duration,
    fut: F,
) -> Result<F::Output, DeadlineExceeded> {
    tokio::time::timeout(deadline, fut)
        .await
        .map_err(|_elapsed| DeadlineExceeded(deadline))
}

/// Outcome of `dispatch_to` for one module. Private; only the `dispatch_*`
/// entry points consume it.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum DispatchOutcome {
    /// Guest returned `Ok(())`.
    Ok,
    /// Guest returned a typed `fault` via WIT.
    Fault,
    /// Guest trapped (panic / OOM / fuel / etc). Marked dead, maybe
    /// quarantined per the poison policy.
    Trapped,
    /// `set_fuel` failed before the call; the module stays alive, this
    /// event is skipped.
    FuelSetFailed,
    /// Per-module dispatch rate limit exceeded; the event is dropped before
    /// the guest runs, liveness untouched.
    RateLimited,
}
