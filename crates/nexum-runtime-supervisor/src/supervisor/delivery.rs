//! Delivery measures, taken at the instant a handler is invoked: how long
//! the trigger waited inside the host, and how old its block was.

use std::time::Duration;

use tokio::time::Instant;

use crate::bindings::nexum::host::types::Trigger;
use nexum_primitives::module_id::ModuleId;
use nexum_runtime_api::WasiClockOverride;

/// The instant the host took a trigger item from its source.
///
/// Monotonic, and the clock the dispatch path reads, so a paused test clock
/// moves both ends of the measure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceivedAt(Instant);

impl ReceivedAt {
    /// Stamp an item as received now.
    pub fn now() -> Self {
        Self(Instant::now())
    }
}

/// One trigger on its way to a handler, with what the measures need.
pub(super) struct Arrival<'a> {
    pub(super) trigger: &'a Trigger,
    received: ReceivedAt,
    /// The block's own time since the Unix epoch, where the trigger has one.
    block_time: Option<Duration>,
}

impl<'a> Arrival<'a> {
    /// A block carries its timestamp in milliseconds. A log mirrors the RPC
    /// shape, so its block timestamp is in seconds, and absent where the node
    /// does not report one.
    pub(super) fn new(trigger: &'a Trigger, received: ReceivedAt) -> Self {
        let block_time = match trigger {
            Trigger::Block(block) => Some(Duration::from_millis(block.timestamp)),
            Trigger::Event(log) => log.block_timestamp.map(Duration::from_secs),
            Trigger::Schedule(_) | Trigger::Extension(_) => None,
        };
        Self {
            trigger,
            received,
            block_time,
        }
    }

    /// Record both measures for a handler starting at `start`.
    ///
    /// `received` is fixed per item, so in a fan-out a later module's wait
    /// includes the handlers that ran before it. The block age reads the
    /// effective wall clock, the one the guest reads.
    pub(super) fn observe(
        &self,
        module: &ModuleId,
        chain_id: u64,
        trigger_kind: &'static str,
        start: Instant,
        clocks: Option<&WasiClockOverride>,
    ) {
        metrics::histogram!(
            "nexum_runtime_delivery_latency_seconds",
            "module" => module.to_string(),
            "trigger_kind" => trigger_kind,
        )
        .record(
            start
                .saturating_duration_since(self.received.0)
                .as_secs_f64(),
        );
        let Some(block_time) = self.block_time else {
            return;
        };
        let wall_now = WasiClockOverride::effective_wall(clocks).now();
        metrics::histogram!(
            "nexum_runtime_dispatch_block_age_seconds",
            "module" => module.to_string(),
            "trigger_kind" => trigger_kind,
            "chain_id" => chain_id.to_string(),
        )
        .record(block_age(wall_now, block_time).as_secs_f64());
    }
}

/// Clamped at zero: a block timestamp has one-second resolution, and the
/// node's clock may run ahead of the host's.
fn block_age(wall_now: Duration, block_time: Duration) -> Duration {
    wall_now.saturating_sub(block_time)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_behind_the_wall_clock_has_its_age() {
        assert_eq!(
            block_age(Duration::from_millis(12_500), Duration::from_secs(10)),
            Duration::from_millis(2_500),
        );
    }

    #[test]
    fn a_block_ahead_of_the_wall_clock_has_zero_age() {
        assert_eq!(
            block_age(Duration::from_secs(10), Duration::from_secs(11)),
            Duration::ZERO,
        );
    }
}
