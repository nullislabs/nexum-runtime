//! Scoped capture of host tracing in the JSON shape the launcher installs.
//!
//! A field on a span and a field on an event render at different depths, so
//! a test that asserts the depth needs the real formatter rather than a
//! field visitor.

use std::sync::{Arc, Mutex};

use tracing::{Level, Subscriber};

/// Captured JSON lines, cheap to clone; every clone writes to one buffer.
#[derive(Clone, Default)]
pub struct JsonLogs(Arc<Mutex<Vec<u8>>>);

impl JsonLogs {
    /// The first captured line carrying `message`.
    pub fn line(&self, message: &str) -> serde_json::Value {
        let bytes = self.0.lock().expect("sink is not poisoned").clone();
        let text = String::from_utf8(bytes).expect("log output is UTF-8");
        let raw = text
            .lines()
            .find(|line| line.contains(message))
            .unwrap_or_else(|| panic!("no line said {message}:\n{text}"));
        serde_json::from_str(raw).expect("each line is one JSON object")
    }
}

impl std::io::Write for JsonLogs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("sink is not poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JsonLogs {
    type Writer = JsonLogs;

    fn make_writer(&'a self) -> JsonLogs {
        self.clone()
    }
}

/// Subscriber writing into `sink` in the launcher's JSON shape; a
/// `nexum-launch` test holds the two shapes to each other.
pub fn json_collector(sink: JsonLogs, max: Level) -> impl Subscriber + Send + Sync {
    tracing_subscriber::fmt()
        .with_max_level(max)
        .json()
        .flatten_event(true)
        .with_span_list(false)
        .with_writer(sink)
        .finish()
}
