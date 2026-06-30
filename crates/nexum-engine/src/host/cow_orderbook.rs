//! `shepherd:cow/cow-api` backend.
//!
//! Two responsibilities:
//!
//! 1. `request` - generic REST passthrough. Module gives the HTTP
//!    method, path (relative to the chain's orderbook base URL), and
//!    optional JSON body. We dispatch via `reqwest`, return the
//!    response body verbatim.
//! 2. `submit_order` - typed submission. Module gives a JSON-encoded
//!    `cowprotocol::OrderCreation`; we parse, dispatch via
//!    `cowprotocol::OrderBookApi::post_order`, return the assigned
//!    `OrderUid` as a `0x`-prefixed hex string.
//!
//! Per-chain `OrderBookApi` instances are constructed once at engine
//! boot from the discriminated chain set in `cowprotocol::Chain`.
//! Chains the SDK does not know about return `Unsupported` at the
//! host call boundary.

use std::collections::BTreeMap;

use cowprotocol::{Chain, OrderBookApi, OrderCreation, OrderUid};
use thiserror::Error;

/// Process-wide pool of `OrderBookApi` clients keyed by EVM chain id.
#[derive(Debug, Clone)]
pub struct OrderBookPool {
    clients: BTreeMap<u64, OrderBookApi>,
    http: reqwest::Client,
}

impl Default for OrderBookPool {
    /// Build a pool covering every `cowprotocol::Chain` variant. Each entry
    /// uses the canonical `api.cow.fi/{slug}/api/v1` base URL from the SDK.
    /// Override individual entries via `OrderBookApi::new_with_base_url` for
    /// barn or staging targets.
    fn default() -> Self {
        let http = reqwest::Client::new();
        let chains = [
            Chain::Mainnet,
            Chain::Gnosis,
            Chain::Sepolia,
            Chain::ArbitrumOne,
            Chain::Base,
        ];
        let clients = chains
            .iter()
            .map(|c| (c.id(), OrderBookApi::new(*c)))
            .collect();
        Self { clients, http }
    }
}

impl OrderBookPool {
    /// Look up the client for a chain.
    pub fn get(&self, chain_id: u64) -> Result<&OrderBookApi, CowApiError> {
        self.clients
            .get(&chain_id)
            .ok_or(CowApiError::UnknownChain(chain_id))
    }

    /// REST passthrough. The base URL is whichever URL the pool's
    /// `OrderBookApi` client carries - overrides set via
    /// `OrderBookApi::new_with_base_url` (staging, wiremock) flow
    /// through here too, which keeps the passthrough and the typed
    /// `submit_order_json` path aimed at the same orderbook.
    pub async fn request(
        &self,
        chain_id: u64,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<String, CowApiError> {
        let api = self.get(chain_id)?;
        let base = api.base_url().clone();
        // `path` may or may not lead with a slash; `Url::join` handles
        // both, but we strip a single leading `/` so consumers can
        // write either `/orders/...` or `orders/...` interchangeably.
        let trimmed = path.strip_prefix('/').unwrap_or(path);
        let url = base
            .join(trimmed)
            .map_err(|e| CowApiError::BadPath(format!("{path:?}: {e}")))?;

        let request = match method.to_ascii_uppercase().as_str() {
            "GET" => self.http.get(url),
            "POST" => self.http.post(url),
            "PUT" => self.http.put(url),
            "DELETE" => self.http.delete(url),
            other => return Err(CowApiError::BadMethod(other.to_owned())),
        };
        let request = if let Some(body) = body {
            request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_owned())
        } else {
            request
        };

        let response = request.send().await.map_err(CowApiError::Network)?;
        // Surface the orderbook's structured 4xx / 5xx bodies verbatim
        // so the guest can decode `{"errorType": "...", "description":
        // "..."}` - projecting them into HostError here loses the
        // detail the guest needs to recover.
        let text = response.text().await.map_err(CowApiError::Network)?;
        Ok(text)
    }

    /// Typed submission. `body` is the JSON encoding of
    /// `cowprotocol::OrderCreation`. The chain's orderbook validates
    /// `from`, the EIP-712 hash, and (if `Eip1271`) the contract
    /// signature; we return whatever UID it assigns.
    pub async fn submit_order_json(
        &self,
        chain_id: u64,
        body: &[u8],
    ) -> Result<OrderUid, CowApiError> {
        let creation: OrderCreation = serde_json::from_slice(body).map_err(CowApiError::Decode)?;
        let api = self.get(chain_id)?;
        let uid = api.post_order(&creation).await?;
        Ok(uid)
    }
}

#[derive(Debug, Error)]
pub enum CowApiError {
    #[error("unknown chain {0} (no cowprotocol::Chain variant)")]
    UnknownChain(u64),
    #[error("bad HTTP method `{0}` (expected GET/POST/PUT/DELETE)")]
    BadMethod(String),
    #[error("invalid path: {0}")]
    BadPath(String),
    #[error("network: {0}")]
    Network(#[from] reqwest::Error),
    #[error("decode OrderCreation JSON: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("orderbook: {0}")]
    Orderbook(#[from] cowprotocol::Error),
}

#[cfg(test)]
mod tests;
