//! A domain-SDK-shaped consumer binds `bind_host_logging_via_wit_bindgen!`
//! against its own generated `nexum::host::logging`, with no base block
//! (no `WitBindgenHost`, no `nexum:host/types`) in scope.

mod nexum {
    pub mod host {
        /// Stands in for the per-cdylib wit-bindgen `logging` output.
        pub mod logging {
            use parking_lot::Mutex;

            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            pub enum Level {
                Trace,
                Debug,
                Info,
                Warn,
                Error,
            }

            pub static RECORDED: Mutex<Vec<(Level, String)>> = Mutex::new(Vec::new());

            pub fn log(level: Level, message: &str) {
                RECORDED.lock().push((level, message.to_owned()));
            }
        }
    }
}

nexum_sdk::bind_host_logging_via_wit_bindgen!();

use nexum::host::logging::Level as Wire;

/// The recorder is process-wide, so every assertion is a containment
/// check rather than an equality on the whole log.
fn recorded(line: &str) -> Option<Wire> {
    let recorded = nexum::host::logging::RECORDED.lock();
    recorded
        .iter()
        .find(|(_, message)| message == line)
        .map(|(level, _)| *level)
}

#[test]
fn sink_forwards_to_the_bound_logging_call() {
    use nexum_sdk::tracing::LogSink as _;

    HostLogSink.log(nexum_sdk::Level::INFO, "ready");
    assert_eq!(recorded("ready"), Some(Wire::Info));
}

#[test]
fn level_mapping_covers_the_wire_enum() {
    for (level, wire) in [
        (nexum_sdk::Level::ERROR, Wire::Error),
        (nexum_sdk::Level::WARN, Wire::Warn),
        (nexum_sdk::Level::INFO, Wire::Info),
        (nexum_sdk::Level::DEBUG, Wire::Debug),
        (nexum_sdk::Level::TRACE, Wire::Trace),
    ] {
        assert_eq!(Wire::from(level), wire);
    }
}

#[test]
fn facade_install_routes_events_to_the_bound_logging_call() {
    install_tracing();
    tracing::warn!("through the facade");
    assert_eq!(recorded("through the facade"), Some(Wire::Warn));
}
