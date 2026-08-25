use nexum_sdk::Level;
use nexum_sdk::host::{
    ChainError, ChainHost, EntryPage, Fault, ListQuery, LocalStoreHost, LoggingHost,
};

use crate::chain::MockChain;
use crate::local_store::MockLocalStore;
use crate::logging::MockLogging;

/// Composed in-memory host; each field is the per-seam mock. Every seam
/// answers whether or not the module declares it; `#[nexum_sdk::module]` is
/// the declaration check, and it does not reach `http`.
#[derive(Default)]
pub struct MockHost {
    /// `nexum:host/chain` mock.
    pub chain: MockChain,
    /// `nexum:host/local-store` mock.
    pub store: MockLocalStore,
    /// `nexum:host/logging` mock.
    pub logging: MockLogging,
}

impl MockHost {
    /// Fresh empty host.
    pub fn new() -> Self {
        Self::default()
    }
}

impl ChainHost for MockHost {
    fn request(&self, chain_id: u64, method: &str, params: &str) -> Result<String, ChainError> {
        self.chain.request(chain_id, method, params)
    }
}

impl LocalStoreHost for MockHost {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Fault> {
        self.store.get(key)
    }
    fn set(&self, key: &str, value: &[u8]) -> Result<(), Fault> {
        self.store.set(key, value)
    }
    fn delete(&self, key: &str) -> Result<(), Fault> {
        self.store.delete(key)
    }
    fn list_keys(&self, prefix: &str) -> Result<Vec<String>, Fault> {
        self.store.list_keys(prefix)
    }
    fn list_entries(&self, query: &ListQuery<'_>) -> Result<EntryPage, Fault> {
        self.store.list_entries(query)
    }
    fn contains(&self, key: &str) -> Result<bool, Fault> {
        self.store.contains(key)
    }
    fn len(&self, key: &str) -> Result<Option<u64>, Fault> {
        // Qualified: the mock's inherent `len` counts rows.
        LocalStoreHost::len(&self.store, key)
    }
    fn count(&self, prefix: &str) -> Result<u64, Fault> {
        self.store.count(prefix)
    }
}

impl LoggingHost for MockHost {
    fn log(&self, level: Level, message: &str) {
        self.logging.log(level, message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_host_dispatches_through_supertrait() {
        let host = MockHost::new();
        host.chain
            .respond_to("eth_blockNumber", "[]", Ok("\"0x1\"".into()));

        // Through the `Host` supertrait: every seam on one value.
        let _: &dyn nexum_sdk::host::Host = &host;
        host.set("key", b"val").unwrap();
        assert_eq!(host.get("key").unwrap().as_deref(), Some(&b"val"[..]));
        assert_eq!(host.request(1, "eth_blockNumber", "[]").unwrap(), "\"0x1\"");
        host.log(Level::INFO, "happy path");

        assert_eq!(host.chain.call_count(), 1);
        assert_eq!(host.logging.lines().len(), 1);
        assert_eq!(host.store.len(), 1);
    }
}
