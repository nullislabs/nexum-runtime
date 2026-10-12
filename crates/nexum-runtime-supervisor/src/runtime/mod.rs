//! Engine-side runtime: the event loop that drives the supervisor from live
//! chain streams and schedule sources, and its pacing and restart policies.

pub mod event_loop;
pub mod restart_policy;
pub mod schedule;
