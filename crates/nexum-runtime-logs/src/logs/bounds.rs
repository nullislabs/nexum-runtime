//! Admission bounds on the host logging verbs: a whole-record byte cap and
//! a per-module token bucket, both applied before the router renders the
//! record, which is the synchronous cost the bound exists to stop.

use std::time::Instant;

use nexum_runtime_config::LogBoundsPolicy;

use super::{LogField, LogRecord};

/// Appended to a message the cap shortened. A cap below its own length
/// drops it rather than exceeding the cap.
const TRUNCATION_MARKER: &str = "...[truncated]";

/// Per-module admission gate for the host logging verbs; not shared, so
/// one module's flood spends only its own bucket.
#[derive(Debug)]
pub struct LogBounds {
    policy: LogBoundsPolicy,
    /// Current tokens in `[0, capacity]`; fractional so slow refill is not lost.
    tokens: f64,
    last_refill: Instant,
}

impl LogBounds {
    /// A gate that starts with a full burst allowance, as of `now`.
    pub fn new(policy: LogBoundsPolicy, now: Instant) -> Self {
        Self {
            policy,
            tokens: f64::from(policy.rate.capacity.get()),
            last_refill: now,
        }
    }

    /// Fit `record` to the cap and spend one token; `false` drops it whole.
    /// An admitted record keeps its message, so the overflow order is
    /// fields last-recorded first, then the call site, then a marked prefix
    /// of the message.
    pub fn admit(&mut self, record: &mut LogRecord, now: Instant) -> bool {
        if !self.try_acquire(now) {
            let module = record.run.module.as_str().to_owned();
            metrics::counter!("nexum_runtime_log_records_dropped_total", "module" => module)
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
        if total > cap {
            // The call site yields before the message: dropping the file
            // frees the most bytes for the fewest a reader misses.
            shortened |= record.source.file.take().is_some();
            let message = record.message.len();
            shortened |= truncate_to(&mut record.source.target, cap.saturating_sub(message));
            let target = record.source.target.len();
            shortened |= truncate_to(&mut record.message, cap.saturating_sub(target));
        }
        if shortened {
            let module = record.run.module.as_str().to_owned();
            metrics::counter!("nexum_runtime_log_records_truncated_total", "module" => module)
                .increment(1);
        }
        if dropped > 0 {
            let module = record.run.module.as_str().to_owned();
            metrics::counter!("nexum_runtime_log_fields_dropped_total", "module" => module)
                .increment(dropped);
        }
        true
    }

    /// Refill for elapsed time, then consume one token; `now` is injected.
    fn try_acquire(&mut self, now: Instant) -> bool {
        let capacity = f64::from(self.policy.rate.capacity.get());
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * f64::from(self.policy.rate.refill_per_sec.get()))
            .min(capacity);
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Shorten `text` to at most `budget` bytes, reporting whether it did. The
/// kept prefix ends on a character boundary, so a multi-byte character is
/// dropped whole rather than left half-written.
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
    /// Bytes the admission cap measures: every byte the guest spelled, and
    /// no fixed overhead, because the cap bounds the transient render
    /// rather than the retained struct. It is an upper bound on the render,
    /// never an estimate of it, or a field list could outrun the cap it
    /// was admitted under.
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

    fn bounds(cap: usize, burst: u32, per_sec: u32) -> (LogBounds, Instant) {
        let policy = LogBoundsPolicy {
            max_record_bytes: NonZeroUsize::new(cap).expect("non-zero cap"),
            rate: DispatchRatePolicy::new(
                NonZeroU32::new(burst).expect("non-zero burst"),
                NonZeroU32::new(per_sec).expect("non-zero rate"),
            ),
        };
        let now = Instant::now();
        (LogBounds::new(policy, now), now)
    }

    fn record(message: &str) -> LogRecord {
        LogRecord::now(
            RunId::new(ModuleId::parse("m").expect("valid module name"), 0),
            LogChannel::HostInterface,
            Level::INFO,
            message.to_owned(),
        )
    }

    /// A multi-byte message doubles as the boundary case: a cut inside a
    /// character would panic rather than truncate.
    #[test]
    fn an_oversized_message_is_truncated_to_the_cap_with_a_marker() {
        let (mut gate, now) = bounds(64, 8, 1);
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
        let (mut gate, now) = bounds(64, 8, 1);
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
        let (mut gate, now) = bounds(64, 8, 1);
        let mut rec = record("short").with_source(LogSource {
            target: "t".repeat(4096),
            file: Some("f".repeat(4096)),
            line: Some(7),
        });
        assert!(gate.admit(&mut rec, now));
        assert_eq!(rec.wire_bytes(), 64, "the call site is measured too");
        assert_eq!(rec.message, "short", "the message still survives whole");
    }

    /// The cap bounds the render, so the measure must never sit under the
    /// bytes the sink writes. A flat scalar charge broke this: an `f64`
    /// renders its whole decimal expansion, so a field list measured well
    /// inside the cap rendered thirty times over it.
    #[test]
    fn the_measure_never_sits_under_the_bytes_the_render_writes() {
        let (mut gate, now) = bounds(512, 8, 1);
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
        let (mut gate, now) = bounds(4096, 2, 4);
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
