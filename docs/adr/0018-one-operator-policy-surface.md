---
status: accepted
---

# One operator policy surface

> Amendment: this record was edited after acceptance.
> The per-dispatch fuel key was renamed, and the Decision text below carries the current name, `max_fuel_per_dispatch`.
> The `[limits]` subsections and `event_deadline_secs` stay in `[limits]`; this record does not fix their names, and later config work may rename them.

## Context

Six planned dials each need a per-component operator control: a ceiling source, a digest pin, a log bucket, a batch cap, a capability allowlist, and an egress allowlist.
Without one surface, each dial invents its own section and its own absent-record behaviour.
There was also no aggregate cap.
The engine bounded each component's memory alone, so N in-ceiling components could exhaust the host together.
ADR-0001 fixes the trust direction: the manifest requests, the engine grants, and a manifest value narrows an operator ceiling and never widens it.
ADR-0001 also requires that policy binding to a component keys on an identifier the operator writes.

## Decision

`engine.toml` carries one `[policy]` surface with three parts.

- `[policy]` holds the ceilings any component gets: `max_memory_bytes`, `max_fuel_per_dispatch`, `max_state_bytes`, the `capabilities` allowlist, and the `http_deny` range list.
- `[policy.total]` holds the aggregate cap on the summed memory reservations.
- `[policy.component.<id>]` holds targeted overrides, and each unset field falls back to `[policy]`.

Every `[[modules]]` entry carries a required, unique `id`.
That `id` is the `[policy.component]` key.
The author-supplied `[component].name` never binds policy, because an author rename must not select a different policy row.

### The clamp rule

A `[component.resources]` value narrows the component's `[policy]` ceiling and never widens it.
A request above the ceiling is capped to the ceiling and logged.
`[policy]` supersedes the three retired `[limits]` scalars `fuel_per_event`, `memory_bytes`, and `state_bytes`.
The `[limits]` subsections and `event_deadline_secs` stay in `[limits]`.
A retired key refuses at load with a message that names its `[policy]` replacement.

### The aggregate cap

Boot resolves each component's effective limits, sums the memory reservations in declaration order, and refuses the set when the sum crosses `[policy.total].max_memory_bytes`.
The refusal names the entry that crossed the cap.
The check bounds declared reservations, not measured usage.
It fails closed by capacity, not by enumeration: a component the operator never named gets the `[policy]` defaults and still counts against the total.

### Capabilities

The effective permitted set for a component is the `capabilities` list of its `[policy.component]` row, else the `[policy]` list, else every capability the runtime supports.
Every `[dependencies]` key the manifest declares must be in the permitted set, or the component refuses at boot.
The component's imports are already checked against the declared set, so the imports cannot exceed the operator grant either.
A block or chain-log subscription delivers chain data through `on_event` without an import, so it also refuses when the permitted set excludes `chain`.
The grant is whole or the component does not boot, per ADR-0001.

### Egress

The effective host set is the author's `[dependencies.http].hosts`, intersected with `[policy.component.<id>].http_allow` where a row is present, minus `[policy].http_deny`.
The gate admits a host only when it matches both name lists, so neither file can widen past the other.
`http_deny` holds address ranges and is applied to the connect destination after every allow, including `[limits.http].permit_destinations`.

### Absent-record defaults

- Absent `[policy]`: the previous `[limits]` defaults apply.
- Absent `[policy].capabilities`: every capability the runtime supports is permitted.
  Failing closed on absence would break every existing deployment on upgrade.
- Absent `[policy.total]`: the sum is unbounded.
- Absent `[policy.component.<id>]`: the component gets the `[policy]` defaults.
- A `[policy.component]` key that matches no `[[modules]].id` refuses at load, because an unapplied narrowing row fails open.

## Consequences

- This is one breaking config change: every `[[modules]]` entry needs an `id`, and the three `[limits]` scalars move to `[policy]`.
  `deny_unknown_fields` makes a second change a second hard boot failure, so the surface lands whole.
- The dial issues consume this surface instead of adding their own sections.
- `[[services]]` entries gain an `id` and join `[policy.component]` when the service load path lands.
- Every load-time refusal goes through the validated `TryFrom` conversion with a typed error, never through a serde string.

## Alternatives rejected

**Key policy on `[component].name`.**
The name is author-supplied, so an author could choose which policy row applies to them.
This is the exact violation ADR-0001's closing consequence forbids.

**Fail closed on an absent `capabilities` list.**
Safe-looking, but it turns an upgrade into an outage for every deployment that never wrote the key, and the capability enforcement against declared imports already bounds the exposure.

**Retire `[[services]].http_allow` into a global CIDR denylist alone.**
That moves a per-service hostname decision out of the trusted file and gives nothing back that can express a hostname.
The per-component `http_allow` row keeps a hostname-level operator allowlist.

**Check the aggregate cap against ceilings at config parse.**
The declared reservation is the effective limit after the manifest clamp, which is only known once manifests load.
A parse-time check over ceilings would refuse sets that fit.
