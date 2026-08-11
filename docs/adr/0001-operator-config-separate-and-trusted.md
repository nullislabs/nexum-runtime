---
status: accepted
---

# Operator config is separate from the module manifest, and only the operator config is trusted

## Context

The runtime reads two configuration files with different owners and different change cadences.
The operator writes `engine.toml` at deployment time.
The module author writes `module.toml` at build time.
A capability declaration is a property of the build, so it belongs in the published bundle and not in the operator's file.

The original decision recorded that split.
It did not record a trust direction.
No document stated that `module.toml` is untrusted input, so the engine honours the values the manifest declares.
`[module.resources]` was added after the original decision and was not recorded here.

## Decision

Two files, two schemas, two loaders.

`engine.toml` is operator-owned and trusted.
It sits beside the engine binary, or `--engine-config` names it.
It defines `[engine]`, `[limits]`, `[chains.<id>]`, `[extensions]`, `[[modules]]` and `[[adapters]]`.
`engine_config::EngineConfig::load` reads it.

`module.toml` is author-owned and untrusted.
It ships in the module bundle beside the `.wasm` component.
It defines `[module]`, `[module.resources]`, `[capabilities]` and `[config]`.
`manifest::load` reads it.

The engine config names each module's manifest path.
The two files never collapse into one.

### The manifest requests, the engine grants

`module.toml` states what the module wants.
`engine.toml` states what the engine provides.
The granted set is the intersection, and the engine computes it at boot.

The engine applies one of three rules to each manifest value.

- A resource value narrows an operator ceiling.
  It never widens one.
- A capability or HTTP host is granted only if the operator also permits it.
- A self-declared name or digest is evidence of intent.
  It is not evidence of authorization.

The module author controls the content of `module.toml`.
Any value the engine honours verbatim is therefore a value the module author controls.

### A request is granted whole, or the module does not boot

The engine grants every capability and host the module requests, or it refuses the module at boot.
There is no partial grant and no degraded mode.

A module therefore never needs to ask what it holds.
What it requested and what it holds are the same set, or it is not running.

A partial grant was considered and rejected.
It costs a host interface for the guest to read its grant, a fault case for calling an ungranted capability, and a branch in every module, and it describes a state the operator can avoid by fixing the configuration.

## Consequences

- A deployment needs both files.
  A missing `engine.toml` gives no chains and the default `state_dir`.
  Chain-backed capabilities then report `unsupported`.
- The component digest in `[module]` proves that the artifact matches what the author published.
  It does not prove that the operator authorized the artifact, because one party writes both the hash and the bytes it covers.
  An operator-side pin closes that gap.
- An absent `[capabilities]` block grants nothing.
  The 0.1-compat fallback that treated every linked capability as required is removed.
- A module bundle carries `module.toml` with the artifact.
  Engines ship no manifest templates.
- Policy that binds to a module must key on an identifier the operator writes.
  A key taken from `[module].name` is author-controlled, so a rename can miss the intended policy row.
