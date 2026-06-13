//! Engine-side runtime: per-module resource limits and the event loop
//! that drives the supervisor from live chain subscriptions.

pub mod event_loop;
pub mod limits;
