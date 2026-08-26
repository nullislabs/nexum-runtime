//! The metric names the runtime emits, described once.
//!
//! [`METRICS`] is the single source, and [`describe_all`] emits the HELP
//! and TYPE text from it. A metric name is an operator contract, so adding
//! or renaming one is a deliberate diff here rather than an incidental
//! string somewhere. `nexum-runtime-guards` holds the guard that scans the
//! tree for a name this table does not carry, because that guard reads the
//! whole workspace and not this crate.

#![forbid(unsafe_code)]

/// How a metric is recorded, which decides the `describe_` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Monotonic count.
    Counter,
    /// Point-in-time value.
    Gauge,
    /// Distribution of observations.
    Histogram,
}

/// One emitted metric: its name, how it is recorded, and its HELP text.
pub struct Metric {
    /// The emitted name, matching the literal at the call site.
    pub name: &'static str,
    /// Counter, gauge, or histogram.
    pub kind: Kind,
    /// Operator-facing HELP text.
    pub help: &'static str,
}

/// Every metric the runtime emits.
pub const METRICS: &[Metric] = &[
    Metric {
        name: "nexum_runtime_boot_refusals_total",
        kind: Kind::Counter,
        help: "Boot refusals by error kind.",
    },
    Metric {
        name: "nexum_runtime_capability_denials_total",
        kind: Kind::Counter,
        help: "Capability requests the host refused, by capability, reason, and module.",
    },
    Metric {
        name: "nexum_runtime_chain_head_height",
        kind: Kind::Gauge,
        help: "Chain head height the runtime last observed, by chain.",
    },
    Metric {
        name: "nexum_runtime_chain_last_delivered_height",
        kind: Kind::Gauge,
        help: "Highest block height delivered to a module, by chain.",
    },
    Metric {
        name: "nexum_runtime_chain_request_total",
        kind: Kind::Counter,
        help: "Chain JSON-RPC requests by chain and outcome.",
    },
    Metric {
        name: "nexum_runtime_chain_response_capped_total",
        kind: Kind::Counter,
        help: "Chain responses truncated at the configured size cap.",
    },
    Metric {
        name: "nexum_runtime_dispatch_dropped_total",
        kind: Kind::Counter,
        help: "Triggers dropped before dispatch, by reason.",
    },
    Metric {
        name: "nexum_runtime_dispatch_latency_seconds",
        kind: Kind::Histogram,
        help: "Wall-clock seconds to dispatch one trigger, by module and outcome.",
    },
    Metric {
        name: "nexum_runtime_log_fields_dropped_total",
        kind: Kind::Counter,
        help: "Structured log fields dropped past the per-record byte cap, by module and channel.",
    },
    Metric {
        name: "nexum_runtime_log_records_dropped_total",
        kind: Kind::Counter,
        help: "Module log records dropped whole by the per-run log rate limit, by module and channel.",
    },
    Metric {
        name: "nexum_runtime_log_records_filtered_total",
        kind: Kind::Counter,
        help: "Module log records dropped by the operator log filter, by module and channel. An operator choice, not a loss.",
    },
    Metric {
        name: "nexum_runtime_log_records_truncated_total",
        kind: Kind::Counter,
        help: "Module log records shortened to fit the per-record byte cap, by module and channel.",
    },
    Metric {
        name: "nexum_runtime_module_errors_total",
        kind: Kind::Counter,
        help: "Module traps by fault label.",
    },
    Metric {
        name: "nexum_runtime_module_poisoned",
        kind: Kind::Gauge,
        help: "Modules quarantined by the poison policy or an unrecoverable event source.",
    },
    Metric {
        name: "nexum_runtime_module_restarts_total",
        kind: Kind::Counter,
        help: "Module restarts after a trap.",
    },
    Metric {
        name: "nexum_runtime_module_unverified",
        kind: Kind::Gauge,
        help: "Modules loaded with neither an operator nor an author digest pin.",
    },
    Metric {
        name: "nexum_runtime_source_reconnects_total",
        kind: Kind::Counter,
        help: "Source reconnects by source_kind and chain; source_kind \"chain-log\" also carries module.",
    },
];

/// Emit HELP and TYPE for every metric in [`METRICS`].
///
/// Without this the exposition carries bare samples, so an operator reading
/// `/metrics` has no statement of what a series means or how to aggregate it.
pub fn describe_all() {
    for metric in METRICS {
        match metric.kind {
            Kind::Counter => metrics::describe_counter!(metric.name, metric.help),
            Kind::Gauge => metrics::describe_gauge!(metric.name, metric.help),
            Kind::Histogram => metrics::describe_histogram!(metric.name, metric.help),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn table_names() -> BTreeSet<&'static str> {
        METRICS.iter().map(|m| m.name).collect()
    }

    #[test]
    fn the_table_has_no_duplicates() {
        assert_eq!(
            table_names().len(),
            METRICS.len(),
            "a duplicate name would describe one series twice and hide the other",
        );
    }
}
