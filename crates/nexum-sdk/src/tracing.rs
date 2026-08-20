//! Guest-side `tracing` facade routing events to a host log sink, so
//! module authors write `tracing::info!(...)` with no host parameter.
//!
//! Events-only: each event crosses as its message, its call site and its
//! fields in the types `tracing` recorded them at, forwarded at the event's
//! [`Level`]; spans are inert. [`init`] also installs a panic hook that
//! writes the panic to stderr, then reports it over the sink (stderr first,
//! so the panic is captured even if the host call traps before
//! `panic = abort`).

use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicU64, Ordering};
use std::panic::{Location, PanicHookInfo};
use std::sync::Arc;

use tracing_core::field::{Field, Visit};
use tracing_core::span::{Attributes, Id, Record};
use tracing_core::{Event, Level, LevelFilter, Metadata, Subscriber};

/// Where an event was emitted from, mirroring [`Metadata`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LogSource<'a> {
    /// Event target, empty when the caller reports none.
    pub target: &'a str,
    /// Source file of the call site.
    pub file: Option<&'a str>,
    /// Line within [`LogSource::file`].
    pub line: Option<u32>,
}

/// One structured key-value pair recorded with an event.
#[derive(Debug, Clone, PartialEq)]
pub struct LogField {
    /// Field name, which `tracing` interns for the lifetime of the program.
    pub name: &'static str,
    /// Recorded value.
    pub value: LogValue,
}

/// A field value in the type `tracing` recorded it at; a value that arrives
/// only as [`fmt::Debug`] is rendered into [`LogValue::Text`].
#[derive(Debug, Clone, PartialEq, derive_more::Display, derive_more::From)]
pub enum LogValue {
    /// A string, or a `Debug` rendering.
    Text(String),
    /// An unsigned integer.
    Unsigned(u64),
    /// A signed integer.
    Signed(i64),
    /// A floating-point number.
    Float(f64),
    /// A boolean.
    Boolean(bool),
}

/// Sink the facade forwards events to; implementors carry the bound host
/// logging call.
pub trait LogSink: Send + Sync {
    /// Forward one event with its call site and its recorded fields.
    /// Required rather than defaulted, so an implementor cannot silently
    /// drop the fields.
    fn log_event(&self, level: Level, source: LogSource<'_>, message: &str, fields: &[LogField]);

    /// Forward a bare line, reporting no call site and no fields.
    fn log(&self, level: Level, message: &str) {
        self.log_event(level, LogSource::default(), message, &[]);
    }
}

/// Install the facade as the global subscriber and register the panic
/// hook over `sink`. The subscriber is set once; a second call only
/// re-registers the panic hook.
pub fn init(sink: impl LogSink + 'static) {
    let sink: Arc<dyn LogSink> = Arc::new(sink);
    let dispatch = tracing_core::Dispatch::new(FacadeSubscriber::new(Arc::clone(&sink)));
    // A second install is a no-op: the global default is set once.
    let _ = tracing_core::dispatcher::set_global_default(dispatch);
    set_panic_hook(sink);
}

/// The events-only subscriber over `sink`, without touching global
/// state.
pub fn subscriber(sink: impl LogSink + 'static) -> impl Subscriber {
    FacadeSubscriber::new(Arc::new(sink))
}

fn set_panic_hook(sink: Arc<dyn LogSink>) {
    std::panic::set_hook(Box::new(move |info| {
        let payload = panic_payload(info);
        let location = info.location();
        // stderr first: host-side stderr capture still records the panic
        // even if the sink's host call traps before `panic = abort` fires.
        // Only the stderr copy flattens the location, because stderr is a
        // line, not a structured record.
        eprintln!(
            "{}",
            format_panic(&payload, location.map(|l| (l.file(), l.line())))
        );
        let source = LogSource {
            target: PANIC_TARGET,
            file: location.map(Location::file),
            line: location.map(Location::line),
        };
        sink.log_event(Level::ERROR, source, &format!("panic: {payload}"), &[]);
    }));
}

/// Target reported for a panic, which has no `tracing` call site of its own.
const PANIC_TARGET: &str = "panic";

fn panic_payload(info: &PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

/// Render a panic into the reported line.
fn format_panic(payload: &str, location: Option<(&str, u32)>) -> String {
    match location {
        Some((file, line)) => format!("panic: {payload} at {file}:{line}"),
        None => format!("panic: {payload}"),
    }
}

struct FacadeSubscriber {
    sink: Arc<dyn LogSink>,
    next_id: AtomicU64,
}

impl FacadeSubscriber {
    fn new(sink: Arc<dyn LogSink>) -> Self {
        Self {
            sink,
            next_id: AtomicU64::new(0),
        }
    }
}

impl Subscriber for FacadeSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        // Forward everything; the host applies its own filter.
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
        let metadata = event.metadata();
        let level = *metadata.level();
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        let source = LogSource {
            target: metadata.target(),
            file: metadata.file(),
            line: metadata.line(),
        };
        self.sink
            .log_event(level, source, &visitor.message, &visitor.fields);
        #[cfg(feature = "stderr-echo")]
        {
            // A field-only event would otherwise carry a leading space; a
            // message keeps its own leading whitespace.
            let mut line = visitor.message.clone();
            for field in &visitor.fields {
                let _ = write!(line, " {}={}", field.name, field.value);
            }
            let line = if visitor.message.is_empty() {
                line.trim_start()
            } else {
                line.as_str()
            };
            eprintln!("[{level}] {line}");
        }
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

/// Splits an event into its `message` field and the rest, keeping record
/// order and the recorded type of each value.
#[derive(Default)]
struct EventVisitor {
    message: String,
    fields: Vec<LogField>,
}

impl EventVisitor {
    fn push(&mut self, field: &Field, value: impl Into<LogValue>) {
        self.fields.push(LogField {
            name: field.name(),
            value: value.into(),
        });
    }
}

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            self.push(field, format!("{value:?}"));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.push(field, value.to_owned());
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field, value);
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field, value);
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.push(field, value);
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field, value);
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use super::*;

    /// One event as the sink received it, with the borrowed source owned.
    #[derive(Debug, Clone)]
    struct Line {
        level: Level,
        target: String,
        file: Option<String>,
        line: Option<u32>,
        message: String,
        fields: Vec<LogField>,
    }

    /// Capturing sink for the scoped subscriber; writes only the required
    /// method, so the defaulted `log` is exercised as an implementor sees it.
    #[derive(Default)]
    struct Captured {
        lines: Mutex<Vec<Line>>,
    }

    impl LogSink for Arc<Captured> {
        fn log_event(
            &self,
            level: Level,
            source: LogSource<'_>,
            message: &str,
            fields: &[LogField],
        ) {
            self.lines.lock().push(Line {
                level,
                target: source.target.to_owned(),
                file: source.file.map(str::to_owned),
                line: source.line,
                message: message.to_owned(),
                fields: fields.to_vec(),
            });
        }
    }

    fn capture(f: impl FnOnce()) -> Vec<Line> {
        let sink = Arc::new(Captured::default());
        let subscriber = subscriber(Arc::clone(&sink));
        tracing::subscriber::with_default(subscriber, f);
        sink.lines.lock().clone()
    }

    #[test]
    fn each_macro_level_forwards_at_its_event_level() {
        let lines = capture(|| {
            tracing::trace!("t");
            tracing::debug!("d");
            tracing::info!("i");
            tracing::warn!("w");
            tracing::error!("e");
        });
        let levels: Vec<Level> = lines.iter().map(|l| l.level).collect();
        assert_eq!(
            levels,
            vec![
                Level::TRACE,
                Level::DEBUG,
                Level::INFO,
                Level::WARN,
                Level::ERROR
            ]
        );
    }

    #[test]
    fn message_only_event_renders_bare_message() {
        let lines = capture(|| tracing::info!("hello world"));
        assert_eq!(lines[0].message, "hello world");
        assert!(lines[0].fields.is_empty());
    }

    #[test]
    fn formatted_message_renders_without_field_suffix() {
        let lines = capture(|| tracing::info!("value is {}", 41 + 1));
        assert_eq!(lines[0].message, "value is 42");
    }

    #[test]
    fn fields_cross_in_their_recorded_types_beside_the_message() {
        let lines = capture(|| {
            tracing::warn!(
                name = "eth",
                count = 7u64,
                signed = -3i64,
                ratio = 0.5f64,
                ready = true,
                answer = ?Some(9),
                "changed"
            );
        });
        assert_eq!(lines[0].message, "changed");
        assert_eq!(
            pairs(&lines[0]),
            vec![
                ("name", LogValue::Text("eth".to_owned())),
                ("count", LogValue::Unsigned(7)),
                ("signed", LogValue::Signed(-3)),
                ("ratio", LogValue::Float(0.5)),
                ("ready", LogValue::Boolean(true)),
                ("answer", LogValue::Text("Some(9)".to_owned())),
            ]
        );
    }

    #[test]
    fn fieldset_without_message_leaves_the_message_empty() {
        let lines = capture(|| tracing::info!(a = 1u64, b = "x"));
        assert_eq!(lines[0].message, "");
        assert_eq!(
            pairs(&lines[0]),
            vec![
                ("a", LogValue::Unsigned(1)),
                ("b", LogValue::Text("x".to_owned())),
            ]
        );
    }

    #[test]
    fn events_carry_the_call_site() {
        let expected_line = line!() + 1;
        let lines = capture(|| tracing::info!("located"));
        assert_eq!(lines[0].target, module_path!());
        assert!(lines[0].file.as_deref().is_some_and(is_this_file));
        assert_eq!(lines[0].line, Some(expected_line));
    }

    #[test]
    fn defaulted_log_reports_no_call_site_and_no_fields() {
        let sink = Arc::new(Captured::default());
        Arc::clone(&sink).log(Level::INFO, "bare");
        let line = sink.lines.lock()[0].clone();
        assert_eq!(
            (line.target.as_str(), line.file, line.line),
            ("", None, None)
        );
        assert!(line.fields.is_empty());
    }

    #[test]
    fn panic_hook_reports_the_location_structurally() {
        let sink = Arc::new(Captured::default());
        // The hook is process-global; restore it so the rest of the binary
        // still panics through the default hook under a plain `cargo test`.
        let previous = std::panic::take_hook();
        set_panic_hook(Arc::new(Arc::clone(&sink)));
        let _ = std::panic::catch_unwind(|| panic!("boom"));
        std::panic::set_hook(previous);
        let reported = sink.lines.lock().first().cloned();
        let reported = reported.expect("the hook reported the panic");
        assert_eq!(reported.message, "panic: boom");
        assert_eq!(reported.target, PANIC_TARGET);
        assert!(reported.file.as_deref().is_some_and(is_this_file));
        assert!(reported.line.is_some());
    }

    fn is_this_file(path: &str) -> bool {
        path.ends_with("tracing.rs")
    }

    fn pairs(line: &Line) -> Vec<(&str, LogValue)> {
        line.fields
            .iter()
            .map(|f| (f.name, f.value.clone()))
            .collect()
    }

    #[test]
    fn spans_are_inert_no_ops() {
        let lines = capture(|| {
            let span = tracing::info_span!("work", key = "v");
            let _entered = span.enter();
            span.record("key", "v2");
        });
        assert!(
            lines.is_empty(),
            "span lifecycle produced events: {lines:?}"
        );
    }

    #[test]
    fn format_panic_with_and_without_location() {
        assert_eq!(
            format_panic("boom", Some(("src/lib.rs", 42))),
            "panic: boom at src/lib.rs:42"
        );
        assert_eq!(format_panic("boom", None), "panic: boom");
    }
}
