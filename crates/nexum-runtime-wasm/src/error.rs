//! The only place a guest-visible `chain-error` or local-store [`Fault`] is
//! built. The scan test below enforces that.

use alloy_primitives::Bytes;
use alloy_transport::TransportError;
use nexum_runtime_api::StoreError;
use nexum_runtime_api::bindings::nexum::host::chain::{ChainError, RpcError};
use nexum_runtime_api::bindings::nexum::host::types::{Fault, RateLimit};
use nexum_runtime_chain::PoolError;

/// Fieldless on purpose: no runtime string can enter the set, so nothing
/// upstream or operator-derived crosses the boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr, strum::VariantArray)]
pub(crate) enum ChainFaultMessage {
    #[strum(serialize = "chain has no configured RPC endpoint")]
    ChainNotConfigured,
    #[strum(serialize = "method is outside the permitted read-only surface")]
    MethodNotPermitted,
    #[strum(serialize = "upstream RPC endpoint unavailable")]
    UpstreamUnavailable,
    #[strum(serialize = "request params are not valid JSON")]
    InvalidParams,
    #[strum(serialize = "chain response exceeds the configured cap")]
    ResponseOverCap,
    #[strum(serialize = "upstream node returned an error response")]
    UpstreamErrorResponse,
}

impl ChainFaultMessage {
    pub(crate) fn text(self) -> &'static str {
        self.into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr, strum::VariantArray)]
pub(crate) enum LocalStoreFaultMessage {
    #[strum(serialize = "local-store namespace quota exhausted")]
    QuotaExhausted,
    #[strum(serialize = "local-store write can never fit the namespace quota")]
    WriteNeverFits,
    #[strum(serialize = "apply batch exceeds the per-batch op cap")]
    ApplyOpsOverCap,
    #[strum(serialize = "apply batch exceeds the per-batch value-byte cap")]
    ApplyBytesOverCap,
    #[strum(serialize = "local-store backend failure")]
    BackendFailure,
}

impl LocalStoreFaultMessage {
    pub(crate) fn text(self) -> &'static str {
        self.into()
    }
}

/// The only route from a [`StoreError`] to a guest [`Fault`]. Deliberately
/// not a `From` impl: `?` would skip the log below, and only that log keeps
/// the quota value and the backend text for the operator.
pub(crate) fn store_fault(
    module: impl std::fmt::Display,
    verb: &'static str,
    err: StoreError,
) -> Fault {
    let refusal = matches!(
        err,
        StoreError::QuotaExceeded { .. }
            | StoreError::QuotaUnsatisfiable { .. }
            | StoreError::ApplyOpsExceeded { .. }
            | StoreError::ApplyBytesExceeded { .. }
    );
    if refusal {
        // Below WARN: a module sitting at its quota or batch cap would
        // otherwise flood the operator log on every dispatch.
        tracing::debug!(module = %module, verb, error = %err, "local-store verb refused");
    } else {
        tracing::warn!(module = %module, verb, error = %err, "local-store verb failed");
    }
    match err {
        StoreError::QuotaExceeded { .. } => {
            Fault::Denied(LocalStoreFaultMessage::QuotaExhausted.text().to_owned())
        }
        StoreError::QuotaUnsatisfiable { .. } => {
            Fault::Denied(LocalStoreFaultMessage::WriteNeverFits.text().to_owned())
        }
        StoreError::ApplyOpsExceeded { .. } => {
            Fault::InvalidInput(LocalStoreFaultMessage::ApplyOpsOverCap.text().to_owned())
        }
        StoreError::ApplyBytesExceeded { .. } => {
            Fault::InvalidInput(LocalStoreFaultMessage::ApplyBytesOverCap.text().to_owned())
        }
        StoreError::Backend(_) | StoreError::InvalidNamespace(_) => {
            Fault::Internal(LocalStoreFaultMessage::BackendFailure.text().to_owned())
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

/// The only route from a [`PoolError`] to `chain-error`: a structured
/// JSON-RPC `ErrorResp` becomes [`ChainError::Rpc`] with its code and revert
/// bytes, everything else a shared [`Fault`].
pub(crate) fn pool_fault(err: PoolError) -> ChainError {
    match err {
        PoolError::UnknownChain(_) => ChainError::Fault(Fault::Unsupported(
            ChainFaultMessage::ChainNotConfigured.text().to_owned(),
        )),
        // The configured per-request timeout elapsed. The dedicated
        // timeout fault lets a guest tell a slow node apart from a
        // revert or an unreachable endpoint.
        PoolError::Timeout => ChainError::Fault(Fault::Timeout),
        PoolError::Rpc(source) => classify_rpc(&source),
        // Boot-time only: `from_config` refuses before any guest runs,
        // so the request path never sees this arm.
        PoolError::Connect { source, .. } => classify_rpc(&source),
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

    const CREDENTIALED_URL: &str =
        "http://user:passsecret@127.0.0.1:1/v2/THISISALONGAPIKEY1234567890?apikey=qsecret";

    fn vocabulary() -> Vec<&'static str> {
        use strum::VariantArray as _;
        ChainFaultMessage::VARIANTS
            .iter()
            .map(|m| m.text())
            .collect()
    }

    fn store_vocabulary() -> Vec<&'static str> {
        use strum::VariantArray as _;
        LocalStoreFaultMessage::VARIANTS
            .iter()
            .map(|m| m.text())
            .collect()
    }

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

    /// `VARIANTS` is compiler-derived, so a new case fails this until the
    /// pinned list is extended.
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
                "upstream node returned an error response",
            ],
        );
    }

    #[test]
    fn the_local_store_vocabulary_is_pinned_and_closed() {
        assert_eq!(
            store_vocabulary(),
            [
                "local-store namespace quota exhausted",
                "local-store write can never fit the namespace quota",
                "apply batch exceeds the per-batch op cap",
                "apply batch exceeds the per-batch value-byte cap",
                "local-store backend failure",
            ],
        );
    }

    /// Equality, not absence: the operator-configured quota value must not
    /// survive the projection in any form. The two denials stay distinct,
    /// so a guest knows whether deletes can help.
    #[test]
    fn quota_faults_are_denied_vocabulary_text_without_the_quota_value() {
        let exhausted = store_fault(
            "m",
            "set",
            StoreError::QuotaExceeded {
                needed: 987_654_321,
                quota: 123_456_789,
            },
        );
        let never_fits = store_fault(
            "m",
            "set",
            StoreError::QuotaUnsatisfiable {
                needed: 987_654_321,
                quota: 123_456_789,
            },
        );
        let (Fault::Denied(exhausted), Fault::Denied(never_fits)) = (&exhausted, &never_fits)
        else {
            panic!("expected Denied faults, got {exhausted:?} and {never_fits:?}");
        };
        assert_eq!(exhausted, LocalStoreFaultMessage::QuotaExhausted.text());
        assert_eq!(never_fits, LocalStoreFaultMessage::WriteNeverFits.text());
        assert_ne!(exhausted, never_fits);
    }

    /// The two batch caps stay `invalid-input` and stay distinguishable, so
    /// a guest knows whether to shrink the op count or the value bytes.
    #[test]
    fn apply_cap_faults_are_distinct_invalid_input_vocabulary_text() {
        let ops = store_fault(
            "m",
            "apply",
            StoreError::ApplyOpsExceeded {
                ops: 4096,
                cap: 1024,
            },
        );
        let bytes = store_fault(
            "m",
            "apply",
            StoreError::ApplyBytesExceeded {
                bytes: 1 << 30,
                cap: 4 << 20,
            },
        );
        let (Fault::InvalidInput(ops), Fault::InvalidInput(bytes)) = (&ops, &bytes) else {
            panic!("expected InvalidInput faults, got {ops:?} and {bytes:?}");
        };
        assert_eq!(ops, LocalStoreFaultMessage::ApplyOpsOverCap.text());
        assert_eq!(bytes, LocalStoreFaultMessage::ApplyBytesOverCap.text());
        assert_ne!(ops, bytes);
    }

    #[test]
    fn backend_faults_are_internal_and_carry_no_upstream_text() {
        // A backend failure wrapping path-bearing io text, and the
        // host-built namespace refusal.
        let io = std::io::Error::other("I/O error: /var/lib/nexum/state/local-store.redb");
        for err in [
            StoreError::Backend(io.into()),
            StoreError::InvalidNamespace("module namespace must not be empty".into()),
        ] {
            let fault = store_fault("m", "get", err);
            let Fault::Internal(msg) = fault else {
                panic!("expected Internal fault, got {fault:?}");
            };
            assert_eq!(msg, LocalStoreFaultMessage::BackendFailure.text());
        }
    }

    /// Pins the operator half of the seam: the log keeps exactly what the
    /// guest fault must not, and refusals sit below WARN.
    #[test]
    fn store_fault_logs_the_full_error_host_side_and_splits_levels() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("sink lock").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Sink;
            fn make_writer(&'a self) -> Sink {
                self.clone()
            }
        }

        let sink = Sink::default();
        let collector = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(sink.clone())
            .finish();
        tracing::subscriber::with_default(collector, || {
            let _ = store_fault(
                "mod-a",
                "set",
                StoreError::QuotaExceeded {
                    needed: 987_654_321,
                    quota: 123_456_789,
                },
            );
            let io = std::io::Error::other("I/O error: /var/lib/nexum/state/local-store.redb");
            let _ = store_fault("mod-a", "get", StoreError::Backend(io.into()));
        });
        let out = String::from_utf8(sink.0.lock().expect("sink lock").clone())
            .expect("log output is UTF-8");
        let quota = out
            .lines()
            .find(|l| l.contains("123456789"))
            .expect("the quota value is logged");
        assert!(quota.contains("DEBUG"), "refusal above DEBUG: {quota}");
        let redb = out
            .lines()
            .find(|l| l.contains("/var/lib/nexum/state/local-store.redb"))
            .expect("the redb text is logged");
        assert!(redb.contains("WARN"), "backend failure not WARN: {redb}");
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
        let chain_err = pool_fault(PoolError::UnknownChain(Chain::from_id(424242)));
        let ChainError::Fault(Fault::Unsupported(msg)) = chain_err else {
            panic!("expected Unsupported fault, got {chain_err:?}");
        };
        assert_eq!(msg, ChainFaultMessage::ChainNotConfigured.text());
    }

    #[test]
    fn timeout_maps_to_timeout_fault() {
        // The tokio-elapsed leg surfaces as the dedicated `timeout` fault,
        // distinct from a revert (`Rpc`) or an unreachable node.
        let chain_err = pool_fault(PoolError::Timeout);
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

        let chain_err = pool_fault(PoolError::Rpc(TransportErrorKind::custom(err)));
        assert!(matches!(chain_err, ChainError::Fault(Fault::Timeout)));
    }

    #[test]
    fn message_only_timeout_maps_to_timeout_fault() {
        // The retained last-resort sniff: no typed timeout anywhere in the
        // chain, only the message marks it.
        let chain_err = pool_fault(PoolError::Rpc(transport_err("request timed out after 30s")));
        assert!(matches!(chain_err, ChainError::Fault(Fault::Timeout)));
    }

    #[test]
    fn transport_failure_maps_to_unavailable_fault() {
        let chain_err = pool_fault(PoolError::Rpc(transport_err("websocket disconnected")));
        assert!(matches!(
            chain_err,
            ChainError::Fault(Fault::Unavailable(_))
        ));
    }

    #[test]
    fn backend_gone_maps_to_unavailable_fault() {
        let chain_err = pool_fault(PoolError::Rpc(TransportErrorKind::backend_gone()));
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
        let chain_err = pool_fault(PoolError::Rpc(TransportErrorKind::custom(err)));
        let ChainError::Fault(Fault::Unavailable(msg)) = chain_err else {
            panic!("expected Unavailable fault, got {chain_err:?}");
        };
        assert_eq!(msg, ChainFaultMessage::UpstreamUnavailable.text());
    }

    /// Upstream text is attacker-influenced, so assert equality, not absence.
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
            let chain_err = pool_fault(PoolError::Rpc(source));
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
        let chain_err = pool_fault(PoolError::Rpc(AlloyRpcError::ErrorResp(payload)));
        let ChainError::Rpc(rpc) = chain_err else {
            panic!("expected ChainError::Rpc, got {chain_err:?}");
        };
        assert_eq!(rpc.message, ChainFaultMessage::UpstreamErrorResponse.text());
        assert_eq!(rpc.code, -32005);
    }

    #[test]
    fn timeout_sniff_still_classifies_url_bearing_text() {
        let msg = format!("request to {CREDENTIALED_URL} timed out");
        let chain_err = pool_fault(PoolError::Rpc(transport_err(&msg)));
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
        let chain_err = pool_fault(PoolError::Rpc(AlloyRpcError::ErrorResp(payload)));
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
            let chain_err = pool_fault(PoolError::Rpc(AlloyRpcError::ErrorResp(payload)));
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
        let chain_err = pool_fault(PoolError::Rpc(AlloyRpcError::ErrorResp(payload)));
        let ChainError::Rpc(rpc) = chain_err else {
            panic!("expected ChainError::Rpc, got {chain_err:?}");
        };
        assert_eq!(rpc.code, -32603);
    }

    #[test]
    fn ser_error_maps_to_invalid_input_fault() {
        let source = serde_json::from_str::<serde_json::Value>("not json")
            .expect_err("`not json` is not valid JSON");
        let chain_err = pool_fault(PoolError::Rpc(AlloyRpcError::SerError(source)));
        let ChainError::Fault(Fault::InvalidInput(msg)) = chain_err else {
            panic!("expected InvalidInput fault, got {chain_err:?}");
        };
        assert_eq!(msg, ChainFaultMessage::InvalidParams.text());
    }

    fn rust_sources(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("source dir reads") {
                let path = entry.expect("dir entry reads").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        out
    }

    /// Text before a `#[cfg(test)] mod`. A single gated item is kept, which
    /// only over-scans.
    fn shipped_region(text: &str) -> &str {
        const ATTR: &str = "#[cfg(test)]";
        let mut from = 0;
        while let Some(i) = text[from..].find(ATTR) {
            let at = from + i;
            if text[at + ATTR.len()..].trim_start().starts_with("mod ") {
                return &text[..at];
            }
            from = at + ATTR.len();
        }
        text
    }

    /// So a multi-line construction scans as one token run.
    fn squash(code: &str) -> String {
        code.lines()
            .map(|line| {
                if line.trim_start().starts_with("//") {
                    ""
                } else {
                    line.find(" //").map_or(line, |i| &line[..i])
                }
            })
            .flat_map(|line| line.chars().filter(|c| !c.is_whitespace()))
            .collect()
    }

    const FAULT_PREFIXES: [&str; 5] = [
        "Fault::Unsupported(",
        "Fault::Unavailable(",
        "Fault::Denied(",
        "Fault::InvalidInput(",
        "Fault::Internal(",
    ];

    /// Suffixes of `code` just past each occurrence of `prefix`.
    fn occurrences<'a>(code: &'a str, prefix: &str) -> Vec<&'a str> {
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(i) = code[from..].find(prefix) {
            let after = from + i + prefix.len();
            out.push(&code[after..]);
            from = after;
        }
        out
    }

    /// Returns how many sites were checked.
    fn funnel_constructions(code: &str) -> usize {
        let mut sites = 0;
        for prefix in FAULT_PREFIXES.into_iter().chain(["message:"]) {
            for rest in occurrences(code, prefix) {
                assert!(
                    rest.starts_with("ChainFaultMessage::")
                        || rest.starts_with("LocalStoreFaultMessage::"),
                    "a fault payload at `{prefix}` is not a vocabulary projection",
                );
                sites += 1;
            }
        }
        sites
    }

    /// Closes the set over construction sites, not just texts: the pinned
    /// lists alone cannot see a payload built somewhere else. Outside the
    /// funnel a string-carrying fault is banned outright, save the pure
    /// destructures in `fault.rs`.
    #[test]
    fn guest_faults_are_constructed_only_in_the_funnel_and_only_from_the_vocabulary() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let roots = [
            manifest.join("src"),
            manifest.join("../nexum-runtime/src"),
            manifest.join("../nexum-runtime-api/src"),
            manifest.join("../nexum-runtime-chain/src"),
            manifest.join("../nexum-runtime-store/src"),
            manifest.join("../nexum-runtime-logs/src"),
            manifest.join("../nexum-runtime-http/src"),
            manifest.join("../nexum-runtime-supervisor/src"),
            manifest.join("../nexum-runtime-testing/src"),
        ];
        let funnel = manifest.join("src").join("error.rs");
        let projections = manifest.join("src").join("fault.rs");
        let mut scanned = 0_usize;
        let mut sites = 0_usize;
        for path in roots.iter().flat_map(|root| rust_sources(root)) {
            let text = std::fs::read_to_string(&path).expect("source file reads");
            let code = squash(shipped_region(&text));
            scanned += 1;
            if path == funnel {
                sites = funnel_constructions(&code);
                // A `From` impl would let `?` reach the guest while skipping
                // the operator log in `store_fault`.
                assert!(
                    code.contains("StoreError"),
                    "the funnel no longer names the store seam error, so this guard scans nothing",
                );
                assert!(
                    !code.contains("From<StoreError>"),
                    "the store projection must stay a logging function, not a From impl",
                );
                assert!(
                    code.contains("PoolError"),
                    "the funnel no longer names the pool error, so this guard scans nothing",
                );
                assert!(
                    !code.contains("From<PoolError>"),
                    "the pool projection must stay a free function, not a From impl",
                );
                continue;
            }
            for token in [
                "ChainError::Fault(",
                "ChainError::Rpc(",
                "RpcError{",
                "Self::Fault(",
                "Self::Rpc(",
                "From<StoreError>",
                "From<PoolError>",
                "Self::Unsupported(",
                "Self::Unavailable(",
                "Self::Denied(",
                "Self::InvalidInput(",
                "Self::Internal(",
            ] {
                assert!(
                    !code.contains(token),
                    "{} builds a guest fault outside the funnel: `{token}`",
                    path.display(),
                );
            }
            if path == projections {
                for prefix in FAULT_PREFIXES {
                    for rest in occurrences(&code, prefix) {
                        assert!(
                            rest.starts_with("_)") || rest.starts_with("m)"),
                            "{} must only destructure a fault, at `{prefix}`",
                            path.display(),
                        );
                    }
                }
            } else {
                for prefix in FAULT_PREFIXES {
                    assert!(
                        occurrences(&code, prefix).is_empty(),
                        "{} builds a string-carrying fault outside the funnel: `{prefix}`",
                        path.display(),
                    );
                }
            }
        }
        // Above 76 minus the smallest root, so losing any one root fails.
        assert!(
            scanned >= 75,
            "the walk must cover the embedding, the facade, the supervisor, the api crate, and the capability crates, saw {scanned} files"
        );
        assert!(
            sites >= 10,
            "the funnel holds the construction sites, saw {sites}"
        );
    }
}
