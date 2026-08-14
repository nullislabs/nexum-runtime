use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use nexum_sdk::host::{Fault, RemoteStoreHost};
use nexum_sdk::prelude::{Address, B256, keccak256};

/// In-memory [`RemoteStoreHost`]: `keccak256`-addressed blobs plus
/// mutable `(owner, topic)` feeds. Feed writes land under the mock's own
/// owner ([`set_owner`](Self::set_owner), zero by default).
#[derive(Default)]
pub struct MockRemoteStore {
    blobs: RefCell<HashMap<B256, Vec<u8>>>,
    feeds: RefCell<HashMap<(Address, B256), Vec<u8>>>,
    owner: Cell<Address>,
    fault: RefCell<Option<Fault>>,
}

impl MockRemoteStore {
    /// Set the owner feed writes land under.
    pub fn set_owner(&self, owner: Address) {
        self.owner.set(owner);
    }

    /// Seed a blob directly; returns its reference.
    pub fn seed_blob(&self, data: impl Into<Vec<u8>>) -> B256 {
        let data = data.into();
        let reference = keccak256(&data);
        self.blobs.borrow_mut().insert(reference, data);
        reference
    }

    /// Seed another owner's feed.
    pub fn seed_feed(&self, owner: Address, topic: B256, data: impl Into<Vec<u8>>) {
        self.feeds.borrow_mut().insert((owner, topic), data.into());
    }

    /// Inject a fault every subsequent operation returns.
    pub fn fail_with(&self, fault: Fault) {
        *self.fault.borrow_mut() = Some(fault);
    }

    /// Number of stored blobs.
    pub fn blob_count(&self) -> usize {
        self.blobs.borrow().len()
    }

    fn check_injected_fault(&self) -> Result<(), Fault> {
        match self.fault.borrow().as_ref() {
            Some(fault) => Err(fault.clone()),
            None => Ok(()),
        }
    }
}

impl RemoteStoreHost for MockRemoteStore {
    fn upload(&self, data: &[u8]) -> Result<B256, Fault> {
        self.check_injected_fault()?;
        Ok(self.seed_blob(data))
    }

    fn download(&self, reference: B256) -> Result<Vec<u8>, Fault> {
        self.check_injected_fault()?;
        self.blobs
            .borrow()
            .get(&reference)
            .cloned()
            .ok_or_else(|| Fault::Unavailable(format!("MockRemoteStore: no blob at {reference}")))
    }

    fn read_feed(&self, owner: Address, topic: B256) -> Result<Option<Vec<u8>>, Fault> {
        self.check_injected_fault()?;
        Ok(self.feeds.borrow().get(&(owner, topic)).cloned())
    }

    fn write_feed(&self, topic: B256, data: &[u8]) -> Result<B256, Fault> {
        self.check_injected_fault()?;
        let reference = self.seed_blob(data);
        self.feeds
            .borrow_mut()
            .insert((self.owner.get(), topic), data.to_vec());
        Ok(reference)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_store_round_trips_content_addressed_blobs() {
        let store = MockRemoteStore::default();
        let reference = store.upload(b"chunk").unwrap();
        assert_eq!(reference, keccak256(b"chunk"));
        assert_eq!(store.download(reference).unwrap(), b"chunk");
        assert_eq!(store.blob_count(), 1);

        let missing = store.download(B256::from([0xCC; 32])).unwrap_err();
        assert!(matches!(missing, Fault::Unavailable(ref m) if m.contains("MockRemoteStore")));
    }

    #[test]
    fn remote_store_feeds_are_owner_scoped() {
        let store = MockRemoteStore::default();
        let owner = Address::from([0xAA; 20]);
        let topic = B256::from([0x11; 32]);

        // Writes land under the mock's own owner and stay downloadable.
        store.set_owner(owner);
        let reference = store.write_feed(topic, b"v1").unwrap();
        assert_eq!(store.download(reference).unwrap(), b"v1");
        assert_eq!(
            store.read_feed(owner, topic).unwrap().as_deref(),
            Some(&b"v1"[..])
        );

        // Another owner's feed is a distinct slot.
        let other = Address::from([0xBB; 20]);
        assert_eq!(store.read_feed(other, topic).unwrap(), None);
        store.seed_feed(other, topic, b"theirs");
        assert_eq!(
            store.read_feed(other, topic).unwrap().as_deref(),
            Some(&b"theirs"[..]),
        );
    }

    #[test]
    fn remote_store_fault_injection_covers_every_operation() {
        let store = MockRemoteStore::default();
        store.fail_with(Fault::Timeout);
        assert!(matches!(store.upload(b"x").unwrap_err(), Fault::Timeout));
        assert!(matches!(
            store.download(B256::ZERO).unwrap_err(),
            Fault::Timeout,
        ));
        assert!(matches!(
            store.read_feed(Address::ZERO, B256::ZERO).unwrap_err(),
            Fault::Timeout,
        ));
        assert!(matches!(
            store.write_feed(B256::ZERO, b"x").unwrap_err(),
            Fault::Timeout,
        ));
    }
}
