use std::cell::RefCell;

use nexum_sdk::Level;
use nexum_sdk::host::LoggingHost;

/// One recorded log line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogLine {
    /// Severity the module passed.
    pub level: Level,
    /// Message body.
    pub message: String,
}

/// In-memory [`LoggingHost`] that buffers every emitted line.
#[derive(Default)]
pub struct MockLogging {
    lines: RefCell<Vec<LogLine>>,
}

impl MockLogging {
    /// All buffered log lines, in emission order.
    pub fn lines(&self) -> Vec<LogLine> {
        self.lines.borrow().clone()
    }

    /// `true` if any buffered line contains `needle` (substring match).
    pub fn contains(&self, needle: &str) -> bool {
        self.lines
            .borrow()
            .iter()
            .any(|l| l.message.contains(needle))
    }

    /// Count of lines at `level`.
    pub fn count_at(&self, level: Level) -> usize {
        self.lines
            .borrow()
            .iter()
            .filter(|l| l.level == level)
            .count()
    }
}

impl LoggingHost for MockLogging {
    fn log(&self, level: Level, message: &str) {
        self.lines.borrow_mut().push(LogLine {
            level,
            message: message.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logging_captures_lines_and_filters_by_level() {
        let log = MockLogging::default();
        log.log(Level::INFO, "hello");
        log.log(Level::WARN, "uh oh");
        log.log(Level::INFO, "still here");

        assert_eq!(log.lines().len(), 3);
        assert_eq!(log.count_at(Level::INFO), 2);
        assert_eq!(log.count_at(Level::WARN), 1);
        assert!(log.contains("uh oh"));
    }
}
