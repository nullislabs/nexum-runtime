//! Guest-visible clock override.

use std::sync::Arc;

use wasmtime_wasi::{HostMonotonicClock, HostWallClock};

/// Shared sources let a test drive guest-visible time and the wall clock
/// extensions receive; `None` keeps the ambient clocks. `RunId.started_at`
/// is host wall-clock and unaffected.
#[derive(Clone)]
pub struct WasiClockOverride {
    wall: Arc<dyn HostWallClock + Send + Sync>,
    monotonic: Arc<dyn HostMonotonicClock + Send + Sync>,
}

impl WasiClockOverride {
    /// Pair the two clocks a guest can observe. Both are replaced
    /// together: a test that moves one and not the other is worse than
    /// the ambient pair.
    pub fn new(
        wall: Arc<dyn HostWallClock + Send + Sync>,
        monotonic: Arc<dyn HostMonotonicClock + Send + Sync>,
    ) -> Self {
        Self { wall, monotonic }
    }

    /// The wall clock guests and extensions observe.
    pub fn wall(&self) -> Arc<dyn HostWallClock + Send + Sync> {
        self.wall.clone()
    }

    /// The monotonic clock guests observe.
    pub fn monotonic(&self) -> Arc<dyn HostMonotonicClock + Send + Sync> {
        self.monotonic.clone()
    }

    /// The effective host wall clock: the override's wall clock when set,
    /// else the real host clock.
    pub fn effective_wall(clocks: Option<&Self>) -> Arc<dyn HostWallClock + Send + Sync> {
        match clocks {
            Some(clocks) => clocks.wall.clone(),
            None => Arc::new(wasmtime_wasi::clocks::WallClock::default()),
        }
    }
}
