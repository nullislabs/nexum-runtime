//! The module-log pipeline for the Nexum runtime.

#![forbid(unsafe_code)]

mod builder;
mod logs;

pub use builder::LogPipelineBuilder;
pub use logs::{
    InMemoryRunLogStore, LogChannel, LogPage, LogPipeline, LogRecord, LogRouter, RunId,
    RunLogStore, RunMeta, StdioStream,
};
