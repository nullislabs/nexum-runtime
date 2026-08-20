use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nexum_sdk::Level;
use parking_lot::Mutex;
use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// One tracing event captured pre-flattening.
#[derive(Clone, Debug, PartialEq)]
pub struct CapturedEvent {
    /// Event severity.
    pub level: Level,
    /// Callsite target (module path by default).
    pub target: String,
    /// The `message` field; empty when the event carried none.
    pub message: String,
    /// Every non-message field, keyed by name.
    pub fields: BTreeMap<String, FieldValue>,
}

/// A field value as tracing's `Visit` delivered it.
#[derive(Clone, Debug, PartialEq, derive_more::Display)]
pub enum FieldValue {
    /// A `record_str` value.
    Str(String),
    /// A `record_u64` value.
    U64(u64),
    /// A `record_i64` value.
    I64(i64),
    /// A `record_f64` value.
    F64(f64),
    /// A `record_bool` value.
    Bool(bool),
    /// A `record_debug` fallback (`?x`, `%x`, ...), pre-rendered with
    /// `{:?}`.
    Debug(String),
}

impl CapturedEvent {
    /// The value recorded for `name`, if the event carried it.
    pub fn field(&self, name: &str) -> Option<&FieldValue> {
        self.fields.get(name)
    }

    /// Display-rendered field, for string comparisons.
    pub fn field_str(&self, name: &str) -> Option<String> {
        self.fields.get(name).map(FieldValue::to_string)
    }
}

/// Events captured during [`capture_tracing`].
pub struct CapturedEvents {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl CapturedEvents {
    /// Every captured event, in emission order.
    pub fn events(&self) -> Vec<CapturedEvent> {
        self.events.lock().clone()
    }

    /// Whether no events were captured.
    pub fn is_empty(&self) -> bool {
        self.events.lock().is_empty()
    }

    /// Count of events at `level`.
    pub fn count_at(&self, level: Level) -> usize {
        self.events
            .lock()
            .iter()
            .filter(|e| e.level == level)
            .count()
    }

    /// Whether any captured event satisfies `pred`.
    pub fn any(&self, pred: impl Fn(&CapturedEvent) -> bool) -> bool {
        self.events.lock().iter().any(pred)
    }

    /// Exactly one matching event; panics with the full capture dump
    /// otherwise.
    pub fn expect_one(&self, pred: impl Fn(&CapturedEvent) -> bool) -> CapturedEvent {
        let events = self.events.lock();
        let matches: Vec<&CapturedEvent> = events.iter().filter(|e| pred(e)).collect();
        match matches.as_slice() {
            [only] => (*only).clone(),
            other => panic!(
                "expected exactly one matching event, found {}; captured: {events:#?}",
                other.len(),
            ),
        }
    }
}

type Buffer = Arc<Mutex<Vec<CapturedEvent>>>;

std::thread_local! {
    /// The capture buffer active on this thread, if any.
    static ACTIVE_CAPTURE: RefCell<Option<Buffer>> = const { RefCell::new(None) };
}

/// Restores the previous thread-local capture slot when a
/// `capture_tracing` call returns or unwinds.
struct CaptureGuard(Option<Buffer>);

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        ACTIVE_CAPTURE.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

/// Events-only subscriber recording each event into the thread's active
/// buffer; spans are inert.
struct CaptureSubscriber {
    next_id: AtomicU64,
}

impl Subscriber for CaptureSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::TRACE)
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        // Spans are inert, but a valid non-zero id must be returned.
        let raw = self.next_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        Id::from_u64(raw.max(1))
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let captured = CapturedEvent {
            level: *event.metadata().level(),
            target: event.metadata().target().to_owned(),
            message: visitor.message,
            fields: visitor.fields,
        };
        ACTIVE_CAPTURE.with(|slot| {
            if let Some(buffer) = slot.borrow().as_ref() {
                buffer.lock().push(captured);
            }
        });
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

/// Splits an event into its `message` field and a name-keyed map of the rest.
#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: BTreeMap<String, FieldValue>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            // tracing delivers `message` as the `format_args!` result, whose
            // `Debug` renders unquoted; keep the raw text, do not re-quote it.
            let _ = write!(self.message, "{value:?}");
        } else {
            self.fields.insert(
                field.name().to_owned(),
                FieldValue::Debug(format!("{value:?}")),
            );
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.fields
                .insert(field.name().to_owned(), FieldValue::Str(value.to_owned()));
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_owned(), FieldValue::U64(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_owned(), FieldValue::I64(value));
    }

    // Present so the capture types a float the way the guest facade does;
    // without it `tracing` falls back to `record_debug` and a module test
    // sees a rendered string where the host receives a number.
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields
            .insert(field.name().to_owned(), FieldValue::F64(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_owned(), FieldValue::Bool(value));
    }
}

static INSTALL_ROUTING: std::sync::Once = std::sync::Once::new();

/// Run `f`, returning its value and every `tracing` event it emitted on
/// the calling thread. Capture is thread-scoped; events emitted outside
/// any `capture_tracing` call are dropped.
pub fn capture_tracing<R>(f: impl FnOnce() -> R) -> (R, CapturedEvents) {
    INSTALL_ROUTING.call_once(|| {
        let _ = tracing::subscriber::set_global_default(CaptureSubscriber {
            next_id: AtomicU64::new(0),
        });
    });

    let events: Buffer = Arc::new(Mutex::new(Vec::new()));
    let previous = ACTIVE_CAPTURE.with(|slot| slot.borrow_mut().replace(Arc::clone(&events)));
    let _guard = CaptureGuard(previous);
    let result = f();
    drop(_guard);
    (result, CapturedEvents { events })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_message_only_event_has_empty_fields() {
        let (_, logs) = capture_tracing(|| tracing::info!("hello"));
        let events = logs.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, Level::INFO);
        assert_eq!(events[0].message, "hello");
        assert!(events[0].fields.is_empty());
    }

    #[test]
    fn capture_fields_land_as_typed_values() {
        let (_, logs) = capture_tracing(|| {
            tracing::warn!(
                name = "eth",
                count = 7u64,
                signed = -3i64,
                ratio = 0.5f64,
                ready = true,
                answer = ?Some(9),
                "changed",
            );
        });
        let ev = logs.expect_one(|e| e.level == Level::WARN);
        assert_eq!(ev.message, "changed");
        assert_eq!(ev.field("name"), Some(&FieldValue::Str("eth".to_owned())));
        assert_eq!(ev.field("count"), Some(&FieldValue::U64(7)));
        assert_eq!(ev.field("signed"), Some(&FieldValue::I64(-3)));
        assert_eq!(ev.field("ratio"), Some(&FieldValue::F64(0.5)));
        assert_eq!(ev.field("ready"), Some(&FieldValue::Bool(true)));
        assert_eq!(
            ev.field("answer"),
            Some(&FieldValue::Debug("Some(9)".to_owned())),
        );
    }

    #[test]
    fn capture_display_recorded_value_lands_as_debug() {
        let (_, logs) = capture_tracing(|| tracing::info!(x = %42u32, "shown"));
        let ev = logs.expect_one(|e| e.message == "shown");
        assert!(matches!(ev.field("x"), Some(FieldValue::Debug(_))));
        assert_eq!(ev.field_str("x").as_deref(), Some("42"));
    }

    #[test]
    fn events_outside_capture_are_dropped() {
        // Prime the global default via one capture, then emit outside any.
        let (_, _) = capture_tracing(|| tracing::info!("primed"));
        tracing::info!("orphan");
        let (_, logs) = capture_tracing(|| tracing::info!("inside"));
        let events = logs.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, "inside");
    }

    #[test]
    fn concurrent_captures_are_thread_isolated() {
        use std::sync::Barrier;
        let barrier = Arc::new(Barrier::new(2));
        let other = Arc::clone(&barrier);
        let handle = std::thread::spawn(move || {
            let (_, logs) = capture_tracing(|| {
                other.wait();
                tracing::info!("thread-one");
            });
            logs.events()
        });
        let (_, main_logs) = capture_tracing(|| {
            barrier.wait();
            tracing::info!("thread-two");
        });
        let thread_events = handle.join().unwrap();

        assert_eq!(main_logs.events().len(), 1);
        assert_eq!(main_logs.events()[0].message, "thread-two");
        assert_eq!(thread_events.len(), 1);
        assert_eq!(thread_events[0].message, "thread-one");
    }
}
