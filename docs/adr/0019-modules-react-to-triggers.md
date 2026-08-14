---
status: accepted
supersedes: 0016-component-vocabulary.md (in part)
amends: 0014-local-store-durability-model.md, 0018-one-operator-policy-surface.md
---

# Modules react to triggers

> Amendment: this record was edited after acceptance.
> [ADR-0020](0020-retire-component-kind.md) retired the manifest `kind` field and so discharged the `module` versus `service` half of the ADR-0016 spelling deferral that the Supersession section below leaves open.
> The sentence carries the mark in place.

## Context

Five words cover the reactive path today.
Counts over `crates`, `wit` and `docs`: watch 442, subscription 362, subscribe 161, dispatch 457, trigger 17, on_event 25, on-event 13.
A module author declares a `[[subscription]]`, the host opens an "event source", dispatches an "event" to `on-event`, and the operator tunes an unrelated thing called watch.
The repository already spells the kind vocabulary three times: the `nexum_world::SubscriptionKind` variants, the serde tags of the manifest parser's `CoreSubscription`, and the `event_kind` metric label.
The label copy is untyped literals at the dispatch call sites, and it never carries `cron`, because cron subscriptions are inert.
That duplication is the generator of the drift, not a symptom of it.

## Decision

Three words, and each owns one closed vocabulary.

The merge criterion: use one word when one closed vocabulary is named the same way at every layer a reader crosses, and use two words when a reader must ask which layer they are on.
The naive rule "two concepts that are not one-to-one cannot share a word" is wrong and self-violating, because a declaration produces N occurrences and this record merges those anyway.

### Trigger

A trigger is why a module ran.
The word covers the kind, the manifest declaration of one, and one delivered occurrence.
Every layer names a kind with one string: the core set `block`, `chain-log` and `cron`, plus the kinds the composition root's extensions declare.
The vocabulary is open across deployments and closed for one composition root, because the load path refuses an undeclared extension kind.
Every layer spells each member the same way, so a reader never has to ask which layer holds the name.

The polarity: a trigger is the reason a module ran, never a lever the host pulls.
SQL puts the polarity the other way: there the trigger is the reacting procedure.
This codebase carried that polarity in `TaskManager::trigger()`, which returned a `ShutdownTrigger` you `.fire()`.
The rename to `shutdown_signal()` and `ShutdownSignal` cleared the word.
The operator prior art matches the chosen polarity: Chainlink Automation names log triggers and time-based upkeeps, and Gelato names block and event triggers.

### Source

A source is the live upstream the host opens, reconnects and backfills.
A source is never a trigger, because the two are not in bijection.
One block stream per chain fans out to every module that declared a block trigger on it.
One trigger kind is fed by two transports, `eth_subscribe(newHeads)` over WS and polling over HTTP.

Source is minted as a reserved word at the host layer, and it is not free today.
It forces four rewrites, which are rewrites and not exemptions:

- `nexum-sdk/src/keeper.rs` documents `Poller` as "A source of conditional commitments ... the source owns its own wire".
  That is a guest-side per-commitment upstream, not the host layer, and the rustdoc rewrites without the word.
- `host/logs/mod.rs` has `LogSource`, which names the four capture points a log record can come from: the host `logging` interface, stdout, stderr, and the panic record the supervisor synthesizes.
  Its snake_case variant names are a live tracing `source` field, so the rename changes observable log output.
  It would sit beside a chain-log source at the host layer, so it renames.
- `test_utils/manifest.rs` has the public `ManifestSource`, which names where a test manifest comes from.
  That is the same origin-of-bytes sense at the host layer, so it renames.
- `host/extension.rs` has `EventSources`, which is in the reserved sense but carries the retired `event` adjective, so it renames in the `event` sweep.

`source` is also the thiserror error-cause field convention throughout this workspace.
A migration grep keys on identifiers and never on the bare word.

### Dispatch

Dispatch is the host act of entering one guest with one occurrence, under a deadline, a fuel grant and a rate limit.
It already has exactly one meaning and one enforcer, the supervisor's dispatch path.
Three operator keys govern one dispatch: `[limits.dispatch]` with `burst` and `refill_per_sec`, `policy.max_fuel_per_dispatch`, and `limits.event_deadline_secs`.
It is not part of the problem and it keeps its name.

### Exemptions

A rename sweep does not touch these:

- Solidity `event` and the manifest's `event_signature`, which carry the ABI sense.
- `tracing::Event` and `tracing::Subscriber`, which are ecosystem types.
- `tokio::sync::watch`, which is a library channel name.
- alloy's `watch_blocks_from` and `watch_canonical_logs_from`, which are upstream API.
- The keeper's durable key value `"watch:"`.
- The persisted cursor keys `last_dispatched_block:{chain_id}` and `chainlog_cursor:{keccak}`.
  Persisted bytes do not rename.

## Supersession

This record supersedes, in part, the closing consequence of [ADR-0016](0016-component-vocabulary.md): "The WIT worlds spell the worker kind `event-module`" with the alignment deferred.
The `event` adjective is now decided: the world spelling becomes `trigger-module` and the export becomes `on-trigger` when the WIT rename lands.
The `module` versus `service` half of that spelling deferral is not discharged here.
Renaming `world event-module` to `world trigger-module` swaps one adjective for another and still does not spell `module`, so that deferral stays open.
Discharged by [ADR-0020](0020-retire-component-kind.md): the manifest spells no kind, so there are no two spellings left to align.
ADR-0016 carries the mark in place, in its status line and on the affected consequence, as it already does for [ADR-0017](0017-capabilities-and-services.md).

[ADR-0018](0018-one-operator-policy-surface.md) is amended in place: `on_event` in its Capabilities text becomes `on-trigger`, marked in its amendment block.
[ADR-0014](0014-local-store-durability-model.md) is amended in place: both `on-event` mentions become `on-trigger`, marked in an amendment block.

## Rejected alternatives

- **Keep "subscription" for the declaration and "event" for the occurrence.**
  Two words for one closed vocabulary, and the reader must ask which layer they are on.
  The repository already spells the kind vocabulary under both words, and that duplication generated the drift.
- **"Two concepts that are not one-to-one cannot share a word" as the criterion.**
  Self-violating, because the declaration-to-occurrence relation is one-to-N and this record merges those.
- **Merge source into trigger.**
  Not a bijection: one source fans out to many triggers, and one trigger kind is fed by two transports.
- **The SQL polarity, where the trigger is the thing that reacts.**
  The operator prior art matches the chosen polarity, and the one in-repo use of the SQL polarity was already renamed away.
- **"Watch" as the umbrella word.**
  The word carries chain-specific connotations from watching blocks and logs, so it cannot name a generalized area.

## Consequences

- The reactive vocabulary is three words: a module declares a trigger, the host opens a source, and dispatch enters the guest.
- The renames this record fixes land in later code issues.
  This record lands alone and precedes all of them.
  - `[[subscription]]` becomes `[[trigger]]`.
  - `SubscriptionKind` becomes `TriggerKind`, and the manifest parser's `CoreSubscription` follows it.
  - The `event_kind` label becomes `trigger_kind`.
  - `on-event` becomes `on-trigger`.
  - `world event-module` becomes `world trigger-module`.
- A migration grep keys on identifiers, never on the bare words, because of the exemption list and the thiserror `source` convention.
