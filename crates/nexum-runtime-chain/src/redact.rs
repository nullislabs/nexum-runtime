//! Keeps a chain's RPC URL out of everything its transport reports.
//!
//! An HTTP client error prints the request URL, an upstream error page
//! echoes the path, and alloy's HTTP transport opens a tracing span with
//! the URL as a field. [`RedactLayer`] sits directly on the transport, so
//! every error leaves it already redacted: the retry layer above, the
//! pollers, and the engine's own log lines never hold the original.

use std::borrow::Cow;
use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::task::{Context, Poll};

use alloy_json_rpc::{ErrorPayload, RequestPacket, Response, ResponsePacket, ResponsePayload};
use alloy_transport::{RpcError, TransportError, TransportErrorKind, TransportFut};
use serde_json::value::RawValue;
use tower::{Layer, Service};

use nexum_runtime_config::RpcEndpoint;

type BoxError = Box<dyn StdError + Send + Sync + 'static>;

/// Wraps one endpoint's transport in [`Redact`].
#[derive(Clone)]
pub(crate) struct RedactLayer(pub(crate) RpcEndpoint);

impl<S> Layer<S> for RedactLayer {
    type Service = Redact<S>;

    fn layer(&self, inner: S) -> Redact<S> {
        Redact {
            inner,
            endpoint: self.0.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Redact<S> {
    inner: S,
    endpoint: RpcEndpoint,
}

impl<S> Service<RequestPacket> for Redact<S>
where
    S: Service<
            RequestPacket,
            Response = ResponsePacket,
            Error = TransportError,
            Future = TransportFut<'static>,
        >,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), TransportError>> {
        self.inner
            .poll_ready(cx)
            .map_err(|err| redact_error(&self.endpoint, err))
    }

    fn call(&mut self, req: RequestPacket) -> TransportFut<'static> {
        // alloy's HTTP transport opens its span, `url` field included, in
        // `call`. Opened under the no-op dispatcher, the span belongs to no
        // real subscriber, so no log line carries it at any level.
        let response =
            tracing::dispatcher::with_default(&tracing::Dispatch::none(), || self.inner.call(req));
        let endpoint = self.endpoint.clone();
        Box::pin(async move {
            match response.await {
                Ok(packet) => Ok(redact_packet(&endpoint, packet)),
                Err(err) => Err(redact_error(&endpoint, err)),
            }
        })
    }
}

/// `err` with the endpoint's URL removed from its text. The variant is
/// kept, so the retry layer and the fault classifier decide as before.
pub(crate) fn redact_error(endpoint: &RpcEndpoint, err: TransportError) -> TransportError {
    let text = |text: &str| endpoint.redact(text).into_owned();
    match err {
        RpcError::Transport(kind) => RpcError::Transport(match kind {
            TransportErrorKind::Custom(source) => {
                TransportErrorKind::Custom(Scrubbed::boxed(endpoint, source))
            }
            TransportErrorKind::NonRetryable(source) => {
                TransportErrorKind::NonRetryable(Scrubbed::boxed(endpoint, source))
            }
            TransportErrorKind::HttpError(http) => {
                return TransportErrorKind::http_error(http.status, text(&http.body));
            }
            kind @ (TransportErrorKind::MissingBatchResponse(_)
            | TransportErrorKind::BackendGone
            | TransportErrorKind::PubsubUnavailable) => kind,
            // A variant alloy adds later is redacted as text until it has
            // an arm here.
            kind => TransportErrorKind::Custom(Scrubbed::boxed(endpoint, Box::new(kind))),
        }),
        RpcError::ErrorResp(payload) => RpcError::ErrorResp(redact_payload(endpoint, payload)),
        RpcError::DeserError { err, text: body } => RpcError::DeserError {
            err,
            text: text(&body),
        },
        RpcError::LocalUsageError(source) => {
            RpcError::LocalUsageError(Scrubbed::boxed(endpoint, source))
        }
        // No upstream text.
        err @ (RpcError::NullResp | RpcError::UnsupportedFeature(_) | RpcError::SerError(_)) => err,
    }
}

/// A JSON-RPC error response arrives as a successful packet, and its
/// message and data are the node's own text.
fn redact_packet(endpoint: &RpcEndpoint, packet: ResponsePacket) -> ResponsePacket {
    let redact = |response: Response| Response {
        id: response.id,
        payload: match response.payload {
            ResponsePayload::Failure(payload) => {
                ResponsePayload::Failure(redact_payload(endpoint, payload))
            }
            success @ ResponsePayload::Success(_) => success,
        },
    };
    match packet {
        ResponsePacket::Single(response) => ResponsePacket::Single(redact(response)),
        ResponsePacket::Batch(batch) => {
            ResponsePacket::Batch(batch.into_iter().map(redact).collect())
        }
    }
}

fn redact_payload(endpoint: &RpcEndpoint, payload: ErrorPayload) -> ErrorPayload {
    let ErrorPayload {
        code,
        message,
        data,
    } = payload;
    let message = match endpoint.redact(&message) {
        Cow::Borrowed(_) => message,
        Cow::Owned(redacted) => Cow::Owned(redacted),
    };
    // Revert bytes are hex and pass through byte for byte. Data that named
    // the endpoint is rewritten, and dropped if that left it unparsable.
    let data = data.and_then(|data| match endpoint.redact(data.get()) {
        Cow::Borrowed(_) => Some(data),
        Cow::Owned(redacted) => RawValue::from_string(redacted).ok(),
    });
    ErrorPayload {
        code,
        message,
        data,
    }
}

/// A transport failure reduced to redacted text.
///
/// The original is not kept as a `source`: a caller that walks the chain
/// would print it. A typed timeout is restated as an [`io::Error`] of kind
/// `TimedOut`, which is what the fault classifier looks for.
struct Scrubbed {
    text: String,
    timed_out: Option<io::Error>,
}

impl Scrubbed {
    fn boxed(endpoint: &RpcEndpoint, source: BoxError) -> BoxError {
        // The typed removal first; the text pass below covers every other
        // error type and anything the sources print.
        let source: BoxError = match source.downcast::<reqwest::Error>() {
            Ok(http) => Box::new(http.without_url()),
            Err(other) => other,
        };
        let mut timed_out = false;
        let mut chain = Vec::new();
        let mut cursor: Option<&(dyn StdError + 'static)> = Some(&*source);
        while let Some(err) = cursor {
            timed_out |= err
                .downcast_ref::<reqwest::Error>()
                .is_some_and(reqwest::Error::is_timeout)
                || err
                    .downcast_ref::<io::Error>()
                    .is_some_and(|io| io.kind() == io::ErrorKind::TimedOut);
            // A wrapper that already prints its source would say it twice.
            let text = err.to_string();
            if !chain
                .last()
                .is_some_and(|outer: &String| outer.ends_with(&text))
            {
                chain.push(text);
            }
            cursor = err.source();
        }
        Box::new(Self {
            text: endpoint.redact(&chain.join(": ")).into_owned(),
            timed_out: timed_out.then(|| io::ErrorKind::TimedOut.into()),
        })
    }
}

impl fmt::Display for Scrubbed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

// The redacted text and nothing else: a derived `Debug` is one field away
// from printing what `Display` hides.
impl fmt::Debug for Scrubbed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.text)
    }
}

impl StdError for Scrubbed {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.timed_out
            .as_ref()
            .map(|err| err as &(dyn StdError + 'static))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_chains::Chain;
    use alloy_provider::Provider as _;
    use nexum_runtime_config::{ChainConfig, EngineConfig};
    use nexum_runtime_logs::LogCapture;
    use nexum_world::ChainMethod;
    use tracing::Level;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::{PoolError, ProviderPool};

    /// Not a chain id any other line prints.
    const CHAIN_ID: u64 = 11_155_111;
    /// Every part carries its own marker, so no assertion passes by chance.
    const USERINFO: &str = "user-marker:pass-marker";
    const PATH_AND_QUERY: &str = "/v2/path-marker-key?apikey=query-marker-key";
    const MARKERS: [&str; 5] = [
        "user-marker",
        "pass-marker",
        "host-marker",
        "path-marker",
        "query-marker",
    ];

    fn config(url: &str) -> (EngineConfig, RpcEndpoint) {
        let endpoint = RpcEndpoint::try_from(url).expect("test rpc url parses");
        let mut cfg = EngineConfig::default();
        cfg.chains = HashMap::from([(
            Chain::from_id(CHAIN_ID),
            ChainConfig {
                rpc_url: endpoint.clone(),
                request_timeout_secs: 30,
                max_log_range_blocks: 1000,
            },
        )]);
        (cfg, endpoint)
    }

    /// A marked URL on `authority`, a loopback `ip:port`.
    fn marked_url(scheme: &str, authority: &str) -> String {
        format!("{scheme}://{USERINFO}@{authority}{PATH_AND_QUERY}")
    }

    /// A loopback port nothing listens on.
    fn closed_port() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        listener.local_addr().expect("local addr").to_string()
    }

    /// The capture as the engine prints it. The HTTP client under the
    /// provider logs the address it dials, and no layer here can reach
    /// that, so the launcher turns its target off; `nexum-launch` tests
    /// that filter.
    fn engine_lines(capture: &LogCapture) -> String {
        capture
            .text()
            .lines()
            .filter(|line| !line.contains(" hyper_util::client::"))
            .map(|line| format!("{line}\n"))
            .collect()
    }

    fn assert_names_no_endpoint(what: &str, text: &str, authority: &str) {
        for part in MARKERS.into_iter().chain([authority]) {
            assert!(!text.contains(part), "{what} prints {part}: {text}");
        }
    }

    /// `Display`, `Debug`, and every `source` under the error: what a log
    /// field, a panic, and an anyhow report each print.
    fn every_rendering(err: &(dyn StdError + 'static)) -> String {
        let mut text = format!("{err}\n{err:?}");
        let mut cursor = err.source();
        while let Some(source) = cursor {
            text.push_str(&format!("\n{source}\n{source:?}"));
            cursor = source.source();
        }
        text
    }

    #[tokio::test]
    async fn opening_a_provider_logs_the_fingerprint_and_no_part_of_the_url() {
        let authority = "host-marker.example:8443";
        let (cfg, endpoint) = config(&marked_url("https", authority));
        let capture = LogCapture::new();
        let guard = capture.install(Level::TRACE);
        let pool = ProviderPool::from_config(&cfg)
            .await
            .expect("an HTTP provider opens without dialling");
        drop(guard);

        assert_names_no_endpoint("the pool's Debug", &format!("{pool:?}"), ":8443");
        let logs = capture.text();
        assert_names_no_endpoint("the boot log", &logs, ":8443");
        for shown in [
            "opening chain RPC provider",
            &format!("chain_id={CHAIN_ID}"),
            "scheme=\"https\"",
            &format!("endpoint=\"{}\"", endpoint.fingerprint()),
        ] {
            assert!(logs.contains(shown), "the boot log lacks {shown}: {logs}");
        }
    }

    #[tokio::test]
    async fn a_served_request_logs_no_part_of_the_url_at_trace() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"jsonrpc":"2.0","id":0,"result":"0x10"}"#),
            )
            .mount(&server)
            .await;
        let authority = server.address().to_string();
        let (cfg, _) = config(&marked_url("http", &authority));
        let capture = LogCapture::new();
        let guard = capture.install(Level::TRACE);
        let pool = ProviderPool::from_config(&cfg).await.expect("pool opens");
        pool.request(
            Chain::from_id(CHAIN_ID),
            ChainMethod::EthBlockNumber,
            "[]".into(),
        )
        .await
        .expect("the mock answers");
        drop(guard);

        let logs = engine_lines(&capture);
        // alloy's own transport lines are in the capture, so their span
        // would be too.
        assert!(logs.contains("received response from server"), "{logs}");
        assert_names_no_endpoint("the request log", &logs, &authority);
    }

    #[tokio::test]
    async fn a_refused_http_request_names_no_part_of_the_url() {
        let authority = closed_port();
        let (cfg, _) = config(&marked_url("http", &authority));
        let chain = Chain::from_id(CHAIN_ID);
        let capture = LogCapture::new();
        let guard = capture.install(Level::TRACE);
        let pool = ProviderPool::from_config(&cfg).await.expect("pool opens");
        let err = pool
            .request(chain, ChainMethod::EthBlockNumber, "[]".into())
            .await
            .expect_err("nothing listens on the port");
        // The line the engine writes for it.
        tracing::warn!(error = %err, debug = ?err, "chain request failed");
        // The provider handed out for the pollers shares the transport.
        let direct = pool
            .provider(chain)
            .expect("configured chain")
            .get_block_number()
            .await
            .expect_err("nothing listens on the port");
        drop(guard);

        assert!(matches!(err, PoolError::Rpc(_)), "{err:?}");
        assert_names_no_endpoint("the pool error", &every_rendering(&err), &authority);
        assert_names_no_endpoint("the provider error", &every_rendering(&direct), &authority);
        let logs = engine_lines(&capture);
        assert!(logs.contains("chain request failed"), "{logs}");
        assert_names_no_endpoint("the failure log", &logs, &authority);
    }

    #[tokio::test]
    async fn a_refused_ws_connect_names_no_part_of_the_url() {
        let authority = closed_port();
        let (cfg, _) = config(&marked_url("ws", &authority));
        let capture = LogCapture::new();
        let guard = capture.install(Level::TRACE);
        let err = ProviderPool::from_config(&cfg)
            .await
            .expect_err("nothing listens on the port");
        tracing::error!(error = %err, debug = ?err, "boot refused");
        drop(guard);

        assert!(
            matches!(err, PoolError::Connect { chain, .. } if chain.id() == CHAIN_ID),
            "{err:?}"
        );
        assert_names_no_endpoint("the connect error", &every_rendering(&err), &authority);
        let logs = engine_lines(&capture);
        assert!(logs.contains("boot refused"), "{logs}");
        assert_names_no_endpoint("the boot log", &logs, &authority);
    }

    #[tokio::test]
    async fn upstream_text_that_echoes_the_url_is_redacted() {
        let echoes = [
            ResponseTemplate::new(404).set_body_string(
                "<p>The requested URL /v2/path-marker-key?apikey=query-marker-key was not found</p>",
            ),
            ResponseTemplate::new(200).set_body_string("no route for path-marker-key"),
            ResponseTemplate::new(200).set_body_string(
                r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32001,"message":"key query-marker-key is not valid","data":"see /v2/path-marker-key"}}"#,
            ),
        ];
        for echo in echoes {
            let server = MockServer::start().await;
            Mock::given(any()).respond_with(echo).mount(&server).await;
            let authority = server.address().to_string();
            let (cfg, _) = config(&marked_url("http", &authority));
            let pool = ProviderPool::from_config(&cfg).await.expect("pool opens");
            let err = pool
                .request(
                    Chain::from_id(CHAIN_ID),
                    ChainMethod::EthBlockNumber,
                    "[]".into(),
                )
                .await
                .expect_err("the upstream refuses");
            let text = every_rendering(&err);
            assert_names_no_endpoint("the upstream error", &text, &authority);
            assert!(text.contains("[redacted]"), "{text}");
        }
    }

    #[tokio::test]
    async fn an_error_response_keeps_its_code_and_revert_data() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"jsonrpc":"2.0","id":0,"error":{"code":3,"message":"execution reverted","data":"0x08c379a0"}}"#,
            ))
            .mount(&server)
            .await;
        let (cfg, _) = config(&marked_url("http", &server.address().to_string()));
        let pool = ProviderPool::from_config(&cfg).await.expect("pool opens");
        let err = pool
            .request(
                Chain::from_id(CHAIN_ID),
                ChainMethod::EthBlockNumber,
                "[]".into(),
            )
            .await
            .expect_err("the node reverts");
        let PoolError::Rpc(source) = &err else {
            panic!("expected an RPC error, got {err:?}");
        };
        let payload = source.as_error_resp().expect("an error response");
        assert_eq!(payload.code, 3);
        assert_eq!(payload.message, "execution reverted");
        assert_eq!(
            payload.data.as_ref().map(|data| data.get()),
            Some(r#""0x08c379a0""#)
        );
    }

    #[test]
    fn a_redacted_timeout_still_reads_as_a_timeout() {
        let endpoint = RpcEndpoint::try_from("http://127.0.0.1:1").expect("parses");
        let timeout = io::Error::from(io::ErrorKind::TimedOut);
        let err = redact_error(&endpoint, TransportErrorKind::custom(timeout));
        let mut cursor: Option<&(dyn StdError + 'static)> = Some(&err);
        let mut typed_timeout = false;
        while let Some(source) = cursor {
            typed_timeout |= source
                .downcast_ref::<io::Error>()
                .is_some_and(|io| io.kind() == io::ErrorKind::TimedOut);
            cursor = source.source();
        }
        assert!(typed_timeout, "{err:?}");
    }
}
