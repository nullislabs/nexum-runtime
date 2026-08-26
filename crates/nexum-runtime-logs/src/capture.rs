//! Scoped capture of formatted `tracing` output.
//!
//! An "operator can see it" done-condition is otherwise satisfiable by a
//! field nobody formats. [`LogCapture`] keeps the bytes a `fmt` subscriber
//! wrote, so a test asserts on the line an operator would read.

// A harness that cannot format into its own buffer has nothing to recover from.
#![allow(clippy::expect_used)]

use std::sync::Arc;

use parking_lot::Mutex;
use tracing_core::{Level, Subscriber};

/// The bytes a `fmt` subscriber formatted, shared by every clone.
#[derive(Clone, Default)]
pub struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    /// An empty capture.
    pub fn new() -> Self {
        Self::default()
    }

    /// A plain-text `fmt` subscriber writing here, admitting up to `max_level`.
    ///
    /// ANSI is off, so a level or field name survives a `contains` assertion.
    pub fn subscriber(&self, max_level: Level) -> impl Subscriber + Send + Sync + 'static {
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(max_level)
            .with_writer(self.clone())
            .finish()
    }

    /// Makes [`Self::subscriber`] the calling thread's default until the guard
    /// drops. Work spawned onto another thread keeps the global default.
    #[must_use = "dropping the guard uninstalls the subscriber, so nothing is captured"]
    pub fn install(&self, max_level: Level) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(self.subscriber(max_level))
    }

    /// Everything captured so far.
    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().clone()).expect("tracing output is UTF-8")
    }
}

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

/// Text `emitting` printed, admitting up to `max_level`.
pub fn capture_logs(max_level: Level, emitting: impl FnOnce()) -> String {
    let capture = LogCapture::new();
    tracing::subscriber::with_default(capture.subscriber(max_level), emitting);
    capture.text()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_above_the_maximum_is_left_out() {
        let out = capture_logs(Level::INFO, || {
            tracing::info!("admitted");
            tracing::debug!("filtered");
        });
        assert!(out.contains("admitted"), "{out}");
        assert!(!out.contains("filtered"), "{out}");
    }

    #[test]
    fn the_capture_holds_no_ansi_escapes() {
        let out = capture_logs(Level::WARN, || tracing::warn!(field = 1, "message"));
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert!(out.contains("WARN") && out.contains("field=1"), "{out}");
    }

    #[test]
    fn install_captures_only_while_the_guard_is_alive() {
        let capture = LogCapture::new();
        let guard = capture.install(Level::INFO);
        tracing::info!("inside");
        drop(guard);
        tracing::info!("outside");
        let out = capture.text();
        assert!(out.contains("inside"), "{out}");
        assert!(!out.contains("outside"), "{out}");
    }
}
