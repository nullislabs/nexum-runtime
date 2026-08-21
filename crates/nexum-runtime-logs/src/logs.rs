//! Typed module-log pipeline.
//!
//! Three capture points build [`LogRecord`]s for one [`LogRouter`]: the
//! `nexum:host/logging` glue, the per-store stdout/stderr pipes, and the
//! supervisor death path. The first two pass one shared
//! [`SharedLogBounds`] and one [`SharedLogFilter`] per run; the death path
//! is host-synthesized, so it is neither bounded nor filtered. The router
//! fans each record to a host `tracing` event and the retention store. [`LogPipeline`] is the shared handle,
//! carrying the write side and the store's read side.
//!
//! One guest panic yields three records distinguished by [`LogChannel`]
//! (stderr, host logging call, supervisor death), redundancy covering
//! channels that survive different failure modes. The first two spend the
//! same bucket, so a run that has already flooded keeps only the ungated
//! death record.

mod bounds;
mod filter;
mod stdio;
mod store;
#[cfg(test)]
mod test_support;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::SystemTime;

use strum::IntoStaticStr;
use tracing_core::Level;

use nexum_primitives::module_id::ModuleId;

pub use bounds::SharedLogBounds;
pub use filter::SharedLogFilter;
pub use stdio::StdioStream;
pub use store::{InMemoryRunLogStore, LogPage, RunLogStore, RunMeta};

/// Identity of one module run; a restart increments `seq`, keying retention.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunId {
    /// Module namespace this run belongs to.
    pub module: ModuleId,
    /// Monotonic run counter within the module; 0 is the first boot.
    pub seq: u64,
    /// Wall-clock instant the run was instantiated.
    pub started_at: SystemTime,
}

impl RunId {
    /// Mint a run for `module` at sequence `seq`.
    pub fn new(module: ModuleId, seq: u64) -> Self {
        Self {
            module,
            seq,
            started_at: SystemTime::now(),
        }
    }
}

/// Which capture point produced a record; the snake_case name is the tracing
/// `channel` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[non_exhaustive]
pub enum LogChannel {
    /// The `nexum:host/logging` glue: an explicit guest `log` call.
    HostInterface,
    /// A line captured from the guest's stdout pipe.
    Stdout,
    /// A line captured from the guest's stderr pipe.
    Stderr,
    /// Synthesized by the supervisor when a run dies via trap or exit.
    Panic,
}

/// Where an event was emitted from, mirroring `tracing::Metadata`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogSource {
    /// Guest-reported target, empty for a capture point that has none.
    pub target: String,
    /// Source file of the call site.
    pub file: Option<String>,
    /// Line within [`LogSource::file`].
    pub line: Option<u32>,
}

impl LogSource {
    /// Bytes charged against the retention budget; the line number rides
    /// in [`RECORD_OVERHEAD`].
    fn cost(&self) -> usize {
        self.target.len() + self.file.as_ref().map_or(0, String::len)
    }
}

/// One structured key-value pair recorded with an event.
#[derive(Debug, Clone, PartialEq)]
pub struct LogField {
    /// Field name as the guest declared it.
    pub name: String,
    /// Recorded value.
    pub value: LogValue,
}

impl LogField {
    /// Bytes charged against the retention budget; the fixed part is what
    /// keeps a long list of tiny fields from outrunning the budget by the
    /// ratio between one byte and the retained [`LogField`].
    fn cost(&self) -> usize {
        FIELD_OVERHEAD + self.name.len() + self.value.cost()
    }

    /// Bytes the tracing render costs, counting the `=` and the separator
    /// [`render_fields`] writes around the pair, which the guest never
    /// sent but the sink still writes.
    fn rendered_len(&self) -> usize {
        self.name.len() + self.value.rendered_len() + 2
    }
}

/// A field value in the type the guest recorded it at.
#[derive(Debug, Clone, PartialEq, derive_more::Display)]
pub enum LogValue {
    /// A string, or a `Debug` rendering the guest flattened for the wire.
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

impl LogValue {
    /// Bytes charged against the retention budget; a scalar is charged at
    /// its wire width, not its rendered length.
    fn cost(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Unsigned(_) | Self::Signed(_) | Self::Float(_) => 8,
            Self::Boolean(_) => 1,
        }
    }

    /// Bytes the tracing render costs, counted through the `Display` the
    /// render itself uses. A scalar is counted rather than charged a flat
    /// width, because `f64` renders its whole decimal expansion and a
    /// subnormal is over three hundred digits wide.
    fn rendered_len(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            scalar => {
                let mut bytes = ByteCount(0);
                let _ = write!(bytes, "{scalar}");
                bytes.0
            }
        }
    }
}

/// A `fmt::Write` sink that keeps only the byte count, so measuring a
/// render allocates nothing.
struct ByteCount(usize);

impl std::fmt::Write for ByteCount {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0 += s.len();
        Ok(())
    }
}

/// One captured log line from any capture point.
#[derive(Debug, Clone)]
pub struct LogRecord {
    /// Run the line belongs to.
    pub run: RunId,
    /// Wall-clock capture time.
    pub ts: SystemTime,
    /// Capture point of origin.
    pub channel: LogChannel,
    /// Line severity.
    pub level: Level,
    /// The line text.
    pub message: String,
    /// Call site the guest reported; default on the stdio and death paths.
    pub source: LogSource,
    /// Structured fields recorded with the event, in record order.
    pub fields: Vec<LogField>,
}

impl LogRecord {
    /// Record stamped at the current instant, with no source and no fields.
    pub fn now(run: RunId, channel: LogChannel, level: Level, message: String) -> Self {
        Self {
            run,
            ts: SystemTime::now(),
            channel,
            level,
            message,
            source: LogSource::default(),
            fields: Vec::new(),
        }
    }

    /// Attach the guest-reported call site.
    #[must_use]
    pub fn with_source(mut self, source: LogSource) -> Self {
        self.source = source;
        self
    }

    /// Attach the event's structured fields.
    #[must_use]
    pub fn with_fields(mut self, fields: Vec<LogField>) -> Self {
        self.fields = fields;
        self
    }

    /// Byte cost charged against the per-run retention budget; every carried
    /// byte is charged, so text moved out of the message and into a field
    /// cannot evade the budget.
    fn cost(&self) -> usize {
        RECORD_OVERHEAD
            + self.message.len()
            + self.source.cost()
            + self.fields.iter().map(LogField::cost).sum::<usize>()
    }
}

/// Fixed per-record charge added to message bytes so empty messages still
/// count against the `[limits.logs]` byte budget. It covers the retained
/// [`LogRecord`] itself, which the source and the field list widened by
/// eighty bytes.
const RECORD_OVERHEAD: usize = 208;

/// Fixed per-field charge, covering the [`LogField`] the ring holds rather
/// than the bytes the guest spelled; a guest sending empty-named booleans
/// would otherwise be charged one byte for forty-eight retained.
const FIELD_OVERHEAD: usize = 64;

/// Fans every captured record to a host `tracing` event and the
/// retention store.
pub struct LogRouter {
    store: Arc<dyn RunLogStore>,
    /// Woken after each append; a reader must arm before reading (see
    /// [`LogPipeline::appended`]).
    appended: Arc<tokio::sync::Notify>,
}

impl LogRouter {
    /// Router writing into `store`.
    pub fn new(store: Arc<dyn RunLogStore>) -> Self {
        Self {
            store,
            appended: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Emit the tracing event, retain the record, then wake append waiters.
    pub fn record(&self, record: LogRecord) {
        emit_tracing(&record);
        self.retain(record);
    }

    /// Retain without emitting: the record cleared the retention floor but
    /// not the console one. Crate-internal, because only the filter can
    /// tell the two floors apart.
    pub(crate) fn retain(&self, record: LogRecord) {
        self.store.append(record);
        self.appended.notify_waiters();
    }

    fn store(&self) -> &Arc<dyn RunLogStore> {
        &self.store
    }
}

/// Emit one record as a host tracing event at its own level. The guest's
/// target rides as `source`, because `target` is a reserved argument of the
/// `tracing` macros. Both ride as `Option`, which `tracing` records as an
/// absent field rather than a blank one, so the stdio and death paths emit
/// exactly the line they did before this verb carried structure.
fn emit_tracing(record: &LogRecord) {
    let module = record.run.module.as_str();
    let run = record.run.seq;
    let channel: &'static str = record.channel.into();
    let message = record.message.as_str();
    let target = record.source.target.as_str();
    let source = (!target.is_empty()).then_some(target);
    let rendered = render_fields(&record.fields);
    let fields = rendered.as_deref();
    if record.level == Level::TRACE {
        tracing::trace!(module, run, channel, source, fields, "{message}");
    } else if record.level == Level::DEBUG {
        tracing::debug!(module, run, channel, source, fields, "{message}");
    } else if record.level == Level::INFO {
        tracing::info!(module, run, channel, source, fields, "{message}");
    } else if record.level == Level::WARN {
        tracing::warn!(module, run, channel, source, fields, "{message}");
    } else {
        tracing::error!(module, run, channel, source, fields, "{message}");
    }
}

/// Render structured fields into one `key=value ...` string, because the
/// `tracing` macros take only statically named fields. `None` for a
/// field-less record, so the common path allocates nothing.
fn render_fields(fields: &[LogField]) -> Option<String> {
    let (first, rest) = fields.split_first()?;
    let mut line = format!("{}={}", first.name, first.value);
    for field in rest {
        let _ = write!(line, " {}={}", field.name, field.value);
    }
    Some(line)
}

/// Shared log pipeline threaded into every module store; cheap to clone.
#[derive(Clone)]
pub struct LogPipeline {
    router: Arc<LogRouter>,
}

impl LogPipeline {
    /// Pipeline over an arbitrary retention backend.
    pub fn new(store: Arc<dyn RunLogStore>) -> Self {
        Self {
            router: Arc::new(LogRouter::new(store)),
        }
    }

    /// Pipeline over the byte-bounded in-memory backend, sized by
    /// `[limits.logs]`.
    pub fn in_memory(limits: nexum_runtime_config::LogRetentionLimits) -> Self {
        Self::new(Arc::new(InMemoryRunLogStore::new(limits)))
    }

    /// The write handle the capture points route through.
    pub fn router(&self) -> Arc<LogRouter> {
        self.router.clone()
    }

    /// Notify woken after each append; arm a `notified()` future before
    /// reading so an append is not lost.
    pub fn appended(&self) -> Arc<tokio::sync::Notify> {
        self.router.appended.clone()
    }

    /// Runs recorded for `module`, oldest retained first.
    pub fn list_runs(&self, module: &str) -> Vec<RunMeta> {
        self.router.store().list_runs(module)
    }

    /// Page a run's retained records from `cursor` (0 for the start).
    pub fn read(&self, run: &RunId, cursor: u64) -> LogPage {
        self.router.store().read(run, cursor)
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use super::*;

    fn test_module_id() -> ModuleId {
        ModuleId::parse("m").expect("valid module name")
    }

    /// Store that records appends so the fan-out test can inspect them.
    struct CountingStore {
        appended: Mutex<Vec<LogRecord>>,
    }

    impl RunLogStore for CountingStore {
        fn append(&self, record: LogRecord) {
            self.appended.lock().push(record);
        }
        fn list_runs(&self, _module: &str) -> Vec<RunMeta> {
            Vec::new()
        }
        fn read(&self, _run: &RunId, _cursor: u64) -> LogPage {
            LogPage::default()
        }
    }

    #[test]
    fn router_fans_out_to_the_retention_store() {
        let store = Arc::new(CountingStore {
            appended: Mutex::new(Vec::new()),
        });
        let router = LogRouter::new(store.clone());
        router.record(LogRecord::now(
            RunId::new(test_module_id(), 0),
            LogChannel::HostInterface,
            Level::INFO,
            "hello".to_owned(),
        ));
        let appended = store.appended.lock();
        assert_eq!(appended.len(), 1, "retention consumer saw the record");
        assert_eq!(appended[0].message, "hello");
        assert_eq!(appended[0].channel, LogChannel::HostInterface);
    }

    #[test]
    fn pipeline_read_side_reaches_the_backend() {
        let limits = nexum_runtime_config::LogRetentionLimits {
            bytes_per_run: std::num::NonZeroUsize::new(1024).unwrap(),
            runs_retained: std::num::NonZeroUsize::new(4).unwrap(),
        };
        let pipeline = LogPipeline::in_memory(limits);
        let run = RunId::new(test_module_id(), 0);
        pipeline.router().record(LogRecord::now(
            run.clone(),
            LogChannel::Stdout,
            Level::INFO,
            "line".to_owned(),
        ));
        let runs = pipeline.list_runs("m");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run.seq, 0);
        let page = pipeline.read(&run, 0);
        assert_eq!(page.records[0].message, "line");
    }

    fn field(name: &str, value: LogValue) -> LogField {
        LogField {
            name: name.to_owned(),
            value,
        }
    }

    #[test]
    fn cost_charges_the_source_and_every_field() {
        let bare = LogRecord::now(
            RunId::new(test_module_id(), 0),
            LogChannel::HostInterface,
            Level::INFO,
            "hi".to_owned(),
        );
        let rich = bare
            .clone()
            .with_source(LogSource {
                target: "guest::work".to_owned(),
                file: Some("src/lib.rs".to_owned()),
                line: Some(7),
            })
            .with_fields(vec![field("n", LogValue::Unsigned(9))]);
        assert_eq!(bare.cost(), RECORD_OVERHEAD + 2);
        assert_eq!(
            rich.cost(),
            bare.cost() + 11 + 10 + (FIELD_OVERHEAD + 1 + 8),
            "the retention budget charges the carried source and fields",
        );
    }

    #[test]
    fn cost_charges_a_long_field_list_per_field() {
        let spam = LogRecord::now(
            RunId::new(test_module_id(), 0),
            LogChannel::HostInterface,
            Level::INFO,
            String::new(),
        )
        .with_fields(vec![field("", LogValue::Boolean(true)); 1000]);
        assert!(
            spam.cost() >= 1000 * FIELD_OVERHEAD,
            "a guest cannot hold the ring open with fields it is charged a byte for",
        );
    }

    #[test]
    fn fields_render_in_record_order_for_the_tracing_event() {
        let rendered = render_fields(&[
            field("key", LogValue::Text("value".to_owned())),
            field("n", LogValue::Signed(-9)),
            field("ok", LogValue::Boolean(true)),
        ]);
        assert_eq!(rendered.as_deref(), Some("key=value n=-9 ok=true"));
        assert_eq!(
            render_fields(&[]),
            None,
            "a field-less record leaves the tracing field absent, not blank",
        );
    }

    #[test]
    fn a_structureless_record_emits_the_line_it_did_before_the_verb_grew() {
        let bare = LogRecord::now(
            RunId::new(test_module_id(), 0),
            LogChannel::Stdout,
            Level::INFO,
            "plain".to_owned(),
        );
        let rich = bare
            .clone()
            .with_source(LogSource {
                target: "guest::work".to_owned(),
                ..LogSource::default()
            })
            .with_fields(vec![field("n", LogValue::Unsigned(9))]);
        let out = test_support::Console::printed(|| {
            emit_tracing(&bare);
            emit_tracing(&rich);
        });
        let (first, second) = out.split_once('\n').expect("two events were emitted");
        assert!(
            !first.contains("source") && !first.contains("fields"),
            "the stdio path gained no blank fields: {first}",
        );
        assert!(
            second.contains("source=\"guest::work\"") && second.contains("fields=\"n=9\""),
            "a structured record renders both: {second}",
        );
    }

    #[test]
    fn channel_names_are_snake_case_for_the_tracing_field() {
        let s: &'static str = LogChannel::HostInterface.into();
        assert_eq!(s, "host_interface");
        let s: &'static str = LogChannel::Panic.into();
        assert_eq!(s, "panic");
    }
}
