//! `nexum:host/chain`: raw JSON-RPC dispatch.

use std::time::Instant;

use alloy_chains::Chain;

use crate::bindings::nexum;
use crate::bindings::nexum::host::chain::ChainError;
use crate::host::component::{ChainMethod, RuntimeTypes};
use crate::host::error::{batch_over_cap, method_denied, response_over_cap};
use crate::host::state::HostState;

/// Resolve a guest method string into the permitted read surface; an unknown
/// or mutating method is a `Denied` fault.
fn resolve_method(method: &str) -> Result<ChainMethod, ChainError> {
    ChainMethod::try_from(method).map_err(|_| method_denied())
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
        return Err(response_over_cap());
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
        let result = self.chain.request(chain, method, params).await;
        if let Err(err) = &result {
            // The one place the upstream error is recorded in full: the
            // guest fault about to be built carries only vocabulary text.
            tracing::warn!(chain_id, method = name, error = %err, "chain request failed");
        }
        let result = result.map_err(ChainError::from).and_then(|body| {
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

    // ── response size cap tests ──

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
