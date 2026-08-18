//! `nexum:host/local-store`: the lattice's store handle, namespaced host-side.

use crate::bindings::nexum;
use crate::bindings::nexum::host::local_store::{KeyValue, WriteOp};
use crate::bindings::nexum::host::types::Fault;
use crate::host::component::{self, RuntimeTypes, StateHandle, StoreError};
use crate::host::state::HostState;

impl<T: RuntimeTypes> HostState<T> {
    fn store_fault(&self, verb: &'static str, err: StoreError) -> Fault {
        crate::host::error::store_fault(&self.run.module, verb, err)
    }
}

impl<T: RuntimeTypes> nexum::host::local_store::Host for HostState<T> {
    async fn get(&mut self, key: String) -> Result<Option<Vec<u8>>, Fault> {
        self.store.get(&key).map_err(|e| self.store_fault("get", e))
    }

    async fn set(&mut self, key: String, value: Vec<u8>) -> Result<(), Fault> {
        self.store
            .set(&key, &value)
            .map_err(|e| self.store_fault("set", e))
    }

    async fn delete(&mut self, key: String) -> Result<(), Fault> {
        self.store
            .delete(&key)
            .map_err(|e| self.store_fault("delete", e))
    }

    async fn list_keys(&mut self, prefix: String) -> Result<Vec<String>, Fault> {
        self.store
            .list_keys(&prefix)
            .map_err(|e| self.store_fault("list-keys", e))
    }

    async fn contains(&mut self, key: String) -> Result<bool, Fault> {
        self.store
            .contains(&key)
            .map_err(|e| self.store_fault("contains", e))
    }

    async fn len(&mut self, key: String) -> Result<Option<u64>, Fault> {
        self.store.len(&key).map_err(|e| self.store_fault("len", e))
    }

    async fn count(&mut self, prefix: String) -> Result<u64, Fault> {
        self.store
            .count(&prefix)
            .map_err(|e| self.store_fault("count", e))
    }

    async fn apply(&mut self, ops: Vec<WriteOp>) -> Result<(), Fault> {
        let ops: Vec<component::WriteOp> = ops
            .into_iter()
            .map(|op| match op {
                WriteOp::Set(KeyValue { key, value }) => component::WriteOp::Set { key, value },
                WriteOp::Delete(key) => component::WriteOp::Delete { key },
            })
            .collect();
        self.store
            .apply(&ops)
            .map_err(|e| self.store_fault("apply", e))
    }
}
