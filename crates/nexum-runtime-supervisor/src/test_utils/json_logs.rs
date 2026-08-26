//! Tracing capture in the JSON shape `nexum-launch` installs, so a test can
//! tell a field on the span from a field on the event.

use std::sync::{Arc, Mutex};

use tracing::{Level, Subscriber};

#[derive(Clone, Default)]
pub(crate) struct JsonLogs(Arc<Mutex<Vec<u8>>>);

impl JsonLogs {
    /// The first captured line carrying `message`.
    pub(crate) fn line(&self, message: &str) -> serde_json::Value {
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

/// Mirrors `nexum_launch::json_subscriber`: event fields flattened onto the
/// object, the innermost span alone under `span`.
pub(crate) fn json_collector(sink: JsonLogs, max: Level) -> impl Subscriber + Send + Sync {
    tracing_subscriber::fmt()
        .with_max_level(max)
        .json()
        .flatten_event(true)
        .with_span_list(false)
        .with_writer(sink)
        .finish()
}
