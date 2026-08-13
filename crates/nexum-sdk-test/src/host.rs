use nexum_sdk::Level;
use nexum_sdk::host::{
    ChainError, ChainHost, Fault, IdentityHost, LocalStoreHost, LoggingHost, RemoteStoreHost,
};
use nexum_sdk::prelude::{Address, B256, Signature};

use crate::chain::MockChain;
use crate::identity::MockIdentity;
use crate::local_store::MockLocalStore;
use crate::logging::MockLogging;
use crate::remote_store::MockRemoteStore;

/// Composed in-memory host; each field is the per-seam mock.
#[derive(Default)]
pub struct MockHost {
    /// `nexum:host/chain` mock.
    pub chain: MockChain,
    /// `nexum:host/identity` mock.
    pub identity: MockIdentity,
    /// `nexum:host/local-store` mock.
    pub store: MockLocalStore,
    /// `nexum:host/remote-store` mock.
    pub remote_store: MockRemoteStore,
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

impl IdentityHost for MockHost {
    fn accounts(&self) -> Result<Vec<Address>, Fault> {
        self.identity.accounts()
    }
    fn sign(&self, account: Address, message: &[u8]) -> Result<Signature, Fault> {
        self.identity.sign(account, message)
    }
    fn sign_typed_data(&self, account: Address, typed_data: &str) -> Result<Signature, Fault> {
        self.identity.sign_typed_data(account, typed_data)
    }
}

impl RemoteStoreHost for MockHost {
    fn upload(&self, data: &[u8]) -> Result<B256, Fault> {
        self.remote_store.upload(data)
    }
    fn download(&self, reference: B256) -> Result<Vec<u8>, Fault> {
        self.remote_store.download(reference)
    }
    fn read_feed(&self, owner: Address, topic: B256) -> Result<Option<Vec<u8>>, Fault> {
        self.remote_store.read_feed(owner, topic)
    }
    fn write_feed(&self, topic: B256, data: &[u8]) -> Result<B256, Fault> {
        self.remote_store.write_feed(topic, data)
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
        assert!(host.accounts().unwrap().is_empty());
        let reference = host.upload(b"blob").unwrap();
        assert_eq!(host.download(reference).unwrap(), b"blob");
        host.log(Level::INFO, "happy path");

        assert_eq!(host.chain.call_count(), 1);
        assert_eq!(host.logging.lines().len(), 1);
        assert_eq!(host.store.len(), 1);
        assert_eq!(host.remote_store.blob_count(), 1);
    }
}
