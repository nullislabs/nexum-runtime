//! Per-module host state, held in the wasmtime `Store` and the receiver for
//! every `Host` impl in this crate.

use std::sync::Arc;

use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;

use nexum_runtime_api::{Handle, RuntimeTypes};
use nexum_runtime_chain::ProviderPool;
use nexum_runtime_http::HttpGate;
use nexum_runtime_logs::{LogRouter, RunId, SharedLogBounds, SharedLogFilter};

/// Per-module host state, generic over the [`RuntimeTypes`] lattice.
pub struct HostState<T: RuntimeTypes> {
    /// WASI context. Deliberately built with no environment, no
    /// arguments, and no preopened directory.
    pub wasi: WasiCtx,
    /// Resource table backing the WASI and wasi:http handles.
    pub table: ResourceTable,
    /// Wasmtime memory/table/instance resource limits for this store, and
    /// the memory reading taken on the way past them.
    pub limits: crate::ObservedLimits,
    /// Per-store wasi:http context.
    pub http_ctx: WasiHttpCtx,
    /// Per-module allowlist gate every wasi:http outgoing request
    /// passes through.
    pub http_gate: HttpGate,
    /// Content topics this store may publish to; empty is unscoped. An
    /// out-of-scope publish is refused before the backend.
    /// Identity of this store's run; tags every captured log record.
    pub run: RunId,
    /// Shared log pipeline the `nexum:host/logging` glue routes through.
    pub log_router: Arc<LogRouter>,
    /// Per-run admission gate every host logging call passes before the
    /// router renders it, shared with this store's stdio capture point.
    pub log_bounds: SharedLogBounds,
    /// Per-run operator filter, shared with this store's stdio capture point.
    pub log_filter: SharedLogFilter,
    /// `chain` backend: per-chain provider pool.
    pub chain: ProviderPool,
    /// Cap on a chain JSON-RPC response body; larger responses are rejected.
    pub chain_response_max_bytes: usize,
    /// `local-store` backend: per-module handle with keccak256 prefix.
    pub store: Handle<T>,
}

// `WasiView: Send`, so the backends must be `Send` too; the lattice
// supertraits already guarantee it.
impl<T: RuntimeTypes> WasiView for HostState<T> {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}
