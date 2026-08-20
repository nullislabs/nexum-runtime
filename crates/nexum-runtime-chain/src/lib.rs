//! The JSON-RPC chain backend for the Nexum runtime.

#![forbid(unsafe_code)]

mod builder;
mod provider_pool;

pub use builder::ProviderPoolBuilder;
pub use provider_pool::{
    BlockStream, CanonicalLogBatch, CanonicalLogStream, MAX_REORG_DEPTH, PoolError, ProviderPool,
};
