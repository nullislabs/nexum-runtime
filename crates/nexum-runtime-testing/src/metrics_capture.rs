//! Scoped metric capture.
//!
//! A "counted" done-condition is otherwise satisfiable by a counter on an
//! unreachable branch, or by a misspelled name, with the suite green either
//! way. [`capture_metrics`] installs a recorder for the closure only, so a
//! test can assert the increment and the labels it claims.

use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};

/// One captured sample: the metric name, its labels, and its value.
#[derive(Debug)]
pub struct Sample {
    /// Emitted metric name.
    pub name: String,
    /// Label pairs, in the order the call site supplied them.
    pub labels: Vec<(String, String)>,
    /// The recorded value.
    pub value: DebugValue,
}

impl Sample {
    /// Whether this sample carries `key` with `value`.
    pub fn has_label(&self, key: &str, value: &str) -> bool {
        self.labels.iter().any(|(k, v)| k == key && v == value)
    }
}

/// Run `f` with a recorder installed for its duration and return what it
/// recorded. The recorder is scoped, so concurrent tests do not collide the
/// way a globally installed one would.
pub fn capture_metrics<T>(f: impl FnOnce() -> T) -> (T, Vec<Sample>) {
    let recorder = DebuggingRecorder::new();
    let snapshotter: Snapshotter = recorder.snapshotter();
    let out = metrics::with_local_recorder(&recorder, f);
    let samples = snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .map(|(key, _unit, _desc, value)| {
            let key = key.key();
            Sample {
                name: key.name().to_owned(),
                labels: key
                    .labels()
                    .map(|l| (l.key().to_owned(), l.value().to_owned()))
                    .collect(),
                value,
            }
        })
        .collect();
    (out, samples)
}

/// Every sample recorded under `name`.
pub fn samples_named<'a>(samples: &'a [Sample], name: &str) -> Vec<&'a Sample> {
    samples.iter().filter(|s| s.name == name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_and_its_labels_are_captured() {
        let ((), samples) = capture_metrics(|| {
            metrics::counter!("nexum_runtime_chain_request_total", "chain" => "1").increment(1);
        });
        let hits = samples_named(&samples, "nexum_runtime_chain_request_total");
        assert_eq!(hits.len(), 1, "the counter was recorded once: {samples:?}");
        assert!(
            hits[0].has_label("chain", "1"),
            "labels survive capture: {:?}",
            hits[0].labels,
        );
    }

    #[test]
    fn a_counter_that_never_fires_records_nothing() {
        // The point of the harness: an unreachable call site is visible as
        // an absent sample rather than passing for a present one.
        let ((), samples) = capture_metrics(|| {});
        assert!(
            samples_named(&samples, "nexum_runtime_chain_request_total").is_empty(),
            "{samples:?}",
        );
    }
}
