//! The only recognized attribute argument is `subscribes(...)`.

use nexum_module_macros::module;

struct Alerts;

#[module(emits(Transfer))]
impl Alerts {
    fn on_tick(_payload: u64) -> Result<(), ()> {
        Ok(())
    }
}

fn main() {}
