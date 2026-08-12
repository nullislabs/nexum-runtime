//! An impl with no recognized handler is rejected: an all-no-op module
//! is a mistake, not a module.

use nexum_module_macros::module;

struct Alerts;

#[module]
impl Alerts {
    fn helper() {}
}

fn main() {}
