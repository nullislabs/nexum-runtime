//! `nexum:host/chain`: raw JSON-RPC dispatch over alloy.

use std::time::Instant;

use crate::bindings::HostError;
use crate::bindings::nexum;
use crate::bindings::nexum::host::types::HostErrorKind;
use crate::host::error::internal_error;
use crate::host::provider_pool::ProviderError;
use crate::host::state::HostState;

/// Methods that could sign transactions or expose sensitive node
/// internals. We warn when a module calls one so operators can audit.
const DANGEROUS_METHODS: &[&str] = &[
    "eth_sign",
    "eth_signTransaction",
    "eth_sendTransaction",
    "personal_sign",
    "personal_unlockAccount",
    "personal_sendTransaction",
];

/// Prefixes whose entire namespace is considered dangerous.
const DANGEROUS_PREFIXES: &[&str] = &["admin_", "debug_", "miner_"];

fn is_dangerous_method(method: &str) -> bool {
    DANGEROUS_METHODS.contains(&method) || DANGEROUS_PREFIXES.iter().any(|p| method.starts_with(p))
}

impl nexum::host::chain::Host for HostState {
    async fn request(
        &mut self,
        chain_id: u64,
        method: String,
        params: String,
    ) -> Result<String, HostError> {
        let start = Instant::now();
        if is_dangerous_method(&method) {
            tracing::warn!(
                chain_id,
                %method,
                "module called a dangerous RPC method — ensure your RPC \
                 endpoint is read-only or this call is intentional"
            );
        }
        tracing::debug!(chain_id, %method, "chain::request");
        let method_label = method.clone();
        let result = match self.chain.request(chain_id, method, params).await {
            Ok(body) => Ok(body),
            Err(ProviderError::UnknownChain(id)) => Err(HostError {
                domain: "chain".into(),
                kind: HostErrorKind::Unsupported,
                code: 0,
                message: format!("chain {id} has no engine.toml RPC entry"),
                data: None,
            }),
            Err(err @ ProviderError::InvalidParams { .. }) => Err(HostError {
                domain: "chain".into(),
                kind: HostErrorKind::InvalidInput,
                code: -32602,
                message: err.to_string(),
                data: None,
            }),
            Err(err @ ProviderError::Rpc { .. }) => Err(HostError {
                domain: "chain".into(),
                kind: HostErrorKind::Internal,
                code: -32603,
                message: err.to_string(),
                data: None,
            }),
            Err(err) => Err(internal_error("chain", err.to_string())),
        };
        tracing::trace!(elapsed_ms = ?start.elapsed(), "chain::request done");
        let outcome = if result.is_ok() { "ok" } else { "err" };
        metrics::counter!(
            "shepherd_chain_request_total",
            "chain_id" => chain_id.to_string(),
            "method" => method_label,
            "outcome" => outcome,
        )
        .increment(1);
        result
    }

    async fn request_batch(
        &mut self,
        chain_id: u64,
        requests: Vec<nexum::host::chain::RpcRequest>,
    ) -> Result<Vec<nexum::host::chain::RpcResult>, HostError> {
        let start = Instant::now();
        tracing::debug!(chain_id, count = requests.len(), "chain::request-batch");
        let mut out = Vec::with_capacity(requests.len());
        for req in requests {
            match nexum::host::chain::Host::request(self, chain_id, req.method, req.params).await {
                Ok(s) => out.push(nexum::host::chain::RpcResult::Ok(s)),
                Err(e) => out.push(nexum::host::chain::RpcResult::Err(e)),
            }
        }
        tracing::trace!(elapsed_ms = ?start.elapsed(), "chain::request-batch done");
        Ok(out)
    }
}
