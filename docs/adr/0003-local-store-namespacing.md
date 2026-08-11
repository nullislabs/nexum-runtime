---
status: accepted
---

# `local-store` namespaces each module by a deterministic hash prefix

## Context

`nexum:host/local-store` is one key-value store shared by every module the runtime runs.
Two modules that use the same key string must see different values.
One module must never read or overwrite another module's data.

The runtime knows each module's identity when it instantiates the module, so namespacing is a host-side concern.
The prefix must be deterministic, and a module must not be able to choose another module's prefix.

## Decision

One redb database file at `EngineConfig.engine.state_dir`, and one shared table.
The host composes every key it gives to redb as the namespace prefix followed by the raw key bytes.

The prefix is `keccak256` of the module name.
keccak256 shares the domain of the ENS namehash.
A module that runs locally and is later published under an ENS name can therefore keep its state through an alias registered at migration.
The alias mechanism is out of scope here.

A module sees plain key strings on both paths.
The prefix is invisible to the WIT API.

## Consequences

- The prefix has a fixed size and does not depend on key length.
  A module's `list-keys` iterates the prefix range, and the host removes the prefix before it returns keys to the guest.
- A change to the prefix derivation orphans every module's stored state.
  The derivation therefore stays stable through 0.x.
  ENS-mode namespacing is added through the alias mechanism and not by a change to existing prefixes.
- The store does not version values.
  A module that needs a schema migration puts its own version marker in the stored payload and migrates on `init`.
- The module name comes from the module manifest, which the module author writes.
  Two modules that claim the same name conflict at boot instead of sharing state, so a rename cannot take over another module's namespace.
