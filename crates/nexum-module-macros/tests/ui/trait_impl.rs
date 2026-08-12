//! The attribute rejects a trait impl: only an inherent impl carries
//! the handler set.

use nexum_module_macros::module;

struct Alerts;

trait Handler {}

#[module]
impl Handler for Alerts {}

fn main() {}
