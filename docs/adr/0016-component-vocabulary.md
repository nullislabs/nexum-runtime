---
status: superseded in part by 0017-capabilities-and-services.md
---

# A component declares a kind and its dependencies

## Context

[ADR-0001](0001-operator-config-separate-and-trusted.md) settles that the manifest requests and the engine grants.
The manifest does not say so.

Nothing in a key name carries the direction.
`[capabilities]` reads as a possession, not a request.
`[module.resources]` reads as a property, not a ceiling the engine may lower.

The other direction is worse.
A component that offers a service to other components declares it through `[module].kind`, a field whose default is `event-module`.
Nothing says that the component provides anything.

The vocabulary also disagrees with itself.
`engine.toml` says adapters, the manifest says kind, and the code says provider, adapter, and worker.
A reader has to learn that four words name two roles.

One thing already works the way the new names describe.
World synthesis takes a single declared list holding both core host capabilities and extension-provided ones, and refuses a collision between them.
A module already depends on a host capability and on another component's service through the same list.

## Decision

A component is the unit the runtime loads.
That is the vocabulary the WASM Component Model already gives us, and the runtime is a Component Model host.

A component declares what it is, and what it depends on.

```toml
[component]
name    = "http-probe"
version = "0.1.0"
kind    = "module"
digest  = "sha256:..."

[dependencies]
logging     = {}
chain       = {}
http        = { hosts = ["api.cow.fi"] }
acme-status = {}
```

`kind` is `module` or `service`.
A module consumes.
A service also registers a name other components may depend on.

```toml
[component]
name = "acme-status"
kind = "service"
```

A dependency names a host capability or another component's service.
The engine resolves the name against the core capability table first, then the registered services.
The two sets may not collide, which world synthesis already enforces.

A dependency is a table, so an attribute belongs to the thing it qualifies.
The outbound HTTP allowlist becomes `hosts` on the `http` dependency rather than a separate section.
An empty table is the common case and carries no attributes.

The manifest file is `component.toml`, and `[[adapters]]` in `engine.toml` becomes `[[services]]`.
`ProviderKind`, `ProviderManifest`, `Role::Adapter` and `ComponentKind::Worker` take the same two words.

## Rejected alternatives

- **`[requests]` or `[requires]` for the consuming side.**
  Accurate about direction and wrong about the concept.
  Dependency already names the relationship, and a reader knows it from every package manager.
- **Keeping `[capabilities]` and renaming the key under it.**
  The section name is the part that misleads.
- **`module` as both the umbrella and one of the two kinds.**
  Shorter, and it makes every sentence about modules ambiguous.
- **A bare list of dependency names.**
  Leaves the HTTP allowlist in a section apart from the dependency it qualifies, which is the split this decision exists to remove.
- **A Cargo-style version value per dependency.**
  Familiar, but no host capability has a version, so the value would be decorative.

## Consequences

- Every manifest changes, and so do videre and shepherd.
  This is a schema break, and the manifest schema version records it.
- The direction of every key is legible.
  A component states its identity, its kind, and what it depends on, and nothing else in the file is a demand on the engine.
- Naming a service and naming a host capability read the same, because the engine already treats them the same.
  Superseded by [ADR-0017](0017-capabilities-and-services.md): the engine treating them the same was the conflation, not evidence they are alike.
- `nexum-module-macros` and the `#[module]` attribute keep their names.
  The macro serves the module kind, so the name stays accurate.
- The WIT worlds spell the worker kind `event-module`.
  Aligning those spellings is a separate WIT change and is not carried here.
