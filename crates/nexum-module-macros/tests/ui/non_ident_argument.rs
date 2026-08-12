//! An argument that is not even an identifier gets the grammar hint.

use nexum_module_macros::module;

struct Alerts;

#[module(42)]
impl Alerts {
    fn on_tick(_payload: u64) -> Result<(), ()> {
        Ok(())
    }
}

fn main() {}
