//! Test fixture: floods `nexum:host/logging` in one dispatch, alternating
//! oversized messages with long field lists, well past the burst. A stderr
//! marker opens the dispatch, before the flood spends the bucket both
//! capture points draw on.

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

/// Records emitted per dispatch, and the fields each odd one carries.
const RECORDS: u32 = 32;

struct LogBomb;

impl Guest for LogBomb {
    fn init(_config: Vec<(String, String)>) -> Result<(), Fault> {
        Ok(())
    }

    fn on_trigger(_trigger: types::Trigger) -> Result<(), Fault> {
        eprintln!("log-bomb flooding {RECORDS}");
        let oversized = "x".repeat(4096);
        let fields: Vec<logging::Field> = (0..RECORDS)
            .map(|i| logging::Field {
                name: format!("f{i}"),
                value: logging::FieldValue::Text("v".repeat(64)),
            })
            .collect();
        let source = logging::Source {
            target: "log-bomb::flood".to_string(),
            file: Some("src/lib.rs".to_string()),
            line: Some(line!()),
        };
        for i in 0..RECORDS {
            if i % 2 == 0 {
                logging::log_event(logging::Level::Info, &source, &oversized, &[]);
            } else {
                logging::log_event(logging::Level::Info, &source, "small", &fields);
            }
        }
        Ok(())
    }
}

export!(LogBomb);
