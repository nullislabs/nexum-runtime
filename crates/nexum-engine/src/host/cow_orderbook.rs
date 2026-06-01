//! `shepherd:cow/cow-api` backend.
//!
//! Two responsibilities:
//!
//! 1. `request` — generic REST passthrough. Module gives the HTTP
//!    method, path (relative to the chain's orderbook base URL), and
//!    optional JSON body. We dispatch via `reqwest`, return the
//!    response body verbatim.
//! 2. `submit_order` — typed submission. Module gives a JSON-encoded
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

impl OrderBookPool {
    /// Build a pool covering every `cowprotocol::Chain` variant. The
    /// default `OrderBookApi::new(chain)` constructor uses the canonical
    /// `api.cow.fi/{slug}/api/v1` base URL from the SDK; callers that
    /// need barn or a custom staging URL override per chain.
    pub fn with_default_chains() -> Self {
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

    /// Look up the client for a chain.
    pub fn get(&self, chain_id: u64) -> Result<&OrderBookApi, CowApiError> {
        self.clients
            .get(&chain_id)
            .ok_or(CowApiError::UnknownChain(chain_id))
    }

    /// REST passthrough. The base URL is whichever URL the pool's
    /// `OrderBookApi` client carries — overrides set via
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
        // "..."}` — projecting them into HostError here loses the
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
        let uid = api
            .post_order(&creation)
            .await
            .map_err(|e| CowApiError::Orderbook(e.to_string()))?;
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
    #[error("orderbook rejected: {0}")]
    Orderbook(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn pool_indexes_default_chains() {
        let pool = OrderBookPool::with_default_chains();
        assert!(pool.get(1).is_ok(), "mainnet present");
        assert!(pool.get(100).is_ok(), "gnosis present");
        assert!(pool.get(11_155_111).is_ok(), "sepolia present");
        assert!(pool.get(42_161).is_ok(), "arbitrum present");
        assert!(pool.get(8_453).is_ok(), "base present");
    }

    #[test]
    fn unknown_chain_surfaces_typed_error() {
        let pool = OrderBookPool::with_default_chains();
        assert!(matches!(
            pool.get(99_999),
            Err(CowApiError::UnknownChain(99_999))
        ));
    }

    /// Build a pool whose Mainnet entry points at `mock.uri()`.
    /// `OrderBookApi::new_with_base_url` ships in cowprotocol; we
    /// rely on it so wiremock-driven tests can exercise the full
    /// request path without re-implementing the HTTP client.
    fn pool_with_mainnet_at(mock: &MockServer) -> OrderBookPool {
        let mut clients = std::collections::BTreeMap::new();
        clients.insert(
            Chain::Mainnet.id(),
            OrderBookApi::new_with_base_url(mock.uri().parse().expect("mock uri parses")),
        );
        OrderBookPool {
            clients,
            http: reqwest::Client::new(),
        }
    }

    #[tokio::test]
    async fn request_passes_get_path_through() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/version"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"version":"x.y.z"}"#))
            .expect(1)
            .mount(&mock)
            .await;

        let pool = pool_with_mainnet_at(&mock);
        let body = pool
            .request(Chain::Mainnet.id(), "GET", "/api/v1/version", None)
            .await
            .expect("request succeeds");
        assert_eq!(body, r#"{"version":"x.y.z"}"#);
    }

    #[tokio::test]
    async fn request_relative_path_works() {
        // Module passes a path without a leading slash. The
        // passthrough should still resolve against the orderbook
        // base URL.
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/native_price/0xabc"))
            .respond_with(ResponseTemplate::new(200).set_body_string("1.23"))
            .expect(1)
            .mount(&mock)
            .await;

        let pool = pool_with_mainnet_at(&mock);
        let body = pool
            .request(
                Chain::Mainnet.id(),
                "GET",
                "api/v1/native_price/0xabc",
                None,
            )
            .await
            .expect("relative path resolves");
        assert_eq!(body, "1.23");
    }

    #[tokio::test]
    async fn request_rejects_unknown_method() {
        let pool = OrderBookPool::with_default_chains();
        let err = pool
            .request(Chain::Mainnet.id(), "PATCH", "/x", None)
            .await
            .unwrap_err();
        assert!(matches!(err, CowApiError::BadMethod(_)));
    }

    #[tokio::test]
    async fn submit_order_propagates_orderbook_response() {
        let mock = MockServer::start().await;
        let body_json = sample_order_json();
        // cowprotocol POST /api/v1/orders returns the order UID
        // (56-byte hex) as a JSON string body.
        let returned_uid = format!("\"0x{}\"", "ab".repeat(56));
        Mock::given(method("POST"))
            .and(path("/api/v1/orders"))
            .respond_with(ResponseTemplate::new(201).set_body_string(returned_uid.clone()))
            .expect(1)
            .mount(&mock)
            .await;

        let pool = pool_with_mainnet_at(&mock);
        let uid = pool
            .submit_order_json(Chain::Mainnet.id(), body_json.as_bytes())
            .await
            .expect("submit succeeds");
        assert_eq!(uid.as_slice().len(), 56);
        assert_eq!(uid.as_slice(), &[0xab; 56]);
    }

    /// A minimal but accepted-by-cowprotocol OrderCreation JSON. We
    /// generate it inside the test so the JSON shape stays in lockstep
    /// with the published `cowprotocol` version.
    fn sample_order_json() -> String {
        use alloy_primitives::{Address, U256};
        use cowprotocol::OrderCreation;
        use cowprotocol::app_data::{EMPTY_APP_DATA_HASH, EMPTY_APP_DATA_JSON};
        use cowprotocol::order::{BuyTokenDestination, OrderData, OrderKind, SellTokenSource};
        use cowprotocol::signature::Signature;
        use cowprotocol::signing_scheme::SigningScheme;

        let order_data = OrderData {
            sell_token: Address::from([0x01; 20]),
            buy_token: Address::from([0x02; 20]),
            receiver: None,
            sell_amount: U256::from(100u64),
            buy_amount: U256::from(99u64),
            valid_to: u32::MAX,
            app_data: EMPTY_APP_DATA_HASH,
            fee_amount: U256::ZERO,
            kind: OrderKind::Sell,
            partially_fillable: false,
            sell_token_balance: SellTokenSource::Erc20,
            buy_token_balance: BuyTokenDestination::Erc20,
        };
        let signature = Signature::from_bytes(SigningScheme::PreSign, &[]).expect("presign empty");
        let creation = OrderCreation::from_signed_order_data(
            &order_data,
            signature,
            Address::from([0x03; 20]),
            EMPTY_APP_DATA_JSON.to_owned(),
            None,
        )
        .expect("valid OrderCreation");
        serde_json::to_string(&creation).expect("serialise OrderCreation")
    }
}
