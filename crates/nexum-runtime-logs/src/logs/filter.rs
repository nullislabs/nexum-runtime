//! Level and target filter on every capture point.

use std::sync::Arc;
use std::time::Instant;

use nexum_runtime_config::{LogFilterPolicy, LogVerdict};

use super::{LogRecord, LogRouter, SharedLogBounds};

/// One run's operator filter. The death path holds none, so a synthesized
/// death record is unfilterable by construction.
#[derive(Clone, Debug)]
pub struct SharedLogFilter(Arc<LogFilterPolicy>);

impl SharedLogFilter {
    /// Handle over `policy`, shared by cloning.
    pub fn new(policy: LogFilterPolicy) -> Self {
        Self(Arc::new(policy))
    }

    /// Route `record` to the sinks its level and target clear. The filter
    /// runs before `bounds` so a filtered record spends no token, rather
    /// than costing a kept record its place and reading as a cap loss.
    pub fn route(&self, router: &LogRouter, bounds: &SharedLogBounds, mut record: LogRecord) {
        let verdict = self.0.verdict(record.level, &record.source.target);
        if verdict == LogVerdict::Drop {
            let module = record.run.module.as_str().to_owned();
            let channel: &'static str = record.channel.into();
            metrics::counter!("nexum_runtime_log_records_filtered_total", "module" => module, "channel" => channel)
                .increment(1);
            return;
        }
        if !bounds.admit(&mut record, Instant::now()) {
            return;
        }
        if verdict == LogVerdict::Emit {
            router.record(record);
        } else {
            router.retain(record);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::{NonZeroU32, NonZeroUsize};

    use nexum_runtime_config::{DispatchRatePolicy, LogBoundsPolicy};
    use tracing_core::Level;

    use super::*;
    use crate::capture_logs;
    use crate::logs::test_support::{CaptureStore, run_id};
    use crate::logs::{LogChannel, LogSource};

    fn unbounded() -> SharedLogBounds {
        bucket(u32::MAX)
    }

    fn bucket(burst: u32) -> SharedLogBounds {
        SharedLogBounds::new(
            LogBoundsPolicy {
                max_record_bytes: NonZeroUsize::new(4096).expect("non-zero cap"),
                // 1/s refills nothing over a flood of microseconds.
                rate: DispatchRatePolicy::new(
                    NonZeroU32::new(burst).expect("non-zero burst"),
                    NonZeroU32::new(1).expect("non-zero rate"),
                ),
            },
            std::time::Instant::now(),
        )
    }

    fn filter(console: Level, retain: Level, targets: &[(&str, Level)]) -> SharedLogFilter {
        SharedLogFilter::new(LogFilterPolicy {
            console,
            retain,
            targets: targets
                .iter()
                .map(|(name, level)| ((*name).to_owned(), *level))
                .collect::<BTreeMap<_, _>>(),
        })
    }

    fn record(level: Level, target: &str) -> LogRecord {
        LogRecord::now(
            run_id(),
            LogChannel::HostInterface,
            level,
            "line".to_owned(),
        )
        .with_source(LogSource {
            target: target.to_owned(),
            file: None,
            line: None,
        })
    }

    fn pipeline() -> (Arc<CaptureStore>, LogRouter) {
        let store = Arc::new(CaptureStore::default());
        (store.clone(), LogRouter::new(store))
    }

    #[test]
    fn a_record_under_the_console_floor_is_retained_and_never_printed() {
        let (store, router) = pipeline();
        let bounds = unbounded();
        let gate = filter(Level::WARN, Level::TRACE, &[]);
        let out = capture_logs(Level::TRACE, || {
            gate.route(&router, &bounds, record(Level::DEBUG, "wallet::keeper"));
            gate.route(&router, &bounds, record(Level::ERROR, "wallet::keeper"));
        });
        assert_eq!(out.lines().count(), 1, "only the error printed: {out}");
        assert!(out.contains("ERROR"), "{out}");
        assert_eq!(
            store.messages().len(),
            2,
            "`nexum logs` keeps what the console did not print",
        );
    }

    /// The last record has no target, as every stdio line does, so the row
    /// cannot reach it.
    #[test]
    fn a_target_row_lifts_its_own_target_and_leaves_the_rest_quiet() {
        let (_store, router) = pipeline();
        let bounds = unbounded();
        let gate = filter(Level::WARN, Level::TRACE, &[("keeper", Level::DEBUG)]);
        let out = capture_logs(Level::TRACE, || {
            gate.route(&router, &bounds, record(Level::DEBUG, "keeper"));
            gate.route(&router, &bounds, record(Level::DEBUG, "signer"));
            gate.route(&router, &bounds, record(Level::DEBUG, ""));
        });
        assert_eq!(out.lines().count(), 1, "only the named target: {out}");
        assert!(out.contains("source=\"keeper\""), "{out}");
    }

    #[test]
    fn a_filtered_record_spends_no_token_of_the_run_bucket() {
        let (store, router) = pipeline();
        let bounds = bucket(1);
        let gate = filter(Level::WARN, Level::WARN, &[]);
        gate.route(&router, &bounds, record(Level::DEBUG, "chatter"));
        gate.route(&router, &bounds, record(Level::ERROR, "chatter"));
        assert_eq!(
            store.messages().len(),
            1,
            "the wanted error lost its token to a record the operator filtered out",
        );
    }

    #[test]
    fn a_record_under_both_floors_is_dropped_and_counted_apart_from_a_bound() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let (store, router) = pipeline();
        let bounds = unbounded();
        metrics::with_local_recorder(&recorder, || {
            filter(Level::ERROR, Level::WARN, &[]).route(
                &router,
                &bounds,
                record(Level::INFO, "keeper"),
            );
        });
        assert!(store.messages().is_empty(), "neither floor was cleared");
        let names: Vec<String> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .map(|(composite, ..)| composite.key().name().to_owned())
            .collect();
        assert_eq!(
            names,
            ["nexum_runtime_log_records_filtered_total"],
            "a filter drop must not read as a rate or cap loss",
        );
    }
}
