//! Engine-side runtime: the event loop that drives the supervisor from live
//! chain streams, and its pacing and restart policies.

pub mod dispatch_rate;
pub mod event_loop;
pub mod restart_policy;
