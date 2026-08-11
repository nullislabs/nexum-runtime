---
status: accepted
---

# One host trait per seam, and a module binds only the seams it uses

## Context

The runtime needs a host abstraction that a module can test against without a `wasm32-wasip2` toolchain.
`wit_bindgen::generate!` emits types per cdylib, so one shared SDK type cannot cross the WIT boundary.
The mocks therefore live in their own crate and compile for the host target.

This decision replaces the trait-surface part of the pre-carve host-trait ADR.
The error envelope in that ADR is superseded by [ADR-0011](0011-per-interface-typed-errors.md).

## Decision

The SDK declares one trait per host seam, such as `ChainHost` and `LocalStoreHost`.

A module binds only the seams it uses.

```rust
pub fn on_block<H: ChainHost + LocalStoreHost>(host: &H, ...) -> Result<(), Fault>
```

Narrow bounds are the only form that compiles.
The `#[nexum_sdk::module]` macro emits a host adapter that implements exactly the traits for the capabilities the manifest declares.
A module that declares two capabilities therefore has an adapter carrying two trait impls, and a bound on any other seam does not resolve.

A sealed supertrait composes every seam.
It is not a module-facing bound, because no capability-gated adapter can satisfy it.
Its purpose is completeness: the mock host asserts that it implements the supertrait, so adding a seam without adding it to the mock fails to compile.

Tests inject the mock host, and the generated adapter serves the module at run time.

## Rejected alternatives

- **A module takes the composed supertrait.**
  Not implementable under capability gating.
  The adapter implements only the declared seams, so the supertrait is satisfied only by the mock, or by a module that declares every capability.
  The pre-carve ADR recorded this form, which predates capability-gated worlds.
- **Delete the supertrait.**
  Nothing binds on it, so it looks vestigial.
  It is kept because it forces a mock impl for every seam structurally, where per-seam assertions rely on someone remembering to add one.

## Consequences

- A module depends only on the seams it names, so a test mocks only the calls the module makes.
- A new seam adds a trait in the SDK and an impl on the mock.
  The supertrait makes the second half mandatory rather than conventional.
- Removing a seam removes it from the supertrait and from the mock.
- The adapter is generated per module from the manifest, so its imports equal its declarations by construction.
  The boot-time capability check remains the backstop for a hand-rolled module built against the full world.
