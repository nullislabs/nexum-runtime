//! The module-log pipeline for the Nexum runtime.

#![forbid(unsafe_code)]

mod builder;
#[cfg(feature = "testing")]
mod capture;
mod logs;

pub use builder::LogPipelineBuilder;
#[cfg(feature = "testing")]
pub use capture::{LogCapture, capture_logs};
pub use logs::{
    InMemoryRunLogStore, LogChannel, LogField, LogPage, LogPipeline, LogRecord, LogRouter,
    LogSource, LogValue, RunId, RunLogStore, RunMeta, SharedLogBounds, SharedLogFilter,
    StdioStream,
};
