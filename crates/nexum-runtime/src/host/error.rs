//! Constructors and `From` conversions building the WIT error shapes
//! (`chain-error`, `Fault`); `fault_label` / `fault_message` project a
//! `Fault` into metric and log fields.

use alloy_primitives::Bytes;
use alloy_transport::TransportError;

use crate::bindings::nexum::host::chain::{ChainError, RpcError};
use crate::bindings::nexum::host::types::{Fault, RateLimit};
use crate::host::local_store_redb::StorageError;
use crate::host::provider_pool::PoolError;

/// The complete guest-visible chain fault vocabulary. Fieldless on
/// purpose: [`text`](Self::text) maps each case to a fixed `&'static str`,
/// so neither upstream error text nor anything derived from operator
/// configuration can cross the WIT boundary through a chain fault. The
/// caller logs the full upstream error host-side before converting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::VariantArray)]
pub(crate) enum ChainFaultMessage {
    ChainNotConfigured,
    MethodNotPermitted,
    UpstreamUnavailable,
    InvalidParams,
    ResponseOverCap,
    UpstreamErrorResponse,
}

impl ChainFaultMessage {
    pub(crate) const fn text(self) -> &'static str {
        match self {
            Self::ChainNotConfigured => "chain has no configured RPC endpoint",
            Self::MethodNotPermitted => "method is outside the permitted read-only surface",
            Self::UpstreamUnavailable => "upstream RPC endpoint unavailable",
            Self::InvalidParams => "request params are not valid JSON",
            Self::ResponseOverCap => "chain response exceeds the configured cap",
            Self::UpstreamErrorResponse => "upstream node returned an error response",
        }
    }
}

pub(crate) fn method_denied() -> ChainError {
    ChainError::Fault(Fault::Denied(
        ChainFaultMessage::MethodNotPermitted.text().to_owned(),
    ))
}

pub(crate) fn response_over_cap() -> ChainError {
    ChainError::Fault(Fault::InvalidInput(
        ChainFaultMessage::ResponseOverCap.text().to_owned(),
    ))
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
            PoolError::UnknownChain(_) => ChainError::Fault(Fault::Unsupported(
                ChainFaultMessage::ChainNotConfigured.text().to_owned(),
            )),
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
/// else a transport [`Fault`]. The node's message text stays host-side; the
/// guest classifies on the code and the revert bytes.
fn classify_rpc(source: &TransportError) -> ChainError {
    // A structured error response (typically an `eth_call` revert) keeps the
    // node's code and revert body so a guest can classify via `decode_revert`.
    if let Some(payload) = source.as_error_resp() {
        return ChainError::Rpc(RpcError {
            // A code outside `i32` is a JSON-RPC spec violation, clamped
            // to `-32603` Internal error.
            code: i32::try_from(payload.code).unwrap_or(-32603),
            message: ChainFaultMessage::UpstreamErrorResponse.text().to_owned(),
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
        alloy_transport::RpcError::SerError(_) => ChainError::Fault(Fault::InvalidInput(
            ChainFaultMessage::InvalidParams.text().to_owned(),
        )),
        _ => ChainError::Fault(transport_fault(source)),
    }
}

/// Classify a transport RPC failure: 429 to `rate-limited`, 503 or a dropped
/// backend to `unavailable`, a timeout to `timeout`, else `unavailable`.
fn transport_fault(source: &TransportError) -> Fault {
    use alloy_transport::TransportErrorKind;
    let unavailable =
        || Fault::Unavailable(ChainFaultMessage::UpstreamUnavailable.text().to_owned());
    if let Some(kind) = source.as_transport_err() {
        match kind {
            TransportErrorKind::HttpError(http) if http.status == 429 => {
                return Fault::RateLimited(RateLimit {
                    retry_after_ms: None,
                });
            }
            TransportErrorKind::HttpError(http) if http.status == 503 => {
                return unavailable();
            }
            TransportErrorKind::BackendGone | TransportErrorKind::PubsubUnavailable => {
                return unavailable();
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
    // The text is only sniffed, never forwarded.
    let lower = source.to_string().to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        Fault::Timeout
    } else {
        unavailable()
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

    /// A credential-bearing endpoint as an upstream error might echo it.
    const CREDENTIALED_URL: &str =
        "http://user:passsecret@127.0.0.1:1/v2/THISISALONGAPIKEY1234567890?apikey=qsecret";

    fn vocabulary() -> Vec<&'static str> {
        use strum::VariantArray as _;
        ChainFaultMessage::VARIANTS
            .iter()
            .map(|m| m.text())
            .collect()
    }

    /// The message `err` would carry across the WIT boundary, if any.
    fn guest_message(err: &ChainError) -> Option<&str> {
        match err {
            ChainError::Fault(
                Fault::Unsupported(m)
                | Fault::Unavailable(m)
                | Fault::Denied(m)
                | Fault::InvalidInput(m)
                | Fault::Internal(m),
            ) => Some(m),
            ChainError::Fault(Fault::RateLimited(_) | Fault::Timeout) => None,
            ChainError::Rpc(rpc) => Some(&rpc.message),
        }
    }

    /// Every message a chain fault can carry is one of the seven fixed
    /// texts. `VARIANTS` is compiler-derived, so a new `ChainFaultMessage`
    /// case joins the enumeration on its own and the equality fails until
    /// the pinned list is consciously extended; and because `text` maps a
    /// fieldless `Copy` enum to `&'static str`, no runtime string
    /// (upstream text, operator configuration) can enter the set at all.
    #[test]
    fn the_guest_vocabulary_is_pinned_and_closed() {
        assert_eq!(
            vocabulary(),
            [
                "chain has no configured RPC endpoint",
                "method is outside the permitted read-only surface",
                "upstream RPC endpoint unavailable",
                "request params are not valid JSON",
                "chain response exceeds the configured cap",
                "chain batch aggregate exceeds the configured cap",
                "upstream node returned an error response",
            ],
        );
    }

    #[test]
    fn the_chain_fault_constructors_stay_inside_the_vocabulary() {
        for err in [method_denied(), response_over_cap()] {
            let msg = guest_message(&err).expect("each constructor carries a message");
            assert!(vocabulary().contains(&msg), "outside the vocabulary: {msg}");
        }
    }

    #[test]
    fn unknown_chain_is_unsupported_fault() {
        let chain_err = ChainError::from(PoolError::UnknownChain(Chain::from_id(424242)));
        let ChainError::Fault(Fault::Unsupported(msg)) = chain_err else {
            panic!("expected Unsupported fault, got {chain_err:?}");
        };
        assert_eq!(msg, ChainFaultMessage::ChainNotConfigured.text());
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
    async fn unreachable_endpoint_fault_carries_only_vocabulary_text() {
        let err = reqwest::Client::new()
            .post(CREDENTIALED_URL)
            .send()
            .await
            .expect_err("port 1 refuses the connection");
        let chain_err = ChainError::from(PoolError::Rpc(TransportErrorKind::custom(err)));
        let ChainError::Fault(Fault::Unavailable(msg)) = chain_err else {
            panic!("expected Unavailable fault, got {chain_err:?}");
        };
        assert_eq!(msg, ChainFaultMessage::UpstreamUnavailable.text());
    }

    /// Upstream text is attacker-influenced; equality with the fixed
    /// vocabulary text proves none of it is forwarded, whatever it embeds.
    #[test]
    fn upstream_text_never_reaches_the_guest() {
        let adversarial = [
            TransportErrorKind::http_error(503, format!("upstream {CREDENTIALED_URL} refused")),
            TransportErrorKind::http_error(
                503,
                format!("upstream \u{201c}{CREDENTIALED_URL}\u{201d} refused"),
            ),
            transport_err(&format!(
                "error sending request for url ({CREDENTIALED_URL})"
            )),
            transport_err(
                "error sending request for url (https://k7fQz2m9Xd.eth.rpc.example.com/)",
            ),
            TransportErrorKind::backend_gone(),
            TransportErrorKind::pubsub_unavailable(),
        ];
        for source in adversarial {
            let chain_err = ChainError::from(PoolError::Rpc(source));
            let ChainError::Fault(Fault::Unavailable(msg)) = chain_err else {
                panic!("expected Unavailable fault, got {chain_err:?}");
            };
            assert_eq!(msg, ChainFaultMessage::UpstreamUnavailable.text());
        }
    }

    #[test]
    fn error_resp_message_is_vocabulary_text() {
        // The node's message echoes the endpoint; only the fixed text, the
        // code, and the decoded revert bytes cross the boundary.
        let payload: ErrorPayload = serde_json::from_str(&format!(
            r#"{{"code":-32005,"message":"daily limit reached for {CREDENTIALED_URL}"}}"#
        ))
        .expect("payload parses");
        let chain_err = ChainError::from(PoolError::Rpc(AlloyRpcError::ErrorResp(payload)));
        let ChainError::Rpc(rpc) = chain_err else {
            panic!("expected ChainError::Rpc, got {chain_err:?}");
        };
        assert_eq!(rpc.message, ChainFaultMessage::UpstreamErrorResponse.text());
        assert_eq!(rpc.code, -32005);
    }

    #[test]
    fn timeout_sniff_still_classifies_url_bearing_text() {
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
        assert_eq!(rpc.message, ChainFaultMessage::UpstreamErrorResponse.text());
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
        let ChainError::Fault(Fault::InvalidInput(msg)) = chain_err else {
            panic!("expected InvalidInput fault, got {chain_err:?}");
        };
        assert_eq!(msg, ChainFaultMessage::InvalidParams.text());
    }
}
