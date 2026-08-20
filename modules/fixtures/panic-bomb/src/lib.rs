//! # panic-bomb (test fixture)
//!
//! Installs the nexum-sdk tracing facade (subscriber + panic hook) in
//! `init` and panics on every `on_trigger`. The hook forwards the panic
//! to stderr and the host logging call before the trap reaches the
//! supervisor, so one death leaves Stderr, HostInterface, and Panic
//! records. Test-only.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: [
        "../../../wit/nexum-host",
    ],
    world: "nexum:host/trigger-module",
    generate_all,
});

use nexum::host::{logging, types};
use nexum_sdk::tracing::{LogField, LogSource, LogValue};

/// Routes facade lines to the bound host logging import.
struct HostLogSink;

impl nexum_sdk::tracing::LogSink for HostLogSink {
    fn log_event(
        &self,
        level: nexum_sdk::Level,
        source: LogSource<'_>,
        message: &str,
        fields: &[LogField],
    ) {
        use nexum_sdk::Level;
        // `Level` is a set of associated consts, so compare rather than
        // match; the five tiers are total, hence the final `Trace` arm.
        let level = if level == Level::ERROR {
            logging::Level::Error
        } else if level == Level::WARN {
            logging::Level::Warn
        } else if level == Level::INFO {
            logging::Level::Info
        } else if level == Level::DEBUG {
            logging::Level::Debug
        } else {
            logging::Level::Trace
        };
        let source = logging::Source {
            target: source.target.to_owned(),
            file: source.file.map(str::to_owned),
            line: source.line,
        };
        let fields: Vec<logging::Field> = fields
            .iter()
            .map(|field| logging::Field {
                name: field.name.to_owned(),
                value: match &field.value {
                    LogValue::Text(v) => logging::FieldValue::Text(v.clone()),
                    LogValue::Unsigned(v) => logging::FieldValue::Unsigned(*v),
                    LogValue::Signed(v) => logging::FieldValue::Signed(*v),
                    LogValue::Float(v) => logging::FieldValue::Float(*v),
                    LogValue::Boolean(v) => logging::FieldValue::Boolean(*v),
                },
            })
            .collect();
        logging::log(level, &source, message, &fields);
    }
}

struct PanicBomb;

impl Guest for PanicBomb {
    fn init(_config: Vec<(String, String)>) -> Result<(), Fault> {
        nexum_sdk::tracing::init(HostLogSink);
        tracing::info!("panic-bomb init (will panic)");
        Ok(())
    }

    fn on_trigger(_trigger: types::Trigger) -> Result<(), Fault> {
        panic!("panic-bomb detonated");
    }
}

export!(PanicBomb);
