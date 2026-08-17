---
status: accepted
supersedes: 0016-component-vocabulary.md (in part)
---

# A capability is host-implemented, a service is guest-implemented

> Amendment: this record was edited after acceptance.
> [ADR-0020](0020-retire-component-kind.md) retired the `[component].kind` field.
> The Context sentence below, "a component declares a kind and its dependencies", now stands only for the dependencies half.
> The three concepts this record settles are unchanged.
> [ADR-0021](0021-provides-and-implements.md) fixes the shapes the Decision below leaves open: the `provides` grammar, the track-keyed `[implements]` row with its digest, and the refusals each gap raises.
> Its second amendment (#205) also fixes the consumer's spelling, which the Decision and the first Consequence below leave open: the consumer names the interface as the `interface` value of a `[dependencies]` entry, under an author-chosen alias key, and the value is a compatibility track rather than a full version.

## Context

[ADR-0016](0016-component-vocabulary.md) settled that a component declares a kind and its dependencies.
It also decided that a dependency names a host capability or another component's service, through one list, and that "naming a service and naming a host capability read the same, because the engine already treats them the same".

That last sentence is the error this record corrects.

The engine treating them the same is not evidence that they are the same.
It is the conflation.
A host capability is a WIT interface the runtime implements in Rust and links into a component's world at build time.
A service is a WIT interface an untrusted guest component exports, which another component imports and calls across a store boundary.
They differ in who writes the implementation, when it is bound, and what can go wrong.

The vocabulary drifted accordingly.
`SERVICE_CAPABILITIES` in `manifest/capabilities.rs` is `[Cap::Chain, Cap::Logging]`, which are host capabilities under a name belonging to the guest concept.
`Extension` carries both a `service` returning a host-side Rust object and a `provider` installing a guest component, two unrelated things one word apart.

## Decision

There are three concepts.

**Host capability.**
A WIT interface the runtime implements, under a namespace the runtime owns.
Bound into a component's world at build time by world synthesis.
An author declares it by name in `[dependencies]`.
An operator bounds it with a dial.

**Extension capability.**
The same thing, with the Rust written by a composition root behind the `Extension` seam, and its rows registered at boot rather than compiled into `CORE`.
An author declares it and an operator bounds it exactly as for a host capability.
The difference is who wrote the Rust, and it is invisible to the author.

**Service.**
A versioned WIT interface exported by an untrusted guest component.
The author claims it with `provides`, and the engine verifies the claim against the component's real exports.
A consumer names the interface in `[dependencies]`.
An operator authorizes which component may implement it.

A capability is trusted because the operator chose the binary that implements it.
A service is untrusted because an author wrote it, so the operator authorizes the binding instead.

## What this deletes

A fourth thing exists in the code today and is not a concept.

`ServiceKind` is a guest component that an extension installs as the implementation of its own capability, held behind an empty `HostService` marker and called from host Rust.
That is a service whose consumer happens to be the host rather than another guest.

Who calls an exported interface is a wiring fact, not a type.
The operator already records it, in the `[implements]` binding.

So `ServiceKind`, `ServiceInstance`, `HostService`, `Extension::provider`, `Extension::service`, `ServiceKinds` and the parallel admission path they drive are removed.
An extension that needs a guest component to implement its capability declares that interface and lets the operator bind a service to it, through the same mechanism every other service edge uses.

The one thing the deleted path can do that a service edge cannot is carry a resource handle, because a trampoline marshals plain data between two stores.
The `nexum:host` WIT is deliberately resource-free, so nothing is lost.
An interface that needs a real handle needs host Rust, and that is a host capability by definition.

## Consequences

- A dependency on a capability and a dependency on a service no longer read the same.
  A capability is named; a service is named by its interface id and version.
  ADR-0016's line to the contrary no longer holds.
- `Cap` splits: the vocabulary the runtime owns is not the vocabulary an extension contributes, and neither is a service.
- The engine gains one mechanism for guest-implemented interfaces and loses two.
- An extension author writes a WIT interface and Rust that consumes it, rather than a `ServiceKind` implementation.
- Breaking the `Extension` trait, `engine.toml` and the WIT is permitted.
  The project is pre-release, there is no compatibility to preserve, and carrying the conflation is more expensive than breaking.

## Alternatives rejected

**Rename the fourth concept and keep it.**
This is what naming it `backend` would do.
It preserves a parallel admission path, a second install mechanism and a second registry, to avoid a break that costs nothing to take.

**Keep one dependency list for capabilities and services, per ADR-0016.**
An author asking for `chain` asks the runtime for something an operator dials.
An author asking for another component's interface asks for a wiring decision an operator authorizes.
One table cannot make that difference legible, and the engine must not treat the two the same.

**`wac`-style composition instead of a trampoline.**
Rejected previously and still rejected.
One component is one `Store`, and the memory ceiling, fuel, local-store namespace, restart window and capability grant all hang off that.
Composed, a plugin with no chain grant is indistinguishable from a module that has one.
