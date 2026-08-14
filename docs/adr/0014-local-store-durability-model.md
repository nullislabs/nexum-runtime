---
status: accepted
---

# Local store durability is per call, not per event

## Context

The `nexum:host/local-store` seam is call by call.
The redb backend commits each write in its own fsynced transaction, and no transaction spans an `on-event` or `init` call.

A handler that writes and then traps keeps every write that already returned.
This is true for a typed fault, a panic, fuel exhaustion, a deadline, and a process crash.
Earlier documentation claimed an implicit per-event write transaction that rolls back on a trap.
That transaction never existed.

The keeper journal reserves a marker before it submits to a venue, and commits after.
A reconcile pass at the top of a sweep re-posts a stranded reservation.
Its correctness needs the reserved marker, committed before the submit, to survive a trap during or after the submit.

Per-event rollback would erase that marker while the venue may already hold the order.
That produces silent divergence or a double submit.

## Decision

Per-call committed durability is the contract.

Every write is fsync-durable when it returns `Ok`.
Writes apply in program order, a module reads its own writes, and a trap freezes state at the last completed call and never rewinds it.
Dispatch is serialized through a single actor, so no observer sees a torn mid-event state.

Per-event atomicity is rejected in every form.

- A write transaction held across `on-event` locks the single shared write head across handler awaits, for as long as the dispatch deadline allows.
  Dropping it on the deadline leaks an open transaction while the host state survives, which freezes writes for every module through the poison-backoff window.
- A host-side staged overlay defers the reserved marker's durability past the wire send.
  Any escape hatch that writes through immediately then carries all the load-bearing writes, so the transaction protects nothing.
- A snapshot or an undo log is worse than both.

Atomicity across the boundary is impossible, because no store transaction undoes an order the venue already holds.

The one sanctioned atomicity scope is the opt-in batch verb, committed in a single synchronous host call.
A multi-key invariant uses the batch verb.
A write sequence that crosses an await or an external effect uses the journal, whose reliance on no rollback is load bearing and not a missing feature.
A logical change that spans keys without the batch verb orders its writes so that every prefix is a valid state, with the recoverable key written last.

## Consequences

- The store is a write-ahead intent log with key-value convenience.
  It is not a transactional database.
  Traps freeze state, they never rewind it.
- At-least-once effects composed with a per-venue idempotency key are effectively exactly-once at the venue.
  That is the strongest guarantee available across the boundary.
- Whole-event staging is rejected and not parked.
  Reopen it only on a measured need that redelivery and idempotence demonstrably fail to cover.
