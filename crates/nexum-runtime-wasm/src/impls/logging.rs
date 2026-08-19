//! `nexum:host/logging`: builds a [`LogRecord`] from the guest's `log` call
//! and routes it.

use tracing_core::Level;

use nexum_runtime_api::RuntimeTypes;
use nexum_runtime_api::bindings::nexum;
use nexum_runtime_logs::{LogChannel, LogRecord};

use crate::state::HostState;

impl<T: RuntimeTypes> nexum::host::logging::Host for HostState<T> {
    async fn log(&mut self, level: nexum::host::logging::Level, message: String) {
        // WIT edge: the generated wire enum crosses into the level
        // vocabulary here, one of the only two such conversions.
        use nexum::host::logging::Level as WireLevel;
        let level = match level {
            WireLevel::Trace => Level::TRACE,
            WireLevel::Debug => Level::DEBUG,
            WireLevel::Info => Level::INFO,
            WireLevel::Warn => Level::WARN,
            WireLevel::Error => Level::ERROR,
        };
        self.log_router.record(LogRecord::now(
            self.run.clone(),
            LogChannel::HostInterface,
            level,
            message,
        ));
    }
}
