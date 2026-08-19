---
status: accepted
---

# Blocks are clocks: logs replay, blocks do not, and host keys leave the module namespace

## Context

After a successful dispatch the supervisor persisted two kinds of key into the dispatched module's own local-store namespace.
`last_dispatched_block:<chain_id>` recorded the last block the module saw, and `chainlog_cursor:<hex>` recorded the resume position of a `resume = true` event trigger.
The event path read its cursor back at boot and backfilled the gap.
Nothing ever read the block marker back, on any path.

Both keys lived inside the namespace that the module's `max_state_bytes` quota measures, so the quota charged the author for host bytes, and an author-chosen key could collide with a host key.
The failure mode was asymmetric in the other direction: the host wrote through an unquotaed handle, so a host write was never refused, but the bytes it left behind entered the author's quota accounting and could refuse an author write that the author's own data admitted.

## Decision

The replay asymmetry is deliberate, and it is the decision under everything else in this record.
An outage backfills logs and does not replay blocks.
A log carries content that exists exactly once, so a module that missed it can only get it from a backfill, and the host keeps a durable cursor to drive one.
A block trigger is a clock, so a module doing periodic work wants the current head rather than five hundred historical ticks, and the source reopens at head.

The host therefore provides no gap-detection primitive for block triggers.
The host keeps no block progress marker, because a write without a reader records nothing.
A module that cares about block gaps keeps its own marker, under a key it chose, with semantics it chose, inside its own quota.

The chain-log cursor is then the only host-written key, and it lives outside the module's namespace.
For a module named `m` the host opens a second store handle under the namespace `host/m`, derived with the same keccak256 prefix rule as every other namespace.
`ModuleId` refuses any name containing `/`, so no author-supplied module name can equal a `host/` namespace: the exclusion is structural, not probabilistic.
The derivation is injective, so the host namespaces of two modules can only collide when the module names already do, which boot refuses.
The host handle carries no quota, because the store's quota is per handle.
`max_state_bytes` therefore measures the author's data alone: the cursor lives in another namespace, so its bytes never enter the author's quota accounting.

## Rejected alternatives

- **A host-provided gap primitive for block triggers.**
  Catch-up semantics are per module: one module wants every missed height, another wants only the latest, a third wants a bounded window.
  A host primitive would fix one policy for all of them, and the module can already build any of these from a marker it owns.
- **Replaying missed blocks the way logs are replayed.**
  A block is a clock tick, not content; replaying an outage hands a periodic module hundreds of stale ticks it must then discard itself.
- **Keeping the marker unread as an operator inspection aid.**
  A write without a reader is untested surface, and it charges the author's quota for host bytes.
- **A reserved key prefix for host keys inside the module's namespace.**
  Collision avoidance by convention only, the quota still measures host bytes, and the guest can still read, overwrite, or delete the host's keys through the local-store interface.
- **A quota on the host handle.**
  The quota exists to bound author data, and the host's footprint is one eight-byte value per resume trigger, so a cap would bound nothing worth bounding and only add a refusal path to the one write the host must make.

## Consequences

- No host write lands in a module's namespace, and a key an author picks cannot collide with a host key.
- The guest cannot read, overwrite, or delete the host's cursor, because the local-store interface only reaches the module's own namespace.
- `max_state_bytes` measures the author's data alone, and a host cursor write can never cause an author write to be refused.
- A module that wants block-gap detection keeps its own marker; the supervisor keeps none.
- The `last_dispatched_block` item in the rename-exemption list of [ADR-0019](0019-modules-react-to-triggers.md) is discharged, because the key no longer exists.
  `chainlog_cursor:<hex>` keeps its spelling and only changes namespace, so its exemption stands.
- Cursors persisted under the old layout are not read and not migrated, because nothing is published or tagged.
  On an existing development state directory a `resume = true` source opens at head once and re-persists under the host namespace from the next successful dispatch.
- The operator-visible store layout in `docs/production.md` follows this record.
