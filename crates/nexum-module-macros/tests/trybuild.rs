//! Pinned diagnostics for the rejected `#[module]` inputs.
//!
//! Each fixture under `tests/ui/` fails inside the macro itself, before
//! it reads `component.toml` or emits `wit_bindgen::generate!`, so the
//! `.stderr` files pin exactly the message a module author sees and the
//! fixtures stay free of manifest and wit-bindgen scaffolding.
//!
//! Accepted inputs are covered elsewhere. The unit tests in
//! `src/lib.rs` pin the accepted argument grammar and the glue the
//! macro emits for accepted inputs: the dispatch arms, the `init`
//! export, and the adapter binding. Five guest modules expand the
//! attribute during the CI wasm builds: `modules/example`, the three
//! under `modules/examples/`, and `modules/fixtures/topic-parity`.
//! The other guest modules call `wit_bindgen::generate!` directly.
//!
//! The `.stderr` text is toolchain-sensitive. The flake pins Rust
//! 1.94.0 exactly, and CI pins the "1.94" minor line, which floats
//! across patch releases, so a patch release can change the rendering
//! in CI before the flake moves. Regenerate with `TRYBUILD=overwrite`
//! when the toolchain moves on either side.

#[test]
fn rejected_inputs_have_pinned_diagnostics() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}
