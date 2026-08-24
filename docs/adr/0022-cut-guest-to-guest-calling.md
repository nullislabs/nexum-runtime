---
status: accepted
supersedes: 0021-provides-and-implements.md, 0016-component-vocabulary.md (in part)
amends: 0017-capabilities-and-services.md, 0018-one-operator-policy-surface.md, 0020-retire-component-kind.md
---

# Guest-to-guest calling is cut and the single-store model stands

> Amendment: [ADR-0025](0025-the-required-digest-is-the-operator-pin.md) redefined what `[engine].require_component_digest` requires, from the author pin to the operator pin this record moved onto `[[modules]].digest`.
> `DigestPolicy`'s third field is `require_operator`, not `require_author`.
> The two-independent-pins decision this record states is unchanged.

## Context

[ADR-0021](0021-provides-and-implements.md) shipped `[component].provides` verification and `[implements]` authorization for a service edge no artifact ever took (#253).
No component the toolchain builds can satisfy a `provides` claim: `synthesize` in `crates/nexum-world` emits one world with func exports only, `export init` and `export on-event`, never an interface instance, and `enforce_provides` accepted only an interface-instance export.
Zero in-tree manifests declared `provides` and zero configs declared `[implements]`.

The runtime serves two products, a daemon runtime and a wallet plugin engine, and in both the caller is host Rust.
A decoder plugin feeding a risk plugin is cascading host-to-guest over a host-defined interchange type, not a guest-to-guest call.

## Decision

Guest-to-guest calling is removed from the runtime (#254).

Deleted:

- `[component].provides` from the manifest, raw and parsed, and `ParseError::InvalidInterfaceId`.
- The `engine.toml` `[implements]` table: `Implementer`, `resolve_implements`, the `implements` field on `EngineConfig`, and the `EngineConfigError` variants `InvalidInterfaceTrack`, `UnknownImplementsComponent` and `InvalidImplementerDigest`.
- `enforce_provides` and `enforce_implements` in the supervisor load path, with `LoadRefusal::{ProvidesNotExported, ImplementerUnbound, ImplementerUnpinned, ImplementerNotClaiming}`.
- The prepass duplicate-claim gate: `InterfaceLedger`, `claim_interface` and `BootRefusal::InterfaceClaimed`.
- `InterfaceId::matches_export`, whose only caller was `enforce_provides`.
- The `--wasm` override's `[implements]` clearing and `BootEnv.implements`.
- The `provides` test area and its component fixture.

Kept, deliberately:

- `InterfaceId` and `InterfaceTrack` in `crates/nexum-runtime/src/interface_id.rs`, less `matches_export`.
  A plugin registry must select the candidates for a slot before it reads any artifact bytes, and `InterfaceTrack`'s leading-zero rule decides whether a registry update stays inside the installed track (auto-installable) or leaves it and needs fresh user consent.
  The module rustdoc records this so the next reader does not delete it as an orphan.
- The digest machinery, strengthened as below.

## The operator pin moves to `[[modules]].digest` (#255)

`Implementer.digest` was the only operator-written artifact pin in trusted config, and it dies with `[implements]`.
The pin therefore moves: `ModuleEntry` gains `digest: Option<ContentDigest>`, parsed under the same strict grammar as the manifest pin.
`DigestPolicy` is unchanged in shape: `operator`, `author` and `require_author`, two independent pins, both verified against the exact bytes handed to the compiler, the operator's expectation reported first on a disagreement.
Amended by [ADR-0025](0025-the-required-digest-is-the-operator-pin.md): the third field is now `require_operator` and it mandates the pin this section moves.
`DigestPin::Operator`'s `Display` retargets from the `[implements]` digest to `[[modules]].digest in engine.toml`; the test that pins that wording changes with it.

This is a net security gain.
The old pin was reachable only for a `provides` claimant, which no artifact could be, so it was unreachable in practice.
The new pin is available for every module the operator configures.

## One component is one `Store`, re-grounded

ADR-0017 rejected `wac`-style composition on the argument that composed imports become the union of the parts.
That is true of `wac` but no longer inherent to the Component Model since `implements` and `external-id` merged, so the record must not rest on it.

The argument that holds is store-scoped enforcement:

- `ResourceLimiter` is store-level, and `memory_growing` receives no instance identity, so two components in one `Store` share one memory ceiling and neither refusal nor attribution can name the grower.
- Fuel and epoch deadlines are `Store` methods, so one component can drain the budget the operator set for another.
- `LinkerInstance::func_new` closures carry no caller identity, so a host capability granted to one component in a shared store is callable by its neighbour.

None of that is fixable by any composition mode, because the enforcement seams are per-`Store` by wasmtime's design.

Conceded: WASI Virt's attenuation is structural and does hold against a malicious guest, because the attenuated import is absent from the composed graph rather than wrapped.
It covers import-shaped capabilities only, it is compile-time and per-graph, and it touches no resource or lifecycle control, so it does not answer the store-scoped list above.
The concession does not weaken the decision; it bounds it.

Rejected: same-`Store`, multiple separately-instantiated components, the option between full composition and one-component-one-store.
It has the same fuel, memory and poison problem, because all three hang off the `Store`.

## Records settled (#256)

- ADR-0021 is superseded in full by this record; its whole Decision is the `provides` and `[implements]` pair.
- ADR-0017 is amended.
  Its deletion of the extension-installed component path (#233) was argued partly on the replacement this record removes, and now stands on its independent merits: an unused parallel admission path, a second registry, and the shadowing defect #204 reported.
  A host-calls-guest seam is expected to return for the plugin engine, and adding one is not re-litigating that deletion.
  Its `wac` alternative carries a mark in place: the union-of-imports ground is superseded, and the rejection stands on the store-scoped grounds above.
- ADR-0018 is amended in its Consequences: the digest-pin dial lands as `[[modules]].digest` on the entry it pins rather than under `[policy.component]`, and the clause that `[[services]]` entries join the policy surface is retired with the service load path.
- ADR-0020 is amended in its Context: the retirement of `[component].kind` stands on the ground that nothing branched on the parsed kind, not on `provides` restating anything.
- ADR-0016's clause that a dependency names another component's service is retired; a dependency names a host capability.

## Consequences

- Six labels leave the closed `error_kind` set: `invalid_interface_id`, `interface_claimed`, `provides_not_exported`, `implementer_unbound`, `implementer_unpinned`, `implementer_not_claiming`.
  That set is an operator contract, and this is a deliberate contract change the pinned-set test now enforces in its reduced form.
- `EngineConfig` carries `deny_unknown_fields`, so a stale `[implements]` table refuses at parse as a TOML unknown-key error an operator cannot distinguish from a typo.
  The message names the table.
  A `RetiredKey` refusal pointing at `[[modules]].digest`, the pin's new home, exists for the retired `[limits]` scalars and was considered here.
  It is not added because zero configs ever declared `[implements]`: no buildable component could satisfy a `provides` claim, so the shim would guide no deployment.
  The operator handbook names the pin's new home instead.
- A stale `[component].provides` line refuses the same way, from the manifest side.
- The operator handbook and the packaging guide drop `[implements]` and repoint the digest guidance at `[[modules]].digest`.
