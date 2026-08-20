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

            #[derive(Clone, Debug, PartialEq)]
            pub enum FieldValue {
                Text(String),
                Unsigned(u64),
                Signed(i64),
                Float(f64),
                Boolean(bool),
            }

            #[derive(Clone, Debug, PartialEq)]
            pub struct Field {
                pub name: String,
                pub value: FieldValue,
            }

            #[derive(Clone, Debug, PartialEq)]
            pub struct Source {
                pub target: String,
                pub file: Option<String>,
                pub line: Option<u32>,
            }

            /// One call as the wire carried it.
            pub type Recorded = (Level, Source, String, Vec<Field>);

            pub static RECORDED: Mutex<Vec<Recorded>> = Mutex::new(Vec::new());

            pub fn log(level: Level, source: &Source, message: &str, fields: &[Field]) {
                RECORDED
                    .lock()
                    .push((level, source.clone(), message.to_owned(), fields.to_vec()));
            }
        }
    }
}

nexum_sdk::bind_host_logging_via_wit_bindgen!();

use nexum::host::logging::{FieldValue, Level as Wire, Recorded};

/// The recorder is process-wide, so every assertion is a containment
/// check rather than an equality on the whole log.
fn recorded(line: &str) -> Option<Recorded> {
    let recorded = nexum::host::logging::RECORDED.lock();
    recorded
        .iter()
        .find(|(_, _, message, _)| message == line)
        .cloned()
}

#[test]
fn sink_forwards_to_the_bound_logging_call() {
    use nexum_sdk::tracing::LogSink as _;

    HostLogSink.log(nexum_sdk::Level::INFO, "ready");
    let (level, source, _, fields) = recorded("ready").expect("the bound call recorded the line");
    assert_eq!(level, Wire::Info);
    assert_eq!(source.target, "", "the defaulted `log` reports no source");
    assert_eq!(source.file, None);
    assert!(fields.is_empty());
}

#[test]
fn the_facade_lowers_the_source_and_the_fields_onto_the_wire() {
    install_tracing();
    tracing::info!(chain = 1u64, name = "eth", "structured");
    let (_, source, _, fields) = recorded("structured").expect("the event reached the wire");
    assert_eq!(source.target, module_path!());
    assert!(source.line.is_some(), "the call site crossed the wire");
    let pairs: Vec<(&str, &FieldValue)> =
        fields.iter().map(|f| (f.name.as_str(), &f.value)).collect();
    assert_eq!(
        pairs,
        vec![
            ("chain", &FieldValue::Unsigned(1)),
            ("name", &FieldValue::Text("eth".to_owned())),
        ]
    );
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
    let (level, ..) = recorded("through the facade").expect("the event reached the wire");
    assert_eq!(level, Wire::Warn);
}
