//! Nothing may follow the `sol_events(...)` list.

use nexum_module_macros::module;

struct Alerts;

#[module(sol_events(Transfer), extra)]
impl Alerts {
    fn on_event(_payload: u64) -> Result<(), ()> {
        Ok(())
    }
}

fn main() {}
