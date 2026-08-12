//! `subscribes()` with no events is rejected: an empty list would pin
//! nothing while looking like it does.

use nexum_module_macros::module;

struct Alerts;

#[module(subscribes())]
impl Alerts {
    fn on_chain_logs(_payload: u64) -> Result<(), ()> {
        Ok(())
    }
}

fn main() {}
