use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt;

use alloy_chains::Chain;
use nexum_primitives::digest::ContentDigest;
use serde::Deserialize;
use thiserror::Error;

use super::error::{EngineConfigError, zero_field};

/// One `[chains.<id>]` table: how the engine reaches a single chain.
#[derive(Debug, Clone)]
pub struct ChainConfig {
    /// JSON-RPC endpoint, validated at load. `ws(s)://` engages pubsub
    /// (needed for `eth_subscribe`); `http(s)://` is request/response only.
    pub rpc_url: RpcEndpoint,
    /// Per-request timeout in seconds, on both transports, and the bound on
    /// each source open. Default 30, zero refused at load: it would leave
    /// every request unbounded.
    pub request_timeout_secs: u64,
    /// The endpoint's `eth_getLogs` maximum block range, and the bulk-backfill
    /// chunk size. Default 1000, zero refused at load. A value above what the
    /// endpoint serves risks silently skipped logs.
    pub max_log_range_blocks: u64,
}

/// Raw `[chains.<id>]` shape; `rpc_url` stays a string until
/// [`resolve_chains`] validates it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawChainConfig {
    pub(super) rpc_url: String,
    #[serde(default = "default_chain_request_timeout_secs")]
    pub(super) request_timeout_secs: u64,
    #[serde(default = "default_max_log_range_blocks")]
    pub(super) max_log_range_blocks: u64,
}

fn default_chain_request_timeout_secs() -> u64 {
    30
}

fn default_max_log_range_blocks() -> u64 {
    1000
}

/// The transport a chain RPC endpoint's scheme selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcTransport {
    /// `http://` or `https://`: request/response only.
    Http,
    /// `ws://` or `wss://`: pubsub-capable.
    WebSocket,
}

/// What [`RpcEndpoint::redact`] writes in place of each URL part.
const REDACTED: &str = "[redacted]";

/// A path segment or query value this long or longer is also redacted
/// alone: a provider key is one, and an upstream can echo it without the
/// rest of the URL. Shorter ones (`v2`, `rpc`) are common words.
const MIN_KEY_LEN: usize = 8;

/// Parsed once at load, so a malformed `rpc_url` refuses at boot rather
/// than at the first chain call. Deliberately not `Deserialize`: a
/// field-level serde refusal would bypass
/// [`EngineConfigError`](super::EngineConfigError).
///
/// The URL is a secret: its path or query commonly carries a provider key,
/// and the host of a self-hosted node is private. There is no `Display`,
/// and `Debug` prints the scheme and the fingerprint only, so a config dump
/// cannot name the endpoint. [`Self::expose_url`] is the one way out.
#[derive(Clone)]
pub struct RpcEndpoint {
    url: url::Url,
    transport: RpcTransport,
    fingerprint: String,
}

impl RpcEndpoint {
    /// The transport the scheme selected.
    pub fn transport(&self) -> RpcTransport {
        self.transport
    }

    /// True when the transport pushes `newHeads` (ws/wss).
    pub fn supports_pubsub(&self) -> bool {
        matches!(self.transport, RpcTransport::WebSocket)
    }

    /// `http`, `https`, `ws` or `wss`. Safe to log.
    pub fn scheme(&self) -> &str {
        self.url.scheme()
    }

    /// The first 8 hex characters of the SHA-256 of the URL as configured.
    /// Safe to log: it tells two endpoints apart and stays the same across
    /// restarts, and `printf %s "$URL" | sha256sum` reproduces it.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The URL, credentials included, for the code that dials it. Nothing
    /// else may call this: the result must not reach a log line, an error,
    /// a metric label, or a probe body.
    pub fn expose_url(&self) -> &url::Url {
        &self.url
    }

    /// `text` with every part of this URL that names or authenticates the
    /// endpoint replaced: the whole URL, the host with and without its
    /// port, the userinfo, the path, the query, and the fragment. For text
    /// this endpoint produced, such as a transport error or a response
    /// body, where an echo of the request URL is expected.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(text);
        for needle in self.secret_parts() {
            if out.contains(needle.as_str()) {
                out = Cow::Owned(out.replace(needle.as_str(), REDACTED));
            }
        }
        out
    }

    /// Longest first, so the whole URL goes before the parts inside it.
    fn secret_parts(&self) -> Vec<String> {
        let url = &self.url;
        let host = url.host_str().unwrap_or_default();
        // An HTTP client moves the userinfo into a header, so its errors
        // print the URL without it.
        let mut bare = url.clone();
        let _ = bare.set_username("");
        let _ = bare.set_password(None);
        let mut parts = vec![
            url.as_str().to_owned(),
            bare.as_str().to_owned(),
            host.to_owned(),
            url.username().to_owned(),
            url.password().unwrap_or_default().to_owned(),
            url.query().unwrap_or_default().to_owned(),
            url.fragment().unwrap_or_default().to_owned(),
        ];
        if let Some(port) = url.port_or_known_default() {
            parts.push(format!("{host}:{port}"));
        }
        if url.path() != "/" {
            parts.push(url.path().to_owned());
        }
        let segments = url.path().split('/');
        let values = url.query().unwrap_or_default().split(['&', '=']);
        parts.extend(
            segments
                .chain(values)
                .filter(|part| part.len() >= MIN_KEY_LEN)
                .map(str::to_owned),
        );
        parts.retain(|part| !part.is_empty());
        parts.sort_by_key(|part| Reverse(part.len()));
        parts
    }
}

impl fmt::Debug for RpcEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcEndpoint")
            .field("scheme", &self.scheme())
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl TryFrom<String> for RpcEndpoint {
    type Error = RpcEndpointError;

    fn try_from(raw: String) -> Result<Self, RpcEndpointError> {
        let url = url::Url::parse(&raw)?;
        let transport = match url.scheme() {
            "http" | "https" => RpcTransport::Http,
            "ws" | "wss" => RpcTransport::WebSocket,
            other => {
                return Err(RpcEndpointError::UnsupportedScheme {
                    scheme: other.to_owned(),
                });
            }
        };
        // `sha256:<hex>`, so the hex starts after the scheme.
        let digest = ContentDigest::of_bytes(raw.as_bytes()).to_string();
        let fingerprint = digest.chars().skip("sha256:".len()).take(8).collect();
        Ok(Self {
            url,
            transport,
            fingerprint,
        })
    }
}

impl TryFrom<&str> for RpcEndpoint {
    type Error = RpcEndpointError;

    fn try_from(raw: &str) -> Result<Self, RpcEndpointError> {
        Self::try_from(raw.to_owned())
    }
}

/// Why a `rpc_url` refused at load.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RpcEndpointError {
    /// Refused before any scheme check.
    #[error("not a valid URL: {0}")]
    Parse(#[from] url::ParseError),
    /// Parsed, but the scheme selects no [`RpcTransport`]. The message does
    /// not print it: a URL written without one, such as `host:8545`, parses
    /// with the host as its scheme.
    #[error("unsupported or missing scheme: expected http(s) or ws(s)")]
    UnsupportedScheme {
        /// As written.
        scheme: String,
    },
}

pub(super) fn resolve_chains(
    raw: HashMap<String, RawChainConfig>,
) -> Result<HashMap<Chain, ChainConfig>, EngineConfigError> {
    let mut chains = HashMap::with_capacity(raw.len());
    for (key, cfg) in raw {
        let Ok(chain) = key.parse::<Chain>() else {
            return Err(EngineConfigError::InvalidChainKey { key });
        };
        if cfg.request_timeout_secs == 0 {
            return Err(zero_field(&format!("chains.{key}.request_timeout_secs")));
        }
        if cfg.max_log_range_blocks == 0 {
            return Err(zero_field(&format!("chains.{key}.max_log_range_blocks")));
        }
        let rpc_url = RpcEndpoint::try_from(cfg.rpc_url)
            .map_err(|source| EngineConfigError::InvalidRpcUrl { key, source })?;
        chains.insert(
            chain,
            ChainConfig {
                rpc_url,
                request_timeout_secs: cfg.request_timeout_secs,
                max_log_range_blocks: cfg.max_log_range_blocks,
            },
        );
    }
    Ok(chains)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scheme_less_host_port_refuses_as_an_unsupported_scheme() {
        // `localhost:8545` parses: `localhost` is the scheme, `8545` the
        // opaque path. The refusal therefore names the scheme; it is not
        // a parse error, and the operator-facing message must say so.
        let err = RpcEndpoint::try_from("localhost:8545").expect_err("scheme-less must refuse");
        assert!(
            matches!(
                err,
                RpcEndpointError::UnsupportedScheme { ref scheme } if scheme == "localhost"
            ),
            "{err:?}",
        );
    }

    /// Every part carries its own marker, so no assertion passes by chance.
    const MARKED: &str = "wss://user-marker:pass-marker@host-marker.example:8546\
         /v2/path-marker-key?apikey=query-marker-key#frag-marker";
    const MARKERS: [&str; 7] = [
        "user-marker",
        "pass-marker",
        "host-marker",
        "8546",
        "path-marker",
        "query-marker",
        "frag-marker",
    ];

    fn marked() -> RpcEndpoint {
        RpcEndpoint::try_from(MARKED).expect("the marked URL parses")
    }

    #[test]
    fn a_scheme_less_refusal_does_not_print_the_host_it_took_for_a_scheme() {
        let err = RpcEndpoint::try_from("host-marker.example:8545").expect_err("must refuse");
        let text = err.to_string();
        assert!(!text.contains("host-marker"), "{text}");
        assert!(text.contains("expected http(s) or ws(s)"), "{text}");
    }

    #[test]
    fn the_fingerprint_is_the_sha256_prefix_of_the_url_as_configured() {
        // `printf %s "$URL" | sha256sum | cut -c1-8`.
        assert_eq!(marked().fingerprint(), "2c97d561");
        let local = RpcEndpoint::try_from("http://localhost:8545").expect("parses");
        assert_eq!(local.fingerprint(), "a82b65c3");
        assert_eq!((marked().scheme(), local.scheme()), ("wss", "http"));
    }

    #[test]
    fn debug_prints_the_scheme_and_fingerprint_and_no_part_of_the_url() {
        let debug = format!("{:?}", marked());
        for marker in MARKERS {
            assert!(!debug.contains(marker), "Debug prints {marker}: {debug}");
        }
        assert!(
            debug.contains("wss") && debug.contains("2c97d561"),
            "{debug}"
        );
    }

    #[test]
    fn redact_removes_the_url_whole_and_in_parts() {
        let endpoint = marked();
        let cases = [
            format!("error sending request for url ({MARKED})"),
            // As an HTTP client prints it: no userinfo.
            "error sending request for url (wss://host-marker.example:8546\
             /v2/path-marker-key?apikey=query-marker-key#frag-marker)"
                .to_owned(),
            "certificate not valid for name \"host-marker.example\"".to_owned(),
            "connect to host-marker.example:8546 refused".to_owned(),
            "The requested URL /v2/path-marker-key was not found on this server".to_owned(),
            "no such key: path-marker-key".to_owned(),
            "rejected ?apikey=query-marker-key".to_owned(),
            "invalid key query-marker-key".to_owned(),
            "basic auth for user-marker with pass-marker refused at #frag-marker".to_owned(),
        ];
        for text in cases {
            let out = endpoint.redact(&text);
            for marker in MARKERS {
                assert!(!out.contains(marker), "{marker} survives in {out:?}");
            }
            assert!(out.contains(REDACTED), "{out}");
        }
    }

    #[test]
    fn redact_leaves_unrelated_text_and_short_path_segments_alone() {
        let text = "eth_getLogs over v2: HTTP error 503 with body: try again in 4ms";
        assert!(matches!(marked().redact(text), Cow::Borrowed(t) if t == text));
    }
}
