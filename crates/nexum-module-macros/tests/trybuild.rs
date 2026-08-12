//! Pinned diagnostics for rejected `#[module]` inputs.
//!
//! Every fixture fails inside the macro, before it reads `component.toml`
//! or emits `wit_bindgen::generate!`, so none carries manifest or bindgen
//! scaffolding. Accepted inputs are covered by the unit tests in `src/lib.rs`.
//!
//! `.stderr` is toolchain-sensitive, and the two pins differ: the flake
//! pins 1.94.0 exactly, CI pins the floating "1.94" line, so a patch
//! release can redden CI first. Regenerate with `TRYBUILD=overwrite`.

#[test]
fn rejected_inputs_have_pinned_diagnostics() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}
