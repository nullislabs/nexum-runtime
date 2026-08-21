//! Level and target filter on every capture point, choosing a record's
//! sinks once the bound has fitted it.

use std::sync::Arc;

use nexum_runtime_config::{LogFilterPolicy, LogVerdict};

use super::{LogRecord, LogRouter};

/// One run's operator filter, held by every capture point that run writes
/// through. The death path holds none: a synthesized death record is
/// unfilterable by construction rather than by an exemption here.
#[derive(Clone, Debug)]
pub struct SharedLogFilter(Arc<LogFilterPolicy>);

impl SharedLogFilter {
    /// Handle over `policy`, shared by cloning.
    pub fn new(policy: LogFilterPolicy) -> Self {
        Self(Arc::new(policy))
    }

    /// Route `record` to the sinks its level and target clear. A record
    /// dropped here is counted apart from a bounded one, because a filter
    /// drop is an operator choice and a bound drop is a loss.
    pub fn route(&self, router: &LogRouter, record: LogRecord) {
        match self.0.verdict(record.level, &record.source.target) {
            LogVerdict::Emit => router.record(record),
            LogVerdict::Retain => router.retain(record),
            LogVerdict::Drop => {
                let module = record.run.module.as_str().to_owned();
                let channel: &'static str = record.channel.into();
                metrics::counter!("nexum_runtime_log_records_filtered_total", "module" => module, "channel" => channel)
                    .increment(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tracing_core::Level;

    use super::*;
    use crate::logs::test_support::{CaptureStore, Console, run_id};
    use crate::logs::{LogChannel, LogSource};

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

    /// The whole point of the two thresholds: quiet console, full history.
    #[test]
    fn a_record_under_the_console_floor_is_retained_and_never_printed() {
        let (store, router) = pipeline();
        let gate = filter(Level::WARN, Level::TRACE, &[]);
        let out = Console::printed(|| {
            gate.route(&router, record(Level::DEBUG, "wallet::keeper"));
            gate.route(&router, record(Level::ERROR, "wallet::keeper"));
        });
        assert_eq!(out.lines().count(), 1, "only the error printed: {out}");
        assert!(out.contains("ERROR"), "{out}");
        assert_eq!(
            store.messages().len(),
            2,
            "`nexum logs` keeps what the console did not print",
        );
    }

    /// The last record has no target, as every stdio line does, so the
    /// row cannot reach it and the default decides.
    #[test]
    fn a_target_row_lifts_its_own_target_and_leaves_the_rest_quiet() {
        let (_store, router) = pipeline();
        let gate = filter(Level::WARN, Level::TRACE, &[("keeper", Level::DEBUG)]);
        let out = Console::printed(|| {
            gate.route(&router, record(Level::DEBUG, "keeper"));
            gate.route(&router, record(Level::DEBUG, "signer"));
            gate.route(&router, record(Level::DEBUG, ""));
        });
        assert_eq!(out.lines().count(), 1, "only the named target: {out}");
        assert!(out.contains("source=\"keeper\""), "{out}");
    }

    #[test]
    fn a_record_under_both_floors_is_dropped_and_counted_apart_from_a_bound() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let (store, router) = pipeline();
        metrics::with_local_recorder(&recorder, || {
            filter(Level::ERROR, Level::WARN, &[]).route(&router, record(Level::INFO, "keeper"));
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
