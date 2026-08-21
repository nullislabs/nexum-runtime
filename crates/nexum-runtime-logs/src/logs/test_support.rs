//! Fixtures shared by the capture-point tests: a run identity, a store
//! that keeps everything appended, and a sink for the console half.

use std::sync::Arc;

use parking_lot::Mutex;
use tracing_core::Level;

use nexum_primitives::module_id::ModuleId;

use super::{LogPage, LogRecord, RunId, RunLogStore, RunMeta};

/// The run every fixture writes as.
pub(super) fn run_id() -> RunId {
    RunId::new(ModuleId::parse("m").expect("valid module name"), 0)
}

/// Store keeping every appended record, so a test reads the retention
/// half without the ring's eviction rules in the way.
#[derive(Default)]
pub(super) struct CaptureStore {
    pub(super) records: Mutex<Vec<LogRecord>>,
}

impl CaptureStore {
    pub(super) fn messages(&self) -> Vec<String> {
        self.records
            .lock()
            .iter()
            .map(|r| r.message.clone())
            .collect()
    }
}

impl RunLogStore for CaptureStore {
    fn append(&self, record: LogRecord) {
        self.records.lock().push(record);
    }
    fn list_runs(&self, _module: &str) -> Vec<RunMeta> {
        Vec::new()
    }
    fn read(&self, _run: &RunId, _cursor: u64) -> LogPage {
        LogPage::default()
    }
}

/// A `tracing` sink keeping what the subscriber wrote.
#[derive(Clone, Default)]
pub(super) struct Console(Arc<Mutex<Vec<u8>>>);

impl Console {
    /// Console text `emitting` produced, captured at every level so a
    /// subscriber default cannot pass for a filter decision.
    pub(super) fn printed(emitting: impl FnOnce()) -> String {
        let sink = Self::default();
        let collector = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(Level::TRACE)
            .with_writer(sink.clone())
            .finish();
        tracing::subscriber::with_default(collector, emitting);
        String::from_utf8(sink.0.lock().clone()).expect("console output is UTF-8")
    }
}

impl std::io::Write for Console {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Console {
    type Writer = Console;
    fn make_writer(&'a self) -> Console {
        self.clone()
    }
}
