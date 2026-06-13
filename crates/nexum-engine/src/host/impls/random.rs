//! `nexum:host/random`: fills `len` bytes from the OS CSPRNG.
//! Getrandom 0.4 failures are exceptionally rare on supported
//! platforms; on failure we return zero-filled bytes - guests that
//! need a strong-failure signal should use identity or chain primitives
//! instead.

use crate::bindings::nexum;
use crate::host::state::HostState;

impl nexum::host::random::Host for HostState {
    async fn fill(&mut self, len: u32) -> Vec<u8> {
        let mut buf = vec![0u8; len as usize];
        let _ = getrandom::fill(&mut buf);
        buf
    }
}
