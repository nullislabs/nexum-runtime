---
status: accepted
amends: 0019-modules-react-to-triggers.md
---

# Schedule triggers follow the wall clock and replay nothing

## Context

[ADR-0019](0019-modules-react-to-triggers.md) names `schedule` in the core trigger set, the WIT carries `schedule-tick { fired-at }`, and `#[nexum_sdk::module]` routes it to `on_schedule`.
The host parsed the manifest's `cron` string and then ignored it, so the trigger was declared and inert.

Making it fire needs decisions the WIT does not make.
The cron dialect, the timezone, what an outage does to the ticks it covered, what a module slower than its schedule receives, and what a step of the host clock does are all open.

[ADR-0024](0024-blocks-are-clocks-and-host-keys-leave-the-module-namespace.md) already decided the nearest case.
A block trigger is a clock, an outage does not replay blocks, and the host keeps no progress marker for one.
A schedule tick is a clock in the same sense and carries less than a block does: one timestamp and no content.

## Decision

A schedule trigger is a wall-clock clock, and it gets the block trigger's replay rule.

### The expression

`cron` is a five-field expression: minute, hour, day of month, month, day of week.
A field is `*`, a number, a range `a-b`, a step `*/n` or `a-b/n`, or a comma list of those.
The parser is hand-written in `nexum-runtime-manifest`, and the manifest carries the parsed form, so nothing downstream sees an unvalidated string.

Everything outside that grammar refuses the manifest at load with `ParseError::InvalidScheduleCron`, counted under the `invalid_schedule_cron` label.
A sixth or seventh field, an `@` macro, a timezone prefix, a name, the `L`, `W`, `#` and `?` extensions, a step on a single number, a wrapping range, an out-of-range value, and a date no year contains are all refusals.
The invariant: an expression either means exactly what the reference in `docs/02-modules-triggers-packaging.md` says, or the engine does not boot.

The two day fields follow Vixie cron.
When both are restricted a day matches when either does, and otherwise both must match.
A field is unrestricted when it starts with `*`, so `*/2` leaves it unrestricted.

### The clock

The expression is evaluated in UTC, against the host wall clock, and nowhere else.
The source reads the same clock a guest reads, so a WASI clock override moves both.

The source arms from the current wall clock on every pass and never from its last fire time.
It sleeps towards the armed minute on the runtime clock, reads the wall clock again at least once a second, and fires only when the wall clock has reached that minute.
A forward step therefore yields one tick for everything it skipped and then continues from the stepped time.
A backward step re-arms from the stepped time, so a minute the clock repeats fires again.

`fired-at` is the wall-clock time the source generated the tick, in milliseconds since the Unix epoch, as the WIT states.
It is not the nominal minute.

### No catch-up and no persistence

The host writes nothing for a schedule trigger.
A boot arms from the present, so the ticks an outage covered are not replayed.
The `host/<name>` namespace of ADR-0024 still holds the chain-log cursor and nothing else.

### One pending tick

One source opens per live schedule trigger, tagged with its module.
Each source holds at most one undelivered tick.
A tick that comes due while that one waits is dropped and counted in `nexum_runtime_dispatch_dropped_total` under `reason = "coalesced"`.
The tick that waits is the first one, and it keeps its own `fired-at`.

The invariant: the backlog of a schedule trigger is one tick, whatever the module's speed and however long the event loop was busy.

The event loop selects a waiting tick ahead of a block or a log.
The select is biased, and a log backfill can keep its channel full for as long as it runs, so a tick polled after it would wait for the whole backfill.
The one-tick bound is what makes the order safe: a schedule cannot starve a chain source.

### Dispatch

A tick is routed to its module by name and enters the guest through the path every other trigger takes: the rate limit, the fuel grant, the wall-clock deadline, the restart backoff and the poison policy.
A dead module opens no source.
A tick for a module in backoff is dropped, and the first tick past the backoff restarts that module alone, as an event does.
A poisoned module receives none.
A dropped tick is never retried.

A schedule source is a live source with no chain behind it.
It keeps the engine running on its own, and a manifest with only schedule triggers needs no `[chains]` entry.

## Rejected alternatives

- **A cron crate.**
  Each maintained one parses a superset: a seconds field, names, macros, or the `L`, `W` and `#` extensions.
  The supported grammar would then be the dependency's and would move with its releases, and refusing the superset needs a pre-pass that is most of a parser.
  Each also puts `chrono` into the compiled graph, where it does not appear today.
  The grammar here is small enough to state, test and own.
- **Local time, or a `timezone` key.**
  A daylight-saving transition makes a local minute occur twice or not at all, and the host would need a timezone database to know when.
  UTC has neither problem, and a module that wants a local time can compute one from `fired-at`.
- **Replaying missed ticks at boot.**
  It needs a host-written last-fired marker, and it hands a periodic module a burst of stale ticks it must then discard.
  ADR-0024 rejected the same thing for blocks, for the same reason.
  A module that must account for every period keeps its own marker and reads the gap from `fired-at`.
- **A queue of ticks per trigger.**
  A module slower than its schedule would accumulate a backlog without bound and then run it long after the instants it names.
- **Selecting schedule ticks after the chain sources.**
  Every module's schedule would stop for the length of one module's log backfill.
- **Keeping the newest pending tick instead of the first.**
  The first tick's `fired-at` tells the module how late its dispatch is.
  The newest would hide the delay behind a recent timestamp.
- **`fired-at` as the nominal minute.**
  The WIT defines it as the generation time, and after a clock step the nominal minute is not when anything happened.
- **Sleeping a fixed interval on the monotonic clock.**
  It drifts from the wall clock after every step, and a cron expression names wall-clock instants.
- **Holding the armed minute across a backward step.**
  The schedule would stay silent until the clock caught up, which is an outage of the size of the step.
- **Reading `*/n` as restricted in the day rule.**
  That is the literal POSIX reading, but Vixie cron and the tools authors check an expression against read it as unrestricted.

## Consequences

- A manifest whose `cron` loaded under the inert parser can refuse now, and a module that declares a schedule trigger starts to receive ticks.
- A module cannot tell two of its own schedule triggers apart, because a tick carries only `fired-at`.
  Telling them apart needs a WIT change, which this record does not make.
- A module sees at most one tick for a period it could not keep up with, and reads the delay from `fired-at` against its own clock.
- The schedule is only as right as the host clock.
  A step moves it, so an operator keeps the clock synchronized.
- `invalid_schedule_cron` joins the pinned `error_kind` set, and `coalesced` joins the `reason` values of `nexum_runtime_dispatch_dropped_total`.
- A sub-minute cadence is not expressible.
  A module that needs one uses a block trigger or an extension source.
