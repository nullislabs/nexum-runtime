//! `nexum:host/chain`: raw JSON-RPC dispatch.

use std::time::Instant;

use alloy_chains::Chain;

use crate::bindings::nexum;
use crate::bindings::nexum::host::chain::ChainError;
use crate::host::component::{ChainMethod, RuntimeTypes};
use crate::host::error::chain_denied;
use crate::host::state::HostState;

/// Resolve a guest method string into the permitted read surface; an unknown
/// or mutating method is a `Denied` fault.
fn resolve_method(method: &str) -> Result<ChainMethod, ChainError> {
    ChainMethod::try_from(method).map_err(|_| {
        chain_denied(format!(
            "method `{method}` is not in the permitted read-only surface"
        ))
    })
}

/// Error if `body` exceeds `cap` bytes, checked before the copy into the
/// guest.
fn check_response_cap(
    body: &str,
    cap: usize,
    chain_id: u64,
    method: &str,
) -> Result<(), ChainError> {
    if body.len() > cap {
        tracing::warn!(
            chain_id,
            method,
            body_bytes = body.len(),
            cap_bytes = cap,
            "chain response exceeds size cap - rejecting before guest copy"
        );
        metrics::counter!(
            "nexum_runtime_chain_response_capped_total",
            "chain_id" => chain_id.to_string(),
            "method" => method.to_owned(),
        )
        .increment(1);
        return Err(ChainError::Fault(
            crate::bindings::nexum::host::types::Fault::InvalidInput(format!(
                "chain response ({} bytes) exceeds the configured cap ({} bytes)",
                body.len(),
                cap,
            )),
        ));
    }
    Ok(())
}

impl<T: RuntimeTypes> nexum::host::chain::Host for HostState<T> {
    async fn request(
        &mut self,
        chain_id: u64,
        method: String,
        params: String,
    ) -> Result<String, ChainError> {
        let start = Instant::now();
        let chain = Chain::from_id(chain_id);
        let method = match resolve_method(&method) {
            Ok(method) => method,
            Err(err) => {
                tracing::warn!(
                    chain_id,
                    %method,
                    "chain::request rejected: method is not in the permitted read surface"
                );
                metrics::counter!(
                    "nexum_runtime_chain_request_total",
                    "chain_id" => chain_id.to_string(),
                    "method" => "<denied>",
                    "outcome" => "err",
                )
                .increment(1);
                return Err(err);
            }
        };
        let name = method.as_str();
        tracing::debug!(chain_id, method = name, "chain::request");
        let result = self
            .chain
            .request(chain, method, params)
            .await
            .map_err(ChainError::from)
            .and_then(|body| {
                check_response_cap(&body, self.chain_response_max_bytes, chain_id, name)?;
                Ok(body)
            });
        tracing::trace!(elapsed_ms = ?start.elapsed(), "chain::request done");
        let outcome = if result.is_ok() { "ok" } else { "err" };
        metrics::counter!(
            "nexum_runtime_chain_request_total",
            "chain_id" => chain_id.to_string(),
            "method" => name,
            "outcome" => outcome,
        )
        .increment(1);
        result
    }

    /// Dispatch a batch, one `RpcResult` per entry in order. Per-entry
    /// failures are independent; the outer `ChainError` is never returned.
    async fn request_batch(
        &mut self,
        chain_id: u64,
        requests: Vec<nexum::host::chain::RpcRequest>,
    ) -> Result<Vec<nexum::host::chain::RpcResult>, ChainError> {
        let start = Instant::now();
        // Each entry is dispatched sequentially and gets its own full
        // per-chain timeout, so the worst-case blocking time for a batch
        // is N x request_timeout_secs.
        tracing::debug!(chain_id, count = requests.len(), "chain::request-batch");
        let cap = self.chain_response_max_bytes;
        let mut out = Vec::with_capacity(requests.len());
        // The per-entry cap (inside `request`) bounds each body; this
        // running total bounds the aggregate `Vec<RpcResult>` lowered into
        // guest memory in one go, so a wide batch of individually-legal
        // bodies cannot saturate the guest heap either - the exact failure
        // the guidance in #154 (block-range chunking via request-batch)
        // would otherwise re-introduce.
        let mut total_bytes: usize = 0;
        for req in requests {
            let method = req.method.clone();
            match nexum::host::chain::Host::request(self, chain_id, req.method, req.params).await {
                Ok(s) => {
                    total_bytes = total_bytes.saturating_add(s.len());
                    if total_bytes > cap {
                        tracing::warn!(
                            chain_id,
                            method = %method,
                            total_bytes,
                            cap_bytes = cap,
                            "chain batch aggregate exceeds size cap - rejecting entry before guest copy"
                        );
                        metrics::counter!(
                            "nexum_runtime_chain_response_capped_total",
                            "chain_id" => chain_id.to_string(),
                            "method" => method,
                        )
                        .increment(1);
                        out.push(nexum::host::chain::RpcResult::Err(ChainError::Fault(
                            crate::bindings::nexum::host::types::Fault::InvalidInput(format!(
                                "batch aggregate ({total_bytes} bytes) exceeds the configured \
                                 cap ({cap} bytes)",
                            )),
                        )));
                    } else {
                        out.push(nexum::host::chain::RpcResult::Ok(s));
                    }
                }
                Err(e) => out.push(nexum::host::chain::RpcResult::Err(e)),
            }
        }
        tracing::trace!(elapsed_ms = ?start.elapsed(), "chain::request-batch done");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::bindings::nexum::host::types::Fault;

    #[test]
    fn permitted_methods_resolve() {
        for m in ["eth_call", "eth_blockNumber", "eth_getBalance"] {
            assert!(resolve_method(m).is_ok(), "{m} should resolve");
        }
    }

    #[test]
    fn signing_methods_are_denied() {
        // The signing-adjacent surface must map to a `Denied` fault,
        // not reach the provider.
        for m in [
            "eth_sign",
            "eth_sendTransaction",
            "eth_accounts",
            "personal_sign",
            "eth_sendRawTransaction",
        ] {
            let err = resolve_method(m).expect_err(m);
            assert!(
                matches!(err, ChainError::Fault(Fault::Denied(_))),
                "{m} must be a Denied fault, got {err:?}"
            );
        }
    }

    #[test]
    fn unknown_method_is_denied() {
        let err = resolve_method("eth_totallyFakeMethod").expect_err("unknown method");
        assert!(matches!(err, ChainError::Fault(Fault::Denied(_))));
    }

    #[test]
    fn batch_entries_are_classified_independently() {
        // `request_batch` routes every entry through `resolve_method`,
        // so one denied entry neither aborts nor taints the permitted
        // entries around it.
        let batch = ["eth_call", "eth_sign", "eth_getBalance"];
        let resolved: Vec<_> = batch.iter().map(|m| resolve_method(m)).collect();
        assert!(resolved[0].is_ok());
        assert!(matches!(
            resolved[1].as_ref().expect_err("eth_sign"),
            ChainError::Fault(Fault::Denied(_)),
        ));
        assert!(resolved[2].is_ok());
    }

    // ── response size cap tests (#154) ──

    #[test]
    fn response_at_cap_is_accepted() {
        let body = "x".repeat(10);
        assert!(
            check_response_cap(&body, 10, 1, "eth_call").is_ok(),
            "body exactly at cap should pass"
        );
    }

    #[test]
    fn response_over_cap_returns_invalid_input() {
        let body = "x".repeat(11);
        let err =
            check_response_cap(&body, 10, 1, "eth_call").expect_err("over-cap body should fail");
        assert!(
            matches!(err, ChainError::Fault(Fault::InvalidInput(_))),
            "expected InvalidInput fault, got {err:?}"
        );
    }
}
