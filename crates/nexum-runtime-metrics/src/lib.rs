//! The metric names the runtime emits, described once.
//!
//! [`METRICS`] is the single source: [`describe_all`] emits the HELP and
//! TYPE text from it, and a test scans the emitting crates for
//! `nexum_runtime_` literals and refuses any that the table does not
//! carry. A metric name is an operator contract, so adding or renaming one
//! is a deliberate diff here rather than an incidental string somewhere.

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
        help: "Wall-clock seconds to dispatch one trigger.",
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

    /// Scans every crate that emits under the `nexum_runtime_` prefix, the
    /// same shape as the single-compile-path guard in the digest tests. A
    /// name reaching an operator without passing through the table is the
    /// failure mode.
    #[test]
    fn every_emitted_name_is_in_the_table_and_every_entry_is_emitted() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut found: BTreeSet<String> = BTreeSet::new();
        let mut scanned = 0usize;
        let mut stack = vec![
            manifest.join("../nexum-runtime/src"),
            manifest.join("../nexum-runtime-supervisor/src"),
            manifest.join("../nexum-runtime-wasm/src"),
            manifest.join("../nexum-runtime-chain/src"),
            manifest.join("../nexum-runtime-store/src"),
            manifest.join("../nexum-runtime-logs/src"),
            manifest.join("../nexum-runtime-http/src"),
            manifest.join("../nexum-runtime-testing/src"),
        ];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read the crate source tree") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                scanned += 1;
                let src = std::fs::read_to_string(&path).expect("read a source file");
                for (idx, _) in src.match_indices("\"nexum_runtime_") {
                    let rest = &src[idx + 1..];
                    if let Some(end) = rest.find('"') {
                        found.insert(rest[..end].to_owned());
                    }
                }
            }
        }
        assert!(
            scanned >= 68,
            "the walk reached only {scanned} files; a shrunken walk loses the \
             operator contract silently, so re-derive the roots before lowering \
             this floor",
        );

        let table = table_names();
        let missing: Vec<&String> = found
            .iter()
            .filter(|n| !table.contains(n.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "emitted but undescribed, add to METRICS: {missing:?}",
        );

        let unused: Vec<&&str> = table.iter().filter(|n| !found.contains(**n)).collect();
        assert!(
            unused.is_empty(),
            "described but never emitted, remove from METRICS: {unused:?}",
        );
    }
}
