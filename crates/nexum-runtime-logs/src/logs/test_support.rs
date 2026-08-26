//! Fixtures shared by the capture-point tests.

use parking_lot::Mutex;

use nexum_primitives::module_id::ModuleId;

use super::{LogPage, LogRecord, RunId, RunLogStore, RunMeta};

pub(super) fn run_id() -> RunId {
    RunId::new(ModuleId::parse("m").expect("valid module name"), 0)
}

/// Store keeping every appended record, so a test reads the retention half
/// without the ring's eviction rules in the way.
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
