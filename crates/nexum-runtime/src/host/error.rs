//! Constructors and `From` conversions building the WIT error shapes
//! (`chain-error`, `Fault`); `fault_label` / `fault_message` project a
//! `Fault` into metric and log fields.

use alloy_primitives::Bytes;
use alloy_transport::TransportError;

use crate::bindings::nexum::host::chain::{ChainError, RpcError};
use crate::bindings::nexum::host::types::{Fault, RateLimit};
use crate::host::local_store_redb::StorageError;
use crate::host::provider_pool::PoolError;

/// `Denied` chain fault for a request the host policy refused.
pub(crate) fn chain_denied(detail: impl Into<String>) -> ChainError {
    ChainError::Fault(Fault::Denied(detail.into()))
}

/// Stable snake_case label for a [`Fault`], for metric and log `kind` fields.
pub fn fault_label(fault: &Fault) -> &'static str {
    use nexum_world::FaultLabel as Label;
    match fault {
        Fault::Unsupported(_) => Label::Unsupported,
        Fault::Unavailable(_) => Label::Unavailable,
        Fault::Denied(_) => Label::Denied,
        Fault::RateLimited(_) => Label::RateLimited,
        Fault::Timeout => Label::Timeout,
        Fault::InvalidInput(_) => Label::InvalidInput,
        Fault::Internal(_) => Label::Internal,
    }
    .into()
}

/// Human-readable detail carried by a [`Fault`], for the log `message` field.
pub fn fault_message(fault: &Fault) -> std::borrow::Cow<'_, str> {
    match fault {
        Fault::Unsupported(m)
        | Fault::Unavailable(m)
        | Fault::Denied(m)
        | Fault::InvalidInput(m)
        | Fault::Internal(m) => std::borrow::Cow::Borrowed(m),
        Fault::RateLimited(rl) => match rl.retry_after_ms {
            Some(ms) => std::borrow::Cow::Owned(format!("rate limited, retry after {ms} ms")),
            None => std::borrow::Cow::Borrowed("rate limited"),
        },
        Fault::Timeout => std::borrow::Cow::Borrowed("timeout"),
    }
}

/// Project a [`PoolError`] into `chain-error`: a structured JSON-RPC
/// `ErrorResp` becomes [`ChainError::Rpc`] with its code and revert bytes,
/// everything else a shared [`Fault`].
impl From<PoolError> for ChainError {
    fn from(err: PoolError) -> Self {
        match err {
            PoolError::UnknownChain(id) => ChainError::Fault(Fault::Unsupported(format!(
                "chain {id} has no engine.toml RPC entry"
            ))),
            // The configured per-request timeout elapsed. The dedicated
            // timeout fault lets a guest tell a slow node apart from a
            // revert or an unreachable endpoint.
            PoolError::Timeout => ChainError::Fault(Fault::Timeout),
            PoolError::Rpc(source) => classify_rpc(&source),
        }
    }
}

/// Classify an alloy RPC failure: a structured `ErrorResp` keeps its code and
/// decoded revert bytes, a malformed request is `invalid-input`, everything
/// else a transport [`Fault`].
fn classify_rpc(source: &TransportError) -> ChainError {
    // A structured JSON-RPC error response (`{"error": {"code":...,
    // "data":...}}`) - typically an `eth_call` revert - keeps the node's
    // code and the hex `error.data` decoded into the abi-encoded revert
    // body, so a guest can classify the outcome via `decode_revert`.
    if let Some(payload) = source.as_error_resp() {
        return ChainError::Rpc(RpcError {
            // Preserve the node-reported JSON-RPC code. A code outside
            // `i32` is a JSON-RPC spec violation, clamped to `-32603`
            // Internal error.
            code: i32::try_from(payload.code).unwrap_or(-32603),
            message: source.to_string(),
            // alloy decodes the hex `error.data` JSON string into `Bytes`
            // in one step; the guest binding is `Vec<u8>`, so land it
            // there once. Non-hex or structured data decodes to `None`.
            data: payload
                .try_data_as::<Bytes>()
                .and_then(Result::ok)
                .map(|b| b.to_vec()),
        });
    }
    match source {
        // The request body was malformed before it reached the node;
        // alloy's own retry layer treats this variant as terminal.
        alloy_transport::RpcError::SerError(err) => {
            ChainError::Fault(Fault::InvalidInput(err.to_string()))
        }
        _ => ChainError::Fault(transport_fault(source)),
    }
}

/// Classify a transport RPC failure: 429 to `rate-limited`, 503 or a dropped
/// backend to `unavailable`, a timeout to `timeout`, else `unavailable`.
fn transport_fault(source: &TransportError) -> Fault {
    use alloy_transport::TransportErrorKind;
    if let Some(kind) = source.as_transport_err() {
        match kind {
            TransportErrorKind::HttpError(http) if http.status == 429 => {
                return Fault::RateLimited(RateLimit {
                    retry_after_ms: None,
                });
            }
            TransportErrorKind::HttpError(http) if http.status == 503 => {
                return Fault::Unavailable(source.to_string());
            }
            TransportErrorKind::BackendGone | TransportErrorKind::PubsubUnavailable => {
                return Fault::Unavailable(source.to_string());
            }
            _ => {}
        }
    }
    // Typed probe first: a client-level timeout hides in the source chain
    // (reqwest's `Display` omits its source), so the string sniff below
    // cannot see it.
    if timeout_in_source_chain(source) {
        return Fault::Timeout;
    }
    // Last resort for transports that only surface a timeout in the message.
    let msg = source.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        Fault::Timeout
    } else {
        Fault::Unavailable(msg)
    }
}

/// Walk the `source()` chain for a typed timeout: a timed-out
/// [`reqwest::Error`] or an [`std::io::Error`] of kind `TimedOut`.
fn timeout_in_source_chain(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cursor = Some(err);
    while let Some(e) = cursor {
        if e.downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout)
            || e.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::TimedOut)
        {
            return true;
        }
        cursor = e.source();
    }
    false
}

/// Project a [`StorageError`]: quota breach to `denied`, a per-batch cap to
/// `invalid-input`, else `internal`.
impl From<StorageError> for Fault {
    fn from(err: StorageError) -> Self {
        match err {
            StorageError::QuotaExceeded { .. } => Fault::Denied(err.to_string()),
            StorageError::ApplyOpsExceeded { .. } | StorageError::ApplyBytesExceeded { .. } => {
                Fault::InvalidInput(err.to_string())
            }
            _ => Fault::Internal(err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use alloy_chains::Chain;
    use alloy_json_rpc::ErrorPayload;
    use alloy_transport::{RpcError as AlloyRpcError, TransportErrorKind};

    /// Build a synthetic transport-level [`TransportError`].
    fn transport_err(msg: &str) -> TransportError {
        TransportErrorKind::custom_str(msg)
    }

    #[test]
    fn unknown_chain_is_unsupported_fault() {
        // Use an id with no `NamedChain` mapping so `Chain`'s `Display`
        // prints the number and the message assertion stays meaningful.
        let chain_err = ChainError::from(PoolError::UnknownChain(Chain::from_id(424242)));
        let ChainError::Fault(Fault::Unsupported(msg)) = chain_err else {
            panic!("expected Unsupported fault, got {chain_err:?}");
        };
        assert!(msg.contains("424242"));
    }

    #[test]
    fn timeout_maps_to_timeout_fault() {
        // The tokio-elapsed leg surfaces as the dedicated `timeout` fault,
        // distinct from a revert (`Rpc`) or an unreachable node.
        let chain_err = ChainError::from(PoolError::Timeout);
        assert!(matches!(chain_err, ChainError::Fault(Fault::Timeout)));
    }

    #[tokio::test]
    async fn reqwest_client_timeout_maps_to_timeout_fault() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};

        // A real reqwest timeout: its `Display` never mentions "timeout",
        // so only the typed source-chain probe can classify it.
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
            .mount(&server)
            .await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .expect("client builds");
        let err = client
            .get(server.uri())
            .send()
            .await
            .expect_err("the request must time out");
        assert!(err.is_timeout(), "precondition: a reqwest timeout error");

        let chain_err = ChainError::from(PoolError::Rpc(TransportErrorKind::custom(err)));
        assert!(matches!(chain_err, ChainError::Fault(Fault::Timeout)));
    }

    #[test]
    fn message_only_timeout_maps_to_timeout_fault() {
        // The retained last-resort sniff: no typed timeout anywhere in the
        // chain, only the message marks it.
        let chain_err =
            ChainError::from(PoolError::Rpc(transport_err("request timed out after 30s")));
        assert!(matches!(chain_err, ChainError::Fault(Fault::Timeout)));
    }

    #[test]
    fn transport_failure_maps_to_unavailable_fault() {
        // A transport-level failure with no timeout marker defaults to an
        // `unavailable` fault.
        let chain_err = ChainError::from(PoolError::Rpc(transport_err("websocket disconnected")));
        assert!(matches!(
            chain_err,
            ChainError::Fault(Fault::Unavailable(_))
        ));
    }

    #[test]
    fn backend_gone_maps_to_unavailable_fault() {
        let chain_err = ChainError::from(PoolError::Rpc(TransportErrorKind::backend_gone()));
        assert!(matches!(
            chain_err,
            ChainError::Fault(Fault::Unavailable(_))
        ));
    }

    #[test]
    fn error_resp_forwards_code_and_decoded_revert_bytes() {
        // An `eth_call` revert shape: the hex `error.data` string lands in
        // the guest as decoded abi-encoded revert bytes.
        let payload: ErrorPayload = serde_json::from_str(
            r#"{"code":-32000,"message":"execution reverted","data":"0x08c379a0deadbeef"}"#,
        )
        .expect("payload parses");
        let chain_err = ChainError::from(PoolError::Rpc(AlloyRpcError::ErrorResp(payload)));
        let ChainError::Rpc(rpc) = chain_err else {
            panic!("expected ChainError::Rpc, got {chain_err:?}");
        };
        assert_eq!(rpc.code, -32000);
        assert_eq!(
            rpc.data,
            Some(vec![0x08, 0xc3, 0x79, 0xa0, 0xde, 0xad, 0xbe, 0xef]),
        );
        assert!(rpc.message.contains("execution reverted"));
    }

    #[test]
    fn error_resp_swallows_undecodable_data_to_none() {
        // Structured or non-hex `error.data` fails the `Bytes` decode and
        // is treated the same as "no revert body".
        for data in [r#"{"reason":"x"}"#, r#""not hex""#] {
            let payload: ErrorPayload = serde_json::from_str(&format!(
                r#"{{"code":-32000,"message":"boom","data":{data}}}"#
            ))
            .expect("payload parses");
            let chain_err = ChainError::from(PoolError::Rpc(AlloyRpcError::ErrorResp(payload)));
            let ChainError::Rpc(rpc) = chain_err else {
                panic!("expected ChainError::Rpc, got {chain_err:?}");
            };
            assert_eq!(rpc.data, None);
        }
    }

    #[test]
    fn out_of_range_rpc_code_saturates_to_internal_fallback() {
        // JSON-RPC codes are conventionally `-32768..-32000`, but the
        // alloy `ErrorPayload.code` field is `i64`. Defensive: an
        // out-of-`i32` code should not poison the projection - clamp
        // to `-32603` so the guest sees a sane code.
        let payload: ErrorPayload =
            serde_json::from_str(&format!(r#"{{"code":{},"message":"weird"}}"#, i64::MAX))
                .expect("payload parses");
        let chain_err = ChainError::from(PoolError::Rpc(AlloyRpcError::ErrorResp(payload)));
        let ChainError::Rpc(rpc) = chain_err else {
            panic!("expected ChainError::Rpc, got {chain_err:?}");
        };
        assert_eq!(rpc.code, -32603);
    }

    #[test]
    fn ser_error_maps_to_invalid_input_fault() {
        let source = serde_json::from_str::<serde_json::Value>("not json")
            .expect_err("`not json` is not valid JSON");
        let chain_err = ChainError::from(PoolError::Rpc(AlloyRpcError::SerError(source)));
        assert!(matches!(
            chain_err,
            ChainError::Fault(Fault::InvalidInput(_))
        ));
    }
}
