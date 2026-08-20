//! The module-log pipeline for the Nexum runtime.

#![forbid(unsafe_code)]

mod builder;
mod logs;

pub use builder::LogPipelineBuilder;
pub use logs::{
    InMemoryRunLogStore, LogChannel, LogField, LogPage, LogPipeline, LogRecord, LogRouter,
    LogSource, LogValue, RunId, RunLogStore, RunMeta, StdioStream,
};
