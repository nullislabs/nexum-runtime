//! Nothing may follow the `subscribes(...)` list.

use nexum_module_macros::module;

struct Alerts;

#[module(subscribes(Transfer), extra)]
impl Alerts {
    fn on_chain_logs(_payload: u64) -> Result<(), ()> {
        Ok(())
    }
}

fn main() {}
