//! `nexum:host/local-store`: redb backend with host-side namespacing.

use crate::bindings::HostError;
use crate::bindings::nexum;
use crate::host::error::internal_error;
use crate::host::state::HostState;

impl nexum::host::local_store::Host for HostState {
    async fn get(&mut self, key: String) -> Result<Option<Vec<u8>>, HostError> {
        self.store
            .get(&self.module_namespace, &key)
            .map_err(|err| internal_error("local-store", err.to_string()))
    }

    async fn set(&mut self, key: String, value: Vec<u8>) -> Result<(), HostError> {
        self.store
            .set(&self.module_namespace, &key, &value)
            .map_err(|err| internal_error("local-store", err.to_string()))
    }

    async fn delete(&mut self, key: String) -> Result<(), HostError> {
        self.store
            .delete(&self.module_namespace, &key)
            .map_err(|err| internal_error("local-store", err.to_string()))
    }

    async fn list_keys(&mut self, prefix: String) -> Result<Vec<String>, HostError> {
        self.store
            .list_keys(&self.module_namespace, &prefix)
            .map_err(|err| internal_error("local-store", err.to_string()))
    }
}
