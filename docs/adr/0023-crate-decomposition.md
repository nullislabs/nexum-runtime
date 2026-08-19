---
status: accepted
---

# nexum-runtime decomposes into a layered crate set before the v1 tag

## Context

`nexum-runtime` is 23,330 lines behind fifteen public top-level modules, out of roughly 33,000 lines in the workspace.
The crate has no primitives layer and no traits layer.
A consumer that only parses a `component.toml` therefore links wasmtime, redb, alloy-provider, reqwest and wasmtime-wasi-http.

The v1 tag freezes this.
#145 publishes the workspace, and a crate split after publication is a breaking change for every consumer.
Before the tag the same split costs nobody anything.

The module graph was measured, not estimated.
The lowest layer has no internal dependencies at all: `module_id`, `interface_id`, `digest` and `host_pattern`.
`manifest` depends only on that layer.
`engine_config` depends on that layer and on two small value modules in `runtime`.
Three edges point the wrong way, and each is one import:

- `host/extension.rs` imports `supervisor::WasiClockOverride`.
- `engine_config/limits.rs` imports `runtime::dispatch_rate` and `runtime::poison_policy`.
- `error.rs` imports `builder::LaunchRefusal`.

No crate outside `nexum-runtime` names `ComponentManifest`, `ContentDigest` or `ModuleId` today.
The decomposition is therefore made for the frozen boundary, not for a consumer that is waiting on it.

reth and alloy are the precedent this workspace applies.
reth splits its surface across roughly sixty crates, keeps `reth-primitives-traits` deliberately dependency-light, and inverts dependencies through trait crates such as `reth-storage-api`.
alloy publishes a facade meta-crate that re-exports its sub-crates.
The trait layer is the part that carries the most weight here, because it is what lets an implementation crate avoid depending on a sibling implementation crate.

## Decision

`nexum-runtime` becomes a layered crate set before the v1 tag.

Layer 0, `nexum-primitives`.
It holds `module_id`, `interface_id`, `digest` and `host_pattern`, which is about 800 lines.
Its default dependencies are `const-hex`, `derive_more`, `semver`, `serde`, `sha2` and `thiserror`.
`digest` reaches hex through `const-hex` directly rather than through the `alloy-primitives` re-export of it, which keeps `DigestParseError::Hex` on the same error type without linking alloy.
`impl From<ModuleId> for metrics::SharedString` lives in this crate because the orphan rule allows it in no other, and it sits behind an off-by-default `metrics` feature that `nexum-runtime` enables.
The name has no `runtime` in it because the layer is not specific to the runtime product, in the manner of `alloy-primitives` under `alloy-*`.
`nexum-world` continues to sit at this level as the WIT and capability vocabulary.

Layer 1, `nexum-runtime-manifest` and `nexum-runtime-config`.
`nexum-runtime-manifest` holds the manifest layer, which is about 1,730 lines.
`nexum-runtime-config` holds `engine_config` and the two value modules that `engine_config` currently reaches upward for, which is about 2,500 lines.
The two value modules did not move whole: `dispatch_rate` splits, and only `DispatchRatePolicy` with its default constants moves down beside all of `poison_policy`.
`TokenBucket` stays in the engine because it holds a `tokio::time::Instant`, which keeps tokio off `nexum-runtime-config`.

Layer 2, `nexum-runtime-api`.
It holds traits and the types those traits name: `Extension`, `ExtensionError`, `RuntimeTypes`, `StateStore` and `WasiClockOverride`.
The move measured more residents than the seams name.
The generated `nexum:host` bindings live here, because `ExtensionDelivery` carries the host `Trigger`; `nexum-runtime` re-exports the one generated module, since a second `bindgen!` invocation would mint a distinct set of types.
The `ComponentBuilder` seam with `BuilderContext` lives here; the concrete builders stay above it.
The per-batch apply caps and the `BoxError` alias live beside the seams that name them.
The measured dependency set is `futures`, `nexum-tasks`, `thiserror`, `wasmtime` and `wasmtime-wasi`, plus `nexum-runtime-config` and `nexum-runtime-manifest`; `nexum-primitives` is not needed.
This layer is the reason the set works.
It gives an extension author one small crate to depend on instead of the engine.
The seams in this layer name no implementation type, which is what unpicks the `host` and `supervisor` knot.
`Extension::link` links against `RuntimeTypes::State`, and the store seam owns its error type, which the redb backend converts into.
`StoreError` carries no `non_exhaustive`, deliberately.
The engine projects each variant to a guest-visible fault, and the attribute would have forced a wildcard arm that sends a future variant to an internal fault with nothing failing to compile.
The seams must hold this shape before any code moves, because a seam that names a concrete type drags that type's crate with it.

Layer 3, the capability crates.
`nexum-runtime-store` holds the redb local store.
`nexum-runtime-chain` holds the provider pool.
`nexum-runtime-http` holds the outbound egress gate.
`nexum-runtime-logs` holds the module-log pipeline.
Each is a true sibling: it never depends on another of the four.
The store, chain and logs crates depend on `nexum-runtime-api`; the http gate needs no seam edge once the `WasiHttpView` impl stays with `HostState`.
`nexum-runtime-wasm` sits above them and holds the wasmtime embedding: `HostState`, the WIT `Host` impls, the component wiring, and the guest-fault funnel with its projections.
The `WasiHttpView` impl for `HostState` is orphan-pinned to the crate that owns `HostState`, so it stays with the embedding and not with the gate.
The measured embedding takes the api, chain, http and logs crates and not the store crate, because the store is reached only through the lattice's associated type.
`BuildError` loses `non_exhaustive` on the move, for the reason recorded for `StoreError` above: the facade routes on every variant, and a new slot must be a compile error there.
`impl From<PoolError> for ChainError` becomes an orphan once `PoolError` leaves the engine crate, so the projection is the free function `pool_fault` in the fault funnel, beside `store_fault`.
`ProviderPool::from_config` returns `Result<Self, PoolError>` rather than the composed `RuntimeError`, which can only live above every capability crate.

Layer 4, `nexum-runtime-supervisor`.
It holds the supervisor and the event loop, which is about 5,000 lines.

Layer 5, `nexum-runtime`.
It is the facade.
It holds the builder, the preset, the add-ons and the composed `RuntimeError`, and it re-exports the curated public surface.

Beside the stack, `nexum-runtime-metrics` holds the metric registry.
`nexum-runtime-testing` holds the test helpers that are a `test-utils` feature today, which is about 3,450 lines.
A separate crate keeps `tempfile`, `tower`, `alloy-json-rpc` and `metrics-util` off the runtime's dependency graph, and it prevents a downstream from enabling mocks in a production build.

The work is phased, and each phase is one pull request.
The bottom layers land first, because every later phase depends on them.

## The composed error moves to the facade

`RuntimeError` composes `BootRefusal`, `LoadRefusal`, `LaunchRefusal`, `CapabilityError`, `EngineConfigError`, `PoolError`, `BuildError` and `ExtensionError`.
It can only live above all of them, so it moves to the facade crate.

This does not undo the typed boundary that ADR-adjacent work put in place.
Each crate keeps the typed error it owns.
The facade keeps the composed value an embedder matches on.

## The engine and daemon seam

The runtime serves two products, a daemon runtime and a wallet plugin engine.
A wallet plugin engine runs wasm plugins, so it links wasmtime.

Which host capabilities it links after that is not known, and this record does not decide it.
It depends on the plugin type.
A plugin that renders a transaction for a human reads chain state, and ADR-adjacent research sets the shape of that access: a method allowlist with a per-invocation call budget, not a live provider handle.
A plugin that scores a transaction against a reputation service makes an outbound request, and per-plugin egress scope is a v1-or-never constraint.
A plugin that caches between invocations needs a store.
The capability set is therefore per-deployment and per-plugin, not a fixed subset of the daemon's.

What is daemon-shaped is narrower than the capability list.
It is the supervisor that reacts to chain triggers and drives long-lived modules, and the operator-facing Prometheus exporter.
A wallet embeds an engine and calls it per transaction, so it does not run that loop.

The seam is a separate question from this record, and it is a feature-flag question before it is a crate question.
The workspace already uses additive features.
This record names the seam so that a later reader does not derive it again, and it defers the decision to the plugin-engine work after v1.
The layer 3 boundaries above put each host capability in its own crate, so a deployment can select the set it grants rather than take the daemon's.

## Alternatives rejected

One crate, accepted permanently.
Rejected because the plugin engine and any registry or packaging tool would link the whole engine to read a manifest, and after publication that is not correctable without breaking consumers.

One crate with a curated facade, revisited after v1.
A facade that seals the module paths to `pub(crate)` does keep a later split non-breaking, because a re-export preserves both the path and the type identity.
This was rejected on the ground that the boundary enforces the dependency discipline now, while trusting a later self to split a 23,000 line crate does not.

A types-only split, one `nexum-runtime-types` crate.
Rejected because it draws one boundary and leaves the `host` and `supervisor` knot, and it leaves the `Extension` trait inside the engine crate, so an extension author still depends on everything.

## Consequences

- #145 publishes twelve crates in lockstep rather than one, and each carries the inherited SPDX identifier and MSRV.
- #260 designs the facade for the top of this stack rather than for one crate, so the two issues are sequenced and not parallel.
- The three reversed edges are corrected as part of the phase that moves the crate they block.
- `test-utils` stops being a feature on `nexum-runtime`, and a downstream test crate depends on `nexum-runtime-testing` instead.
- The workspace gains a crate-level dependency rule that a later reader can check mechanically: no crate depends on a crate in its own layer, except that layer 1 crates may depend on layer 0.
