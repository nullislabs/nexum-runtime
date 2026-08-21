//! Byte cap and rate bound on every capture point, applied before the
//! router renders the record.

use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use nexum_runtime_config::{LogBoundsPolicy, TokenBucket};

use super::{LogField, LogRecord};

/// A cap below this length drops the marker rather than exceed the cap.
const TRUNCATION_MARKER: &str = "...[truncated]";

/// Bytes the target keeps ahead of the message, so an oversized dump still
/// names its subsystem and a guest cannot hide payload in the target.
const TARGET_ALLOWANCE: usize = 128;

/// One run's gate, held by every capture point that run writes through, so
/// a flood spends the same bucket whichever way it enters.
#[derive(Clone, Debug)]
pub struct SharedLogBounds(Arc<LogBounds>);

impl SharedLogBounds {
    /// Starts with a full burst, as of `now`.
    pub fn new(policy: LogBoundsPolicy, now: Instant) -> Self {
        Self(Arc::new(LogBounds::new(policy, now)))
    }

    /// Fit `record` to the cap and spend one token; `false` drops it whole.
    pub fn admit(&self, record: &mut LogRecord, now: Instant) -> bool {
        self.0.admit(record, now)
    }

    /// The byte cap, for a capture point that must bound a buffer before it
    /// has a record to admit.
    pub fn max_record_bytes(&self) -> usize {
        self.0.policy.max_record_bytes.get()
    }
}

/// The bucket is the only mutable half. The cap is read on every stdio
/// write, most of which emit nothing, so it stays outside the lock.
#[derive(Debug)]
struct LogBounds {
    policy: LogBoundsPolicy,
    rate: Mutex<TokenBucket>,
}

impl LogBounds {
    /// Starts with a full burst, as of `now`.
    fn new(policy: LogBoundsPolicy, now: Instant) -> Self {
        Self {
            policy,
            rate: Mutex::new(TokenBucket::new(policy.rate, now)),
        }
    }

    /// Fit `record` to the cap and spend one token; `false` drops it whole.
    /// Yield order: fields last-first, the call site, the message, the
    /// target. Each stage yields only what the earlier ones left over.
    fn admit(&self, record: &mut LogRecord, now: Instant) -> bool {
        if !self.rate.lock().try_acquire(now) {
            let module = record.run.module.as_str().to_owned();
            let channel: &'static str = record.channel.into();
            metrics::counter!("nexum_runtime_log_records_dropped_total", "module" => module, "channel" => channel)
                .increment(1);
            return false;
        }
        let cap = self.policy.max_record_bytes.get();
        let mut total = record.wire_bytes();
        let mut dropped = 0u64;
        while total > cap {
            let Some(field) = record.fields.pop() else {
                break;
            };
            total -= field.rendered_len();
            dropped += 1;
        }
        let mut shortened = false;
        // Most bytes freed for the fewest a reader misses.
        if total > cap
            && let Some(file) = record.source.file.take()
        {
            total -= file.len();
            record.source.line = None;
            shortened = true;
        }
        if total > cap {
            // The allowance is the target's floor, not its ceiling.
            let reserved = record.source.target.len().min(TARGET_ALLOWANCE).min(cap);
            shortened |= truncate_to(&mut record.message, cap - reserved);
            let spare = cap - record.message.len();
            shortened |= truncate_to(&mut record.source.target, spare.min(TARGET_ALLOWANCE));
        }
        let channel: &'static str = record.channel.into();
        if shortened {
            let module = record.run.module.as_str().to_owned();
            metrics::counter!("nexum_runtime_log_records_truncated_total", "module" => module, "channel" => channel)
                .increment(1);
        }
        if dropped > 0 {
            let module = record.run.module.as_str().to_owned();
            metrics::counter!("nexum_runtime_log_fields_dropped_total", "module" => module, "channel" => channel)
                .increment(dropped);
        }
        true
    }
}

/// Shorten `text` to `budget` bytes, reporting whether it did. The prefix
/// ends on a character boundary.
fn truncate_to(text: &mut String, budget: usize) -> bool {
    if text.len() <= budget {
        return false;
    }
    let mut keep = budget.saturating_sub(TRUNCATION_MARKER.len());
    while !text.is_char_boundary(keep) {
        keep -= 1;
    }
    text.truncate(keep);
    if budget >= TRUNCATION_MARKER.len() {
        text.push_str(TRUNCATION_MARKER);
    }
    true
}

impl LogRecord {
    /// Bytes the cap measures: what the guest spelled, no fixed overhead,
    /// because the cap bounds the render and not the retained struct. An
    /// upper bound on the render, never an estimate.
    fn wire_bytes(&self) -> usize {
        self.message.len()
            + self.source.cost()
            + self
                .fields
                .iter()
                .map(LogField::rendered_len)
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroUsize};
    use std::time::Duration;

    use nexum_runtime_config::DispatchRatePolicy;
    use tracing_core::Level;

    use super::*;
    use crate::logs::{LogChannel, LogField, LogSource, LogValue, RunId};
    use nexum_primitives::module_id::ModuleId;

    fn bounds(cap: usize, burst: u32, per_sec: u32) -> (SharedLogBounds, Instant) {
        let policy = LogBoundsPolicy {
            max_record_bytes: NonZeroUsize::new(cap).expect("non-zero cap"),
            rate: DispatchRatePolicy::new(
                NonZeroU32::new(burst).expect("non-zero burst"),
                NonZeroU32::new(per_sec).expect("non-zero rate"),
            ),
        };
        let now = Instant::now();
        (SharedLogBounds::new(policy, now), now)
    }

    fn record(message: &str) -> LogRecord {
        LogRecord::now(
            RunId::new(ModuleId::parse("m").expect("valid module name"), 0),
            LogChannel::HostInterface,
            Level::INFO,
            message.to_owned(),
        )
    }

    /// Multi-byte: a cut inside a character would panic.
    #[test]
    fn an_oversized_message_is_truncated_to_the_cap_with_a_marker() {
        let (gate, now) = bounds(64, 8, 1);
        let mut rec = record(&"\u{20ac}".repeat(4096));
        assert!(
            gate.admit(&mut rec, now),
            "the cap truncates, never refuses"
        );
        assert!(rec.wire_bytes() <= 64, "the admitted record fits the cap");
        assert!(
            rec.message.starts_with('\u{20ac}') && rec.message.ends_with(TRUNCATION_MARKER),
            "a reader can tell the line was cut: {}",
            rec.message,
        );
    }

    #[test]
    fn overflow_fields_drop_last_recorded_first_and_the_message_survives() {
        let (gate, now) = bounds(64, 8, 1);
        let mut rec = record("keep me");
        rec.fields = (0..64)
            .map(|i| LogField {
                name: format!("f{i}"),
                value: LogValue::Text("v".repeat(16)),
            })
            .collect();
        assert!(gate.admit(&mut rec, now));
        assert_eq!(rec.message, "keep me", "the message is never the overflow");
        assert!(rec.wire_bytes() <= 64 && rec.fields.len() < 64);
        assert_eq!(
            rec.fields.first().map(|f| f.name.as_str()),
            Some("f0"),
            "the earliest context survives the drop",
        );
    }

    #[test]
    fn an_oversized_call_site_cannot_evade_the_cap_by_leaving_the_message() {
        let (gate, now) = bounds(512, 8, 1);
        let mut rec = record("short").with_source(LogSource {
            target: "t".repeat(4096),
            file: Some("f".repeat(4096)),
            line: Some(7),
        });
        assert!(gate.admit(&mut rec, now));
        assert_eq!(rec.source.file, None, "the file yields before the rest");
        assert_eq!(rec.source.line, None, "the line yields with its file");
        assert_eq!(rec.message, "short", "the message still survives whole");
        assert_eq!(
            rec.wire_bytes(),
            "short".len() + TARGET_ALLOWANCE,
            "the call site is measured too, and only the target is left",
        );
    }

    /// The allowance is a floor, not a ceiling: an over-allowance target
    /// survives whole when dropping the file already fit the record.
    #[test]
    fn a_target_past_its_allowance_survives_an_overflow_the_file_alone_covers() {
        let (gate, now) = bounds(1024, 8, 1);
        let target = "deep::module::path::".repeat(18);
        assert!(
            target.len() > TARGET_ALLOWANCE,
            "the case needs a deep target"
        );
        let mut rec = record("short").with_source(LogSource {
            target: target.clone(),
            file: Some("f".repeat(4096)),
            line: Some(7),
        });
        assert!(gate.admit(&mut rec, now));
        assert_eq!(rec.source.file, None, "the file yields first");
        assert_eq!(
            rec.source.target, target,
            "the target yields only what the cap still needs after the file",
        );
        assert_eq!(rec.message, "short", "the message survives whole");
    }

    #[test]
    fn a_message_that_fills_the_cap_still_carries_its_target() {
        let (gate, now) = bounds(512, 8, 1);
        let mut rec = record(&"m".repeat(4096)).with_source(LogSource {
            target: "wallet::signer".to_owned(),
            file: None,
            line: None,
        });
        assert!(gate.admit(&mut rec, now));
        assert_eq!(
            rec.source.target, "wallet::signer",
            "the dump that fills the cap is the one worth attributing",
        );
        assert!(rec.wire_bytes() <= 512, "the target is charged, not free");
        assert!(rec.message.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn an_oversized_target_is_cut_to_its_allowance_and_hides_no_payload() {
        let (gate, now) = bounds(4096, 8, 1);
        let mut rec = record("short").with_source(LogSource {
            target: "t".repeat(4096),
            file: None,
            line: None,
        });
        assert!(gate.admit(&mut rec, now));
        assert_eq!(
            rec.source.target.len(),
            TARGET_ALLOWANCE,
            "the target keeps its allowance and no more",
        );
        assert_eq!(rec.message, "short", "the message still survives whole");
    }

    /// The measure must never sit under the rendered bytes. A flat scalar
    /// charge let an `f64` field list render thirty times over the cap.
    #[test]
    fn the_measure_never_sits_under_the_bytes_the_render_writes() {
        let (gate, now) = bounds(512, 8, 1);
        let mut rec = record("m").with_source(LogSource {
            target: "guest::work".to_owned(),
            file: None,
            line: None,
        });
        rec.fields = (0..64)
            .map(|i| LogField {
                name: format!("f{i}"),
                // The widest `f64` render: a subnormal spells out every
                // digit of its decimal expansion.
                value: LogValue::Float(f64::from_bits(1)),
            })
            .collect();
        assert!(gate.admit(&mut rec, now));
        let rendered = rec.message.len()
            + rec.source.cost()
            + crate::logs::render_fields(&rec.fields).map_or(0, |line| line.len());
        assert!(
            rendered <= 512,
            "the record renders {rendered} bytes under a 512-byte cap",
        );
    }

    #[test]
    fn the_rate_drops_records_past_the_burst_and_refills() {
        let (gate, now) = bounds(4096, 2, 4);
        assert!(gate.admit(&mut record("a"), now));
        assert!(gate.admit(&mut record("b"), now));
        assert!(
            !gate.admit(&mut record("c"), now),
            "the third record in one instant is over the burst",
        );
        let later = now + Duration::from_millis(250);
        assert!(gate.admit(&mut record("d"), later), "4/s refills one token");
        assert!(!gate.admit(&mut record("e"), later), "only one refilled");
    }
}
