//! `nexum:host/logging`: builds a [`LogRecord`] from the guest's `log` and
//! `log-event` calls and routes both through the same router.

use tracing_core::Level;

use nexum_runtime_api::RuntimeTypes;
use nexum_runtime_api::bindings::nexum;
use nexum_runtime_logs::{LogChannel, LogField, LogRecord, LogSource, LogValue};

use crate::state::HostState;

impl<T: RuntimeTypes> nexum::host::logging::Host for HostState<T> {
    async fn log(&mut self, level: nexum::host::logging::Level, message: String) {
        self.route(LogRecord::now(
            self.run.clone(),
            LogChannel::HostInterface,
            lift_level(level),
            message,
        ));
    }

    async fn log_event(
        &mut self,
        level: nexum::host::logging::Level,
        source: nexum::host::logging::Source,
        message: String,
        fields: Vec<nexum::host::logging::Field>,
    ) {
        self.route(
            LogRecord::now(
                self.run.clone(),
                LogChannel::HostInterface,
                lift_level(level),
                message,
            )
            .with_source(LogSource {
                target: source.target,
                file: source.file,
                line: source.line,
            })
            .with_fields(fields.into_iter().map(lift_field).collect()),
        );
    }
}

impl<T: RuntimeTypes> HostState<T> {
    /// Let the operator filter pick the record's sinks, then bound what
    /// reaches one. Neither is in the router: it renders before it
    /// retains, and the death record it also carries is gated by neither.
    fn route(&mut self, record: LogRecord) {
        self.log_filter
            .route(&self.log_router, &self.log_bounds, record);
    }
}

/// WIT edge: the generated wire enum crosses into the level vocabulary
/// here, one of the only two such conversions.
fn lift_level(level: nexum::host::logging::Level) -> Level {
    use nexum::host::logging::Level as Wire;
    match level {
        Wire::Trace => Level::TRACE,
        Wire::Debug => Level::DEBUG,
        Wire::Info => Level::INFO,
        Wire::Warn => Level::WARN,
        Wire::Error => Level::ERROR,
    }
}

/// Lift one wire field into the log pipeline's vocabulary.
fn lift_field(field: nexum::host::logging::Field) -> LogField {
    use nexum::host::logging::FieldValue as Wire;
    LogField {
        name: field.name,
        value: match field.value {
            Wire::Text(value) => LogValue::Text(value),
            Wire::Unsigned(value) => LogValue::Unsigned(value),
            Wire::Signed(value) => LogValue::Signed(value),
            Wire::Float(value) => LogValue::Float(value),
            Wire::Boolean(value) => LogValue::Boolean(value),
        },
    }
}
