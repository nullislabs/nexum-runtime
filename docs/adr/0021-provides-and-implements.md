---
status: accepted
amends: 0017-capabilities-and-services.md
---

# A provides claim is verified by the engine and authorized by the operator

> Amendment: this record was edited after acceptance.
> The consumer side (#205) landed with a dependency that names only a compatibility track, so it carries no WIT for a structural comparison.
> The deferred structural check is re-pointed in place, from #205 to the call wiring (#206).
>
> Second amendment: the consumer grammar #205 landed is recorded here, since it is the claim's one consumer.
> A `[dependencies]` entry carrying `interface = "<track>"` depends on a provided interface; the key is the alias the author's own code calls, and the value goes through the same track derivation as the ledger and the `[implements]` key.
> An interface id in key position would need quoting because of `:` and `/`, and the first unquoted attempt yields a TOML column error that says nothing about interfaces.
> A bareword key still names a capability, resolved against the core capability table first, so a provided interface can never shadow a capability, and an alias equal to a capability name refuses.
> A bareword naming a provider component refuses with the corrected `interface` line, a track no loaded component provides refuses at boot blaming the consumer, and a component's own claim never satisfies its own dependency.
> Five more refusals join the closed `error_kind` set: `invalid_interface_track`, `alias_shadows_capability`, `dependency_names_component`, `interface_not_provided`, `self_interface_dependency`.
> An interface dependency is outside the `[policy].capabilities` allowlist; [ADR-0018](0018-one-operator-policy-surface.md) records why.

## Context

[ADR-0017](0017-capabilities-and-services.md) settled that a service is a versioned WIT interface an untrusted guest exports, that the author claims it with `provides`, and that the engine verifies the claim against the component's real exports.
[ADR-0020](0020-retire-component-kind.md) retired `[component].kind` on the strength of that model, with `provides` still in the future tense.
This record fixes the shapes `provides` and its authorization take now that both exist (#207).

The manifest is author-supplied and untrusted ([ADR-0001](0001-operator-config-separate-and-trusted.md)).
A claim can be false, and a true claim can still come from a component the operator never chose to trust with that interface.
Verification and authorization are therefore separate acts with separate owners.

## Decision

`[component].provides` is an optional full interface id with a full semver, for example `acme:pool/quoter@2.0.0`.
The WIT parser rejects a truncated version, so the manifest grammar does too.
A component that provides nothing is the common case and declares nothing.

The engine verifies the claim after compile and before instantiation, on the same seam as import enforcement.
Only an interface-instance export satisfies it: a bare func export under a matching name does not.
The export must name the same interface, on the same compatibility track, at a version no older than the claim.
A claim no export satisfies refuses with `provides_not_exported`.
The match is nominal, on name, kind and version.
No component in the engine holds the interface's WIT, so an empty instance under the claimed name passes.
Amended per the note above: the structural check arrives with the call wiring (#206), the first stage where a consumer's compiled artifact can carry the imported shape for the engine to compare against the provider's export.

Authorization is the `engine.toml` `[implements]` table, keyed on the interface's compatibility track.
The track is semver's compatibility range, which is leading-zero sensitive: the major at or above 1.0 (`@2`), `0.minor` below it (`@0.3`), and the full version below 0.1 (`@0.0.7`), because every `0.0.z` release is a distinct interface.
One derivation serves the track everywhere, so the ledger, the key and the export match cannot disagree about what is compatible.
The row's `component` value is the operator-written `[[modules]].id`, never `[component].name`, which is the ADR-0001 closing rule made mechanical.
The row's `digest` pins the implementer's artifact and is verified on the exact bytes handed to the compiler, ahead of the author's own pin.
The track key means a compatible provider release does not force an edit to trusted config; the digest, not the version, fixes the artifact.

The defaults fail closed.
A claimant with no row refuses with `implementer_unbound`.
A row bound to a different id refuses the claimant the same way, naming the bound id.
A row without a digest refuses with `implementer_unpinned`.
A row that names an entry whose manifest makes no matching claim refuses with `implementer_not_claiming`.
That last one is what keeps the two halves symmetric: the row's digest is the only operator-written pin on the artifact, so if an unmatched row went inert, deleting one line of the untrusted manifest would delete the operator's pin with it.
The `[implements]` row is therefore read from the entry's side as well as the claim's side, and a `provides` claim is never the trigger for consulting trusted config.
The `--wasm` override synthesizes its id from the file stem, which is author-controlled, so no `[implements]` row binds on that path and an override claimant always refuses ([ADR-0018](0018-one-operator-policy-surface.md)).

Two claimants of one track refuse in the prepass with `interface_claimed`, naming both artifact paths, before either artifact is read or compiled.
The prepass ledger and `[implements]` key on the track through one shared derivation, so the duplicate gate and the binding cannot disagree.

## Alternatives rejected

Keying `[implements]` on `[component].name`: the exact ADR-0001 violation an earlier draft made; the author would choose their own authorization key.
Keying `[implements]` on the full version: a patch release would force a trusted-config edit, and two in-track claimants would each match a distinct row, defeating the duplicate gate.
Accept-and-warn for an unbound or unpinned implementer: an authorization gap is not a warning.
Applying an unmatched row's digest silently instead of refusing: the row would then pin an artifact it does not authorize, and the operator would never learn that the component stopped implementing the interface they bound it for.

## Consequences

Until #205 lands, `provides` has no consumer; verification and authorization ship first so a false claim never enters the tree.
Verification proves self-consistency and authorization proves operator intent; neither proves that the exported interface behaves, which no boot-time check can.
The six refusals join the closed `error_kind` label set: `invalid_interface_id`, `interface_claimed`, `provides_not_exported`, `implementer_unbound`, `implementer_unpinned`, `implementer_not_claiming`.
A `digest_mismatch` names which pin failed, because the operator's row and the author's manifest now raise the same refusal from different files.
