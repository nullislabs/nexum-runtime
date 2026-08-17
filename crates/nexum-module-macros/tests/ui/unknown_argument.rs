//! The only recognized attribute argument is `sol_events(...)`.

use nexum_module_macros::module;

struct Alerts;

#[module(emits(Transfer))]
impl Alerts {
    fn on_schedule(_payload: u64) -> Result<(), ()> {
        Ok(())
    }
}

fn main() {}
