//! `nexum:host/chain`: raw JSON-RPC dispatch.

use std::time::Instant;

use alloy_chains::Chain;

use nexum_runtime_api::RuntimeTypes;
use nexum_runtime_api::bindings::nexum;
use nexum_runtime_api::bindings::nexum::host::chain::ChainError;
use nexum_runtime_chain::{PoolError, ProviderPool};
use nexum_world::ChainMethod;

use crate::error::{method_denied, pool_fault, response_over_cap};
use crate::state::HostState;

/// Resolve a guest method string into the permitted read surface; an unknown
/// or mutating method is a `Denied` fault.
fn resolve_method(method: &str) -> Result<ChainMethod, ChainError> {
    ChainMethod::try_from(method).map_err(|_| method_denied())
}

/// The guest picks the chain, so one outside the pool counts under a
/// sentinel rather than minting a series.
fn count_request(pool: &ProviderPool, chain: Chain, method: &'static str, outcome: &'static str) {
    let chain_id = if pool.provider(chain).is_ok() {
        chain.id().to_string()
    } else {
        "unconfigured".to_owned()
    };
    metrics::counter!(
        "nexum_runtime_chain_request_total",
        "chain_id" => chain_id,
        "method" => method,
        "outcome" => outcome,
    )
    .increment(1);
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
                count_request(&self.chain, chain, "<denied>", "err");
                return Err(err);
            }
        };
        let name = method.as_str();
        tracing::debug!(chain_id, method = name, "chain::request");
        let result = self.chain.request(chain, method, params).await;
        if let Err(err) = &result {
            // The one place the upstream error is recorded in full: the
            // guest fault about to be built carries only vocabulary text.
            // Below WARN: a reverting poll would otherwise flood the
            // operator log with node-controlled text.
            if matches!(err, PoolError::Rpc(e) if e.as_error_resp().is_some()) {
                tracing::debug!(
                    module = %self.run.module,
                    chain_id,
                    method = name,
                    error = %err,
                    "chain request returned an error response"
                );
            } else {
                tracing::warn!(
                    module = %self.run.module,
                    chain_id,
                    method = name,
                    error = %err,
                    "chain request failed"
                );
            }
        }
        let result = result.map_err(pool_fault).and_then(|body| {
            check_response_cap(&body, self.chain_response_max_bytes, chain_id, name)?;
            Ok(body)
        });
        tracing::trace!(elapsed_ms = ?start.elapsed(), "chain::request done");
        let outcome = if result.is_ok() { "ok" } else { "err" };
        count_request(&self.chain, chain, name, outcome);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nexum_runtime_api::bindings::nexum::host::types::Fault;

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

    fn request_labels(f: impl FnOnce()) -> Vec<Vec<(String, String)>> {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, f);
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(key, ..)| key.key().name() == "nexum_runtime_chain_request_total")
            .map(|(key, ..)| {
                key.key()
                    .labels()
                    .map(|l| (l.key().to_owned(), l.value().to_owned()))
                    .collect()
            })
            .collect()
    }

    fn assert_labels(labels: &[Vec<(String, String)>], expected: &[(&str, &str)]) {
        assert_eq!(labels.len(), 1, "one sample expected: {labels:?}");
        for (key, value) in expected {
            assert!(
                labels[0].iter().any(|(k, v)| k == key && v == value),
                "expected {key}={value} in {:?}",
                labels[0],
            );
        }
    }

    /// The endpoint is never contacted.
    async fn pool_with_chain_one() -> nexum_runtime_chain::ProviderPool {
        use nexum_runtime_config::{ChainConfig, EngineConfig, RpcEndpoint};
        let mut cfg = EngineConfig::default();
        cfg.chains.insert(
            Chain::from_id(1),
            ChainConfig {
                rpc_url: RpcEndpoint::try_from("http://127.0.0.1:1").expect("test rpc url parses"),
                request_timeout_secs: 1,
                max_log_range_blocks: 1000,
            },
        );
        ProviderPool::from_config(&cfg).await.expect("pool opens")
    }

    #[test]
    fn unconfigured_chain_counts_under_the_sentinel() {
        let pool = ProviderPool::empty();
        let labels =
            request_labels(|| count_request(&pool, Chain::from_id(999), "eth_call", "err"));
        assert_labels(
            &labels,
            &[
                ("chain_id", "unconfigured"),
                ("method", "eth_call"),
                ("outcome", "err"),
            ],
        );
    }

    #[tokio::test]
    async fn configured_chain_keeps_its_id() {
        let pool = pool_with_chain_one().await;
        let labels = request_labels(|| count_request(&pool, Chain::from_id(1), "eth_call", "ok"));
        assert_labels(
            &labels,
            &[("chain_id", "1"), ("method", "eth_call"), ("outcome", "ok")],
        );
    }
}
