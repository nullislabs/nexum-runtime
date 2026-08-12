//! The attribute rejects a generic impl: the export type must be
//! concrete.

use nexum_module_macros::module;

struct Alerts<T>(T);

#[module]
impl<T> Alerts<T> {
    fn on_tick(_payload: u64) -> Result<(), ()> {
        Ok(())
    }
}

fn main() {}
