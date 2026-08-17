//! `sol_events()` with no events is rejected: an empty list would pin
//! nothing while looking like it does.

use nexum_module_macros::module;

struct Alerts;

#[module(sol_events())]
impl Alerts {
    fn on_event(_payload: u64) -> Result<(), ()> {
        Ok(())
    }
}

fn main() {}
