use std::cell::RefCell;
use std::collections::HashMap;

use nexum_sdk::host::{ChainError, ChainHost, Fault};

/// In-memory [`ChainHost`] over a `(method, params)` response map;
/// records every call.
#[derive(Default)]
pub struct MockChain {
    responses: RefCell<HashMap<(String, String), Result<String, ChainError>>>,
    calls: RefCell<Vec<ChainCall>>,
}

/// One recorded [`MockChain::request`] invocation.
#[derive(Clone, Debug)]
pub struct ChainCall {
    /// EVM chain id the guest passed.
    pub chain_id: u64,
    /// JSON-RPC method name.
    pub method: String,
    /// JSON-encoded params array (verbatim).
    pub params: String,
}

impl MockChain {
    /// Program the response for `(method, params)`; overwrites any prior entry.
    pub fn respond_to(
        &self,
        method: impl Into<String>,
        params: impl Into<String>,
        result: Result<String, ChainError>,
    ) {
        self.responses
            .borrow_mut()
            .insert((method.into(), params.into()), result);
    }

    /// All calls received, in arrival order.
    pub fn calls(&self) -> Vec<ChainCall> {
        self.calls.borrow().clone()
    }

    /// Last call received, if any.
    pub fn last_call(&self) -> Option<ChainCall> {
        self.calls.borrow().last().cloned()
    }

    /// Total call count.
    pub fn call_count(&self) -> usize {
        self.calls.borrow().len()
    }
}

impl ChainHost for MockChain {
    fn request(&self, chain_id: u64, method: &str, params: &str) -> Result<String, ChainError> {
        self.calls.borrow_mut().push(ChainCall {
            chain_id,
            method: method.to_string(),
            params: params.to_string(),
        });
        self.responses
            .borrow()
            .get(&(method.to_string(), params.to_string()))
            .cloned()
            .unwrap_or_else(|| {
                Err(ChainError::Fault(Fault::Unsupported(format!(
                    "MockChain: no response configured for {method} {params}"
                ))))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_records_calls_and_returns_programmed_response() {
        let chain = MockChain::default();
        chain.respond_to("eth_blockNumber", "[]", Ok("\"0x1234\"".into()));

        assert_eq!(
            chain.request(1, "eth_blockNumber", "[]").unwrap(),
            "\"0x1234\""
        );
        assert_eq!(chain.call_count(), 1);
        let last = chain.last_call().unwrap();
        assert_eq!(last.chain_id, 1);
        assert_eq!(last.method, "eth_blockNumber");
    }

    #[test]
    fn chain_unconfigured_method_returns_unsupported() {
        let chain = MockChain::default();
        let err = chain.request(1, "eth_call", "[]").unwrap_err();
        let ChainError::Fault(Fault::Unsupported(msg)) = err else {
            panic!("expected Unsupported fault, got {err:?}");
        };
        assert!(msg.contains("MockChain"));
        assert_eq!(chain.call_count(), 1);
    }
}
