//! The [`HostState`] side of the outbound gate in `nexum-runtime-http`.

use wasmtime_wasi_http::p2::{WasiHttpCtxView, WasiHttpView};

use nexum_runtime_api::RuntimeTypes;

use crate::state::HostState;

impl<T: RuntimeTypes> WasiHttpView for HostState<T> {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http_ctx,
            table: &mut self.table,
            hooks: &mut self.http_gate,
        }
    }
}
