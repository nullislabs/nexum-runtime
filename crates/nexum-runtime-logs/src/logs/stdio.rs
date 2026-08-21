//! Per-store stdout/stderr capture: a [`StdoutStream`] line-buffering guest
//! output and routing each line as a [`LogRecord`] tagged with its run and
//! channel.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use tokio::io::AsyncWrite;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};

use tracing_core::Level;

use super::{LogChannel, LogRecord, LogRouter, RunId, SharedLogBounds, SharedLogFilter};

/// Per-store stdout or stderr sink; each [`StdoutStream::async_stream`] yields
/// a line-splitting writer bound to the run and channel.
pub struct StdioStream {
    router: Arc<LogRouter>,
    bounds: SharedLogBounds,
    filter: SharedLogFilter,
    run: RunId,
    channel: LogChannel,
}

impl StdioStream {
    /// Sink routing `channel` lines for `run` through `router`, spending the
    /// run's shared admission bucket per line.
    pub fn new(
        router: Arc<LogRouter>,
        bounds: SharedLogBounds,
        filter: SharedLogFilter,
        run: RunId,
        channel: LogChannel,
    ) -> Self {
        Self {
            router,
            bounds,
            filter,
            run,
            channel,
        }
    }
}

impl IsTerminal for StdioStream {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for StdioStream {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(LineWriter {
            router: self.router.clone(),
            bounds: self.bounds.clone(),
            filter: self.filter.clone(),
            run: self.run.clone(),
            channel: self.channel,
            buf: Vec::new(),
        })
    }
}

/// Line-splitting writer: one record per newline, so a code point split
/// across writes reassembles. The force-flush past the record cap is the
/// one cut that can land mid-character.
struct LineWriter {
    router: Arc<LogRouter>,
    bounds: SharedLogBounds,
    filter: SharedLogFilter,
    run: RunId,
    channel: LogChannel,
    buf: Vec<u8>,
}

impl LineWriter {
    /// Route every complete line in the buffer, then force-flush an
    /// unterminated remainder already past the record cap.
    fn drain(&mut self) {
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            self.route(&line[..line.len() - 1]);
        }
        if self.buf.len() > self.bounds.max_record_bytes() {
            let chunk = std::mem::take(&mut self.buf);
            self.route(&chunk);
        }
    }

    /// Emit any buffered partial line; idempotent, so shutdown and drop never
    /// double-emit.
    fn flush_remainder(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let rest = std::mem::take(&mut self.buf);
        self.route(&rest);
    }

    /// Decode one line, dropping a trailing `\r` and skipping empties, then
    /// route what the run's shared bounds admit. A captured line carries no
    /// target, so the operator filter sees its channel level and nothing else.
    fn route(&self, bytes: &[u8]) {
        let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        if bytes.is_empty() {
            return;
        }
        let mut record = LogRecord::now(
            self.run.clone(),
            self.channel,
            level_for(self.channel),
            String::from_utf8_lossy(bytes).into_owned(),
        );
        if self.bounds.admit(&mut record, Instant::now()) {
            self.filter.route(&self.router, record);
        }
    }
}

/// Level for a captured line: stdout INFO, stderr WARN.
fn level_for(channel: LogChannel) -> Level {
    match channel {
        LogChannel::Stderr => Level::WARN,
        _ => Level::INFO,
    }
}

impl AsyncWrite for LineWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.buf.extend_from_slice(data);
        self.drain();
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // A flush is not an end-of-line; partial lines stay buffered.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.flush_remainder();
        Poll::Ready(Ok(()))
    }
}

impl Drop for LineWriter {
    fn drop(&mut self) {
        // A store dropped on module death must not lose the final
        // unterminated line.
        self.flush_remainder();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::{NonZeroU32, NonZeroUsize};

    use nexum_runtime_config::{DispatchRatePolicy, LogBoundsPolicy, LogFilterPolicy};
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::logs::test_support::{CaptureStore, Console, run_id};
    use crate::logs::{LogChannel, LogPipeline};

    fn setup(channel: LogChannel) -> (LineWriter, Arc<CaptureStore>) {
        let store = Arc::new(CaptureStore::default());
        let bounds = SharedLogBounds::new(LogBoundsPolicy::default(), Instant::now());
        (writer(&store, &bounds, channel), store)
    }

    fn writer(
        store: &Arc<CaptureStore>,
        bounds: &SharedLogBounds,
        channel: LogChannel,
    ) -> LineWriter {
        writer_filtered(store, bounds, channel, LogFilterPolicy::default())
    }

    fn writer_filtered(
        store: &Arc<CaptureStore>,
        bounds: &SharedLogBounds,
        channel: LogChannel,
        filter: LogFilterPolicy,
    ) -> LineWriter {
        LineWriter {
            router: LogPipeline::new(store.clone()).router(),
            bounds: bounds.clone(),
            filter: SharedLogFilter::new(filter),
            run: run_id(),
            channel,
            buf: Vec::new(),
        }
    }

    fn policy(cap: usize, burst: u32) -> LogBoundsPolicy {
        LogBoundsPolicy {
            max_record_bytes: NonZeroUsize::new(cap).expect("non-zero cap"),
            // 1/s refills nothing over a flood of microseconds.
            rate: DispatchRatePolicy::new(
                NonZeroU32::new(burst).expect("non-zero burst"),
                NonZeroU32::new(1).expect("non-zero rate"),
            ),
        }
    }

    fn messages(store: &CaptureStore) -> Vec<String> {
        store.messages()
    }

    #[tokio::test]
    async fn splits_on_newlines() {
        let (mut w, store) = setup(LogChannel::Stdout);
        w.write_all(b"alpha\nbeta\n").await.unwrap();
        assert_eq!(messages(&store), ["alpha", "beta"]);
    }

    #[tokio::test]
    async fn buffers_a_partial_line_until_the_newline_arrives() {
        let (mut w, store) = setup(LogChannel::Stdout);
        w.write_all(b"partial").await.unwrap();
        assert!(messages(&store).is_empty(), "no newline yet");
        w.write_all(b" line\n").await.unwrap();
        assert_eq!(messages(&store), ["partial line"]);
    }

    #[tokio::test]
    async fn reassembles_a_utf8_code_point_split_across_writes() {
        // The euro sign is three bytes; splitting mid-code-point across
        // two writes must not corrupt the decoded line.
        let euro = "\u{20ac}".as_bytes();
        let (mut w, store) = setup(LogChannel::Stdout);
        w.write_all(&euro[..1]).await.unwrap();
        w.write_all(&euro[1..]).await.unwrap();
        w.write_all(b"\n").await.unwrap();
        assert_eq!(messages(&store), ["\u{20ac}"]);
    }

    #[tokio::test]
    async fn interleaved_writes_accumulate_into_one_line() {
        let (mut w, store) = setup(LogChannel::Stdout);
        for chunk in [&b"a"[..], b"b", b"c", b"\n", b"d", b"e", b"\n"] {
            w.write_all(chunk).await.unwrap();
        }
        assert_eq!(messages(&store), ["abc", "de"]);
    }

    #[tokio::test]
    async fn final_unterminated_line_is_flushed_on_drop() {
        let (mut w, store) = setup(LogChannel::Stdout);
        w.write_all(b"no trailing newline").await.unwrap();
        assert!(messages(&store).is_empty(), "buffered, not yet flushed");
        drop(w);
        assert_eq!(messages(&store), ["no trailing newline"]);
    }

    #[tokio::test]
    async fn empty_lines_are_skipped() {
        let (mut w, store) = setup(LogChannel::Stdout);
        w.write_all(b"\n\nkept\n\n").await.unwrap();
        assert_eq!(messages(&store), ["kept"]);
    }

    #[tokio::test]
    async fn trailing_carriage_return_is_trimmed() {
        let (mut w, store) = setup(LogChannel::Stdout);
        w.write_all(b"crlf\r\n").await.unwrap();
        assert_eq!(messages(&store), ["crlf"]);
    }

    #[tokio::test]
    async fn stderr_lines_carry_the_warn_level() {
        let (mut w, store) = setup(LogChannel::Stderr);
        w.write_all(b"oops\n").await.unwrap();
        let records = store.records.lock();
        assert_eq!(records[0].channel, LogChannel::Stderr);
        assert_eq!(records[0].level, Level::WARN);
    }

    #[tokio::test]
    async fn over_long_unterminated_line_is_force_flushed_and_cut_to_the_cap() {
        let cap = LogBoundsPolicy::default().max_record_bytes.get();
        let (mut w, store) = setup(LogChannel::Stdout);
        w.write_all(&vec![b'x'; cap + 1]).await.unwrap();
        // The force-flush bounds host memory without waiting for a newline,
        // and the same cap the host verbs pass bounds the record it makes.
        assert_eq!(messages(&store).len(), 1);
        assert_eq!(messages(&store)[0].len(), cap);
        assert!(messages(&store)[0].ends_with("...[truncated]"));
    }

    #[tokio::test]
    async fn a_line_flood_past_the_burst_is_dropped() {
        let store = Arc::new(CaptureStore::default());
        let bounds = SharedLogBounds::new(policy(4096, 2), Instant::now());
        let mut w = writer(&store, &bounds, LogChannel::Stdout);
        for i in 0..8 {
            w.write_all(format!("line {i}\n").as_bytes()).await.unwrap();
        }
        assert_eq!(
            messages(&store),
            ["line 0", "line 1"],
            "a size bound alone would have let all eight lines through",
        );
    }

    /// The channel level is all a captured line offers the filter, so a
    /// `warn` console floor silences stdout and keeps stderr.
    #[test]
    fn a_console_floor_silences_stdout_without_losing_the_line() {
        let store = Arc::new(CaptureStore::default());
        let bounds = SharedLogBounds::new(LogBoundsPolicy::default(), Instant::now());
        let quiet = LogFilterPolicy {
            console: Level::WARN,
            retain: Level::TRACE,
            targets: BTreeMap::new(),
        };
        let out = writer_filtered(&store, &bounds, LogChannel::Stdout, quiet.clone());
        let err = writer_filtered(&store, &bounds, LogChannel::Stderr, quiet);
        let printed = Console::printed(|| {
            out.route(b"println debugging");
            err.route(b"oops");
        });
        assert!(!printed.contains("println debugging"), "{printed}");
        assert!(printed.contains("oops"), "stderr clears warn: {printed}");
        assert_eq!(
            messages(&store),
            ["println debugging", "oops"],
            "`nexum logs` keeps the line the console never printed",
        );
    }

    #[tokio::test]
    async fn one_bucket_covers_both_pipes() {
        let store = Arc::new(CaptureStore::default());
        let bounds = SharedLogBounds::new(policy(4096, 2), Instant::now());
        let mut out = writer(&store, &bounds, LogChannel::Stdout);
        let mut err = writer(&store, &bounds, LogChannel::Stderr);
        out.write_all(b"a\nb\n").await.unwrap();
        err.write_all(b"c\n").await.unwrap();
        assert_eq!(
            messages(&store),
            ["a", "b"],
            "stderr found the burst already spent by stdout",
        );
    }
}
