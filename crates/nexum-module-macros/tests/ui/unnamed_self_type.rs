//! The attribute rejects a self type that is not a plain named path.

use nexum_module_macros::module;

struct Alerts;

#[module]
impl &Alerts {
    fn on_tick(_payload: u64) -> Result<(), ()> {
        Ok(())
    }
}

fn main() {}
