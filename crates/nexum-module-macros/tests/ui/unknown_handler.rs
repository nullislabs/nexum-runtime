//! A typo'd `on_`-prefixed method is a compile error, not a silent
//! no-op helper.

use nexum_module_macros::module;

struct Alerts;

#[module]
impl Alerts {
    fn on_blocks(_payload: u64) -> Result<(), ()> {
        Ok(())
    }
}

fn main() {}
