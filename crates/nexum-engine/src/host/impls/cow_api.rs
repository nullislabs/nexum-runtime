//! `shepherd:cow/cow-api`: REST passthrough + typed `submit_order`.
//! Backend logic lives in [`crate::host::cow_orderbook`]; this is the
//! WIT-side error mapping.

use std::time::Instant;

use crate::bindings::nexum::host::types::HostErrorKind;
use crate::bindings::{HostError, shepherd};
use crate::host::cow_orderbook::CowApiError;
use crate::host::error::{internal_error, unimplemented};
use crate::host::state::HostState;

impl shepherd::cow::cow_api::Host for HostState {
    async fn request(
        &mut self,
        chain_id: u64,
        method: String,
        path: String,
        body: Option<String>,
    ) -> Result<String, HostError> {
        let start = Instant::now();
        tracing::debug!(chain_id, %method, %path, "cow-api::request");
        let result = match self
            .cow
            .request(chain_id, &method, &path, body.as_deref())
            .await
        {
            Ok(body) => Ok(body),
            Err(CowApiError::UnknownChain(id)) => Err(unimplemented(
                "cow-api",
                format!("chain {id} not in cowprotocol"),
            )),
            Err(CowApiError::BadMethod(m)) => Err(HostError {
                domain: "cow-api".into(),
                kind: HostErrorKind::InvalidInput,
                code: 0,
                message: format!("unsupported HTTP method: {m}"),
                data: None,
            }),
            Err(CowApiError::BadPath(msg)) => Err(HostError {
                domain: "cow-api".into(),
                kind: HostErrorKind::InvalidInput,
                code: 0,
                message: msg,
                data: None,
            }),
            Err(CowApiError::HttpError { status, body }) => Err(HostError {
                domain: "cow-api".into(),
                kind: HostErrorKind::Internal,
                code: status as i32,
                message: format!("HTTP {status}"),
                data: Some(body),
            }),
            Err(err) => Err(internal_error("cow-api", err.to_string())),
        };
        tracing::trace!(elapsed_ms = ?start.elapsed(), "cow-api::request done");
        result
    }

    async fn submit_order(
        &mut self,
        chain_id: u64,
        order_data: Vec<u8>,
    ) -> Result<String, HostError> {
        let start = Instant::now();
        tracing::debug!(chain_id, bytes = order_data.len(), "cow-api::submit-order");
        let result = match self.cow.submit_order_json(chain_id, &order_data).await {
            Ok(uid) => Ok(alloy_primitives::hex::encode_prefixed(uid.as_slice())),
            Err(CowApiError::UnknownChain(id)) => Err(unimplemented(
                "cow-api",
                format!("chain {id} not in cowprotocol"),
            )),
            Err(CowApiError::Decode(err)) => Err(HostError {
                domain: "cow-api".into(),
                kind: HostErrorKind::InvalidInput,
                code: 0,
                message: format!("invalid OrderCreation JSON: {err}"),
                data: None,
            }),
            Err(CowApiError::Orderbook(err)) => Err(HostError {
                domain: "cow-api".into(),
                kind: HostErrorKind::Denied,
                code: 0,
                message: err.to_string(),
                data: None,
            }),
            Err(err) => Err(internal_error("cow-api", err.to_string())),
        };
        tracing::trace!(elapsed_ms = ?start.elapsed(), "cow-api::submit-order done");
        result
    }
}
