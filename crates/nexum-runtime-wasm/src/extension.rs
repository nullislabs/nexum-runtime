//! The launch-side glue for the extension seam.

use std::sync::Arc;

use nexum_runtime_api::{Extension, RuntimeTypes, WasiClockOverride};

/// Hand every extension the effective wall clock. Every launch path calls
/// this before it builds the linker.
pub fn attach_wall_clock<T: RuntimeTypes>(
    extensions: &[Arc<dyn Extension<T>>],
    clocks: Option<&WasiClockOverride>,
) {
    let wall = WasiClockOverride::effective_wall(clocks);
    for ext in extensions {
        ext.attach_clock(wall.clone());
    }
}
