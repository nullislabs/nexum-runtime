# The linker extension seam

## What

The core host binds the `nexum:host/event-module` world: the `nexum:host` interfaces (chain, identity, local-store, remote-store, logging) plus the allowlisted `wasi:http` outgoing surface.
A domain capability is not a core seam.
It plugs into the host through an extension assembled at the composition root, so the core runtime compiles and runs with no domain backend at all and no extension registered.

## The `Extension` trait

One trait, `Extension<T: RuntimeTypes>` in `crates/nexum-runtime/src/host/extension.rs`, is what a domain contributes.
Its members:

- `namespace()`: the namespace it owns.
- `capabilities() -> NamespaceCaps`: the `{ prefix, ifaces }` merged into enforcement, so a component importing its interfaces still validates.
- `link(&mut Linker<HostState<T>>)`: adds its WIT imports to each worker linker, after the core interfaces and before instantiation.
  It takes only `&mut Linker` and never the wasmtime `Store`, which is not `Sync`, so the seam stays compatible with a future per-extension call router that serializes access to a `Store`.
- `attach_clock(Arc<dyn HostWallClock>)`: receives the effective host wall clock once per launch, before `link`.
  The clock is the WASI override's wall clock when a test sets one, else the real host clock, so extension time and guest time share one source.
- `manifest_sections`, `admit_worker`: the non-core manifest sections it claims and its install-time predicate over them.
  An `Err` refuses the install fail-fast.
- `subscriptions`, `events`: the manifest subscription kinds it emits and the event sources it opens once the engine is booted.

An extension defines its own `bindgen!` for its world, which generates a `Host` trait local to the extension, and implements it for the foreign `HostState<T>`.
That is orphan-legal, because the trait is local.
The bindgen shares `nexum:host/types` with the core bindings through `with`, so the extension's `fault` is the same type the core host constructs.

## Registration and enforcement

`CapabilityRegistry` starts from the core namespace (`nexum:host/`) and registers each extension's namespace.
`enforce_capabilities` and manifest name validation both consult it.
The composition root assembles the `Vec<Arc<dyn Extension<T>>>` once and threads it through the runtime builder (`with_extensions`), which builds the linker and the registry from it.
The supervisor caches the list, so the restart path rebuilds an identical linker.
Two wired extensions claiming one namespace is a boot refusal, `ExtensionNamespaceClaimed`.

An extension lives in its own crate.
It depends on the runtime for the seam types (`HostState`, `Extension`, the `nexum:host/types` bindgen), and the composition-root binary depends on it.
The runtime carries no dependency on any extension crate, so a domain cone stays out of the bare engine.
`crates/nexum-cli` composes the core lattice and registers no extension.

## Extension config

`engine.toml` stays domain-free.
The engine deserializes every `[extensions.<name>]` table into an opaque `toml::Value` (`EngineConfig::extensions`) and never interprets it.
The composition root hands each extension its own entry to parse.

Two different files use the `[extensions.<name>]` heading, and they are unrelated.
The `engine.toml` table above is operator config, read at runtime and left opaque.
A composition root's `extensions.toml` is WIT wiring: each row carries `import` and `packages`, `nexum_world::manifest_extensions` parses it, and the `#[module]` macro reads it at expansion time through an ancestor walk.
See issue #42.

## Normative rule: import narrowing and boot ordering

A component built through `#[nexum_sdk::module]` compiles against a per-component world derived from its manifest's `[dependencies]`.
A component that never declares an extension capability therefore has no such import, and boots with a core-only linker by construction.
A component that does import an extension interface instantiates only if, before instantiation:

- the extension's linker hook is registered, else instantiation fails with an unsatisfied-import error, AND
- the extension's capability namespace is registered, else the manifest's declaration of that capability is rejected as unknown.

The linker hook and the capability namespace of an extension MUST therefore be registered as a pair, from the same `Extension` value, before any component is instantiated.
Registering one without the other is a boot-time failure, not a compile-time one.
