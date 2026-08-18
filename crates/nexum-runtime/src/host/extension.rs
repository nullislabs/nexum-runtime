//! The extension seam.

use std::sync::Arc;

pub use nexum_runtime_api::{
    Extension, ExtensionDelivery, ExtensionError, ExtensionSource, HostWallClock, SourceContext,
};

use crate::host::component::RuntimeTypes;
use crate::supervisor::WasiClockOverride;

/// Hand every extension the effective wall clock. Every launch path calls
/// this before it builds the linker.
pub(crate) fn attach_wall_clock<T: RuntimeTypes>(
    extensions: &[Arc<dyn Extension<T>>],
    clocks: Option<&WasiClockOverride>,
) {
    let wall = WasiClockOverride::effective_wall(clocks);
    for ext in extensions {
        ext.attach_clock(wall.clone());
    }
}
