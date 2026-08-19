use std::collections::HashMap;

use alloy_chains::Chain;
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
}

/// Raw `[chains.<id>]` shape; `rpc_url` stays a string until
/// [`resolve_chains`] validates it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawChainConfig {
    pub(super) rpc_url: String,
    #[serde(default = "default_chain_request_timeout_secs")]
    pub(super) request_timeout_secs: u64,
}

fn default_chain_request_timeout_secs() -> u64 {
    30
}

/// The transport a chain RPC endpoint's scheme selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcTransport {
    /// `http://` or `https://`: request/response only.
    Http,
    /// `ws://` or `wss://`: pubsub-capable.
    WebSocket,
}

/// Parsed once at load, so a malformed `rpc_url` refuses at boot rather
/// than at the first chain call. Deliberately not `Deserialize`: a
/// field-level serde refusal would bypass
/// [`EngineConfigError`](super::EngineConfigError).
#[derive(Debug, Clone)]
pub struct RpcEndpoint {
    url: url::Url,
    transport: RpcTransport,
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

    /// Credentials included; the dial path needs them.
    pub fn url(&self) -> &url::Url {
        &self.url
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
        Ok(Self { url, transport })
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
    /// Parsed, but the scheme selects no [`RpcTransport`].
    #[error("unsupported scheme {scheme:?}: expected http(s) or ws(s)")]
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
        let rpc_url = RpcEndpoint::try_from(cfg.rpc_url)
            .map_err(|source| EngineConfigError::InvalidRpcUrl { key, source })?;
        chains.insert(
            chain,
            ChainConfig {
                rpc_url,
                request_timeout_secs: cfg.request_timeout_secs,
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
}
