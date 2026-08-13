//! Constructors and `From` conversions building the WIT error shapes
//! (`chain-error`, `Fault`); `fault_label` / `fault_message` project a
//! `Fault` into metric and log fields.

use alloy_primitives::Bytes;
use alloy_transport::TransportError;

use crate::bindings::nexum::host::chain::{ChainError, RpcError};
use crate::bindings::nexum::host::types::{Fault, RateLimit};
use crate::engine_config::redact_urls_in_text;
use crate::host::local_store_redb::StorageError;
use crate::host::provider_pool::PoolError;

/// Render a [`TransportError`] for a guest-visible fault. The text can embed
/// the RPC endpoint (reqwest appends `for url (<url>)`), and the endpoint
/// carries operator credentials by design, so every URL is dropped for a
/// placeholder before the string crosses the WIT boundary to the untrusted
/// module (ADR-0001). Partial redaction is not enough: a provider key can
/// sit in the subdomain or a short path segment, which `redact_url` keeps.
/// A body that echoes a credential without URL shape stays beyond any
/// URL-level rule. Host-side logs render the error directly and keep the
/// full text.
fn guest_text(source: &TransportError) -> String {
    redact_urls_in_text(&source.to_string())
}

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
    // A structured error response (typically an `eth_call` revert) keeps the
    // node's code and revert body so a guest can classify via `decode_revert`.
    if let Some(payload) = source.as_error_resp() {
        return ChainError::Rpc(RpcError {
            // A code outside `i32` is a JSON-RPC spec violation, clamped
            // to `-32603` Internal error.
            code: i32::try_from(payload.code).unwrap_or(-32603),
            message: guest_text(source),
            // alloy decodes the hex `error.data` into `Bytes`; non-hex or
            // structured data decodes to `None`.
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
                return Fault::Unavailable(guest_text(source));
            }
            TransportErrorKind::BackendGone | TransportErrorKind::PubsubUnavailable => {
                return Fault::Unavailable(guest_text(source));
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
    // The sniff runs on the unredacted text so redaction cannot change the
    // classification; only the guest-visible string is redacted.
    let msg = source.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        Fault::Timeout
    } else {
        Fault::Unavailable(redact_urls_in_text(&msg))
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

    fn transport_err(msg: &str) -> TransportError {
        TransportErrorKind::custom_str(msg)
    }

    /// A credentialed endpoint in the shapes `${VAR}` interpolation produces:
    /// userinfo, an API key path segment, and a query credential. reqwest
    /// strips userinfo from the URL it renders, so the userinfo leg only
    /// matters for text that echoes the URL as configured.
    const CREDENTIALED_URL: &str =
        "http://user:passsecret@127.0.0.1:1/v2/THISISALONGAPIKEY1234567890?apikey=qsecret";

    /// Assert neither a credential nor the endpoint authority survives in a
    /// guest-visible payload.
    fn assert_no_endpoint(payload: &str) {
        for secret in [
            "passsecret",
            "THISISALONGAPIKEY1234567890",
            "qsecret",
            "127.0.0.1",
        ] {
            assert!(
                !payload.contains(secret),
                "`{secret}` leaked into the fault payload: {payload}"
            );
        }
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

    #[tokio::test]
    async fn unreachable_endpoint_fault_redacts_the_credentialed_url() {
        // A real reqwest send failure renders `error sending request for url
        // (<url>)` with the key in the clear; the guest fault must not.
        let err = reqwest::Client::new()
            .post(CREDENTIALED_URL)
            .send()
            .await
            .expect_err("port 1 refuses the connection");
        let chain_err = ChainError::from(PoolError::Rpc(TransportErrorKind::custom(err)));
        let ChainError::Fault(Fault::Unavailable(msg)) = chain_err else {
            panic!("expected Unavailable fault, got {chain_err:?}");
        };
        assert_no_endpoint(&msg);
        assert!(
            msg.contains("error sending request"),
            "diagnosis kept: {msg}"
        );
    }

    #[test]
    fn http_503_fault_redacts_an_echoed_endpoint() {
        // A proxy 503 body can echo the request URL; the guest fault keeps
        // the status but not the credential.
        let body = format!("upstream {CREDENTIALED_URL} refused");
        let chain_err = ChainError::from(PoolError::Rpc(TransportErrorKind::http_error(503, body)));
        let ChainError::Fault(Fault::Unavailable(msg)) = chain_err else {
            panic!("expected Unavailable fault, got {chain_err:?}");
        };
        assert_no_endpoint(&msg);
        assert!(msg.contains("503"), "status kept: {msg}");
    }

    #[test]
    fn fault_drops_a_host_borne_or_short_path_key() {
        // A provider key can sit in the subdomain or a path segment of 20
        // chars or fewer, which `redact_url` keeps, so the guest fault
        // drops the whole URL.
        for url in [
            "https://k7fQz2m9Xd.eth.rpc.example.com/",
            "https://rpc.example.com/k7fQz2m9Xd",
        ] {
            let msg = format!("error sending request for url ({url})");
            let chain_err = ChainError::from(PoolError::Rpc(transport_err(&msg)));
            let ChainError::Fault(Fault::Unavailable(msg)) = chain_err else {
                panic!("expected Unavailable fault, got {chain_err:?}");
            };
            assert!(!msg.contains("k7fQz2m9Xd"), "key gone: {msg}");
            assert!(!msg.contains("rpc.example.com"), "endpoint gone: {msg}");
        }
    }

    #[test]
    fn fault_redaction_survives_multibyte_server_text() {
        // An HTTP error body is server-controlled and can abut the URL with
        // a multi-byte char (a typographic quote, an NBSP); redaction must
        // not panic, since a host panic aborts the whole process.
        let body = format!("upstream \u{201c}{CREDENTIALED_URL}\u{201d} refused");
        let chain_err = ChainError::from(PoolError::Rpc(TransportErrorKind::http_error(503, body)));
        let ChainError::Fault(Fault::Unavailable(msg)) = chain_err else {
            panic!("expected Unavailable fault, got {chain_err:?}");
        };
        assert_no_endpoint(&msg);
    }

    #[test]
    fn dropped_backend_fault_carries_no_endpoint() {
        // `BackendGone` and `PubsubUnavailable` render compile-time constant
        // text with no data, so this pins that the constants stay free of
        // the endpoint; it cannot observe the defensive redaction at the
        // call site, which is a no-op on such text today.
        for source in [
            TransportErrorKind::backend_gone(),
            TransportErrorKind::pubsub_unavailable(),
        ] {
            let chain_err = ChainError::from(PoolError::Rpc(source));
            let ChainError::Fault(Fault::Unavailable(msg)) = chain_err else {
                panic!("expected Unavailable fault, got {chain_err:?}");
            };
            assert_no_endpoint(&msg);
            assert!(!msg.is_empty(), "the fault still carries a diagnosis");
        }
    }

    #[test]
    fn error_resp_message_redacts_an_echoed_endpoint() {
        // A node error message can echo the endpoint it was reached on.
        let payload: ErrorPayload = serde_json::from_str(&format!(
            r#"{{"code":-32005,"message":"daily limit reached for {CREDENTIALED_URL}"}}"#
        ))
        .expect("payload parses");
        let chain_err = ChainError::from(PoolError::Rpc(AlloyRpcError::ErrorResp(payload)));
        let ChainError::Rpc(rpc) = chain_err else {
            panic!("expected ChainError::Rpc, got {chain_err:?}");
        };
        assert_no_endpoint(&rpc.message);
        assert_eq!(rpc.code, -32005);
        assert!(
            rpc.message.contains("daily limit reached"),
            "node message kept: {}",
            rpc.message
        );
    }

    #[test]
    fn timeout_sniff_classifies_before_redaction() {
        // The sniff runs on the unredacted text, so a URL in the message
        // cannot change the class; `timeout` carries no payload to leak.
        let msg = format!("request to {CREDENTIALED_URL} timed out");
        let chain_err = ChainError::from(PoolError::Rpc(transport_err(&msg)));
        assert!(matches!(chain_err, ChainError::Fault(Fault::Timeout)));
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
        // alloy's `ErrorPayload.code` is `i64`; an out-of-`i32` code clamps
        // to `-32603` rather than poisoning the projection.
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
