---
status: accepted
supersedes: 0016-component-vocabulary.md (in part)
amends: 0017-capabilities-and-services.md, 0019-modules-react-to-triggers.md
---

# A component declares no kind

## Context

[ADR-0016](0016-component-vocabulary.md) gave the manifest a `[component].kind` field spelled `module` or `service`.
[ADR-0017](0017-capabilities-and-services.md) then settled that a service is a versioned WIT interface an untrusted guest exports, which the author claims with `provides` and the engine verifies against the component's real exports.
Under that model a component is not typed at all.
What it offers is per-interface, and the engine reads it from the exports.

Nothing in the runtime branches on the parsed kind.
`ComponentKind` was parsed, validated, and stored, and its only readers were the tests that asserted the round trip.
No manifest in the tree carries the field.
`provides` (#207) will restate, per interface, what the engine already verifies, so `kind` would be a second author-written statement of a fact the engine derives.
A fact the engine derives must not also be author-declared, because two sources can disagree and each disagreement forces a precedence rule, a refusal, or a silent winner.

## Decision

The manifest has no `kind` field.
A component states its identity and its dependencies, and nothing classifies the component itself.
`ComponentKind`, the `UnknownComponentKind` refusal, and the `unknown_component_kind` metric label are deleted.

A manifest that still carries `kind` refuses at load.
`ComponentSection` carries `deny_unknown_fields`, so the retired field fails deserialization and surfaces as `ParseError::Toml` naming the unknown field.
No alias, no deprecation window, and no accepted-but-ignored path exists: the project is pre-release, so the break is cost free.

`wit/nexum-host/query-module.wit` is deleted.
Nothing binds it: the host binds only the `nexum:host/event-module` world, and no Rust, TOML, manifest, or test names the file.
Git history preserves its shape for the M3 service work.

## Supersession

This record supersedes, in part, [ADR-0016](0016-component-vocabulary.md): its title, the "`kind` is `module` or `service`" rule and the examples that carry the field, and the two consequences that turn on the kind.
ADR-0016 carries the mark in place, in its status line, its amendment block, and on each affected statement.

[ADR-0019](0019-modules-react-to-triggers.md) held that "The `module` versus `service` half of that spelling deferral is not discharged here", because renaming the world still does not spell `module`.
This record discharges that remainder: the manifest spells no kind, so there are no two spellings left to align.
ADR-0019 carries the mark in an amendment block and on the affected sentence.

[ADR-0017](0017-capabilities-and-services.md) opens with "ADR-0016 settled that a component declares a kind and its dependencies".
That sentence now stands only for the dependencies half, and ADR-0017 carries the mark in an amendment block.
Its three concepts, host capability, extension capability, and service, are settled and this record does not reopen them.

The historical design document [capability-and-service-model.md](../design/capability-and-service-model.md) argued that `kind` is required with no default, and cited `query-module.wit` as the shape to generalize from.
Its header now marks both as retired, so the #207 plan must not reintroduce the field.

## Rejected alternatives

- **Keep `kind` beside `provides`.**
  A service that omits `kind = "service"`, or a module that claims it, forces the engine to pick a winner, and either pick hides an author error.
- **Accept `kind` and ignore it, or keep a serde alias.**
  An accepted field reads as load-bearing and does nothing, which is exactly the silently wrong path a typed refusal exists to close.
  Pre-release, compatibility cruft costs more than the break it avoids.
- **A dedicated retired-field variant, as `EngineConfigError::RetiredKey` does for `engine.toml`.**
  That variant exists to name the `[policy]` replacement an operator must set ([ADR-0018](0018-one-operator-policy-surface.md)).
  A retired manifest field has no replacement to name, because `provides` states a different fact about a different subject.
  The unknown-field message already tells the author the only thing left to say.
- **Keep `query-module.wit` as a sketch for M3.**
  An unbound world drifts silently, because no test can fail on it.
  Git history is the cheaper archive.

## Consequences

- `unknown_component_kind` leaves the pinned `error_kind` label set.
  That set is an operator contract, and this retirement is a deliberate contract change.
- A stale manifest refuses under the `toml` error kind, which an operator cannot tell apart from a typo.
  The author-facing message names the field, so the cost falls on the reader of a metric rather than on the reader of the refusal.
- The guest build path does not see the manifest schema.
  `nexum-world` reads `component.toml` untyped, so a stale manifest still compiles its wasm and refuses only when the engine loads it.
- #207 adds `provides` and the verification of it.
  This record clears the field ahead of that work and adds nothing in its place.
