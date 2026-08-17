# Component lifecycle, event system, and packaging

## The component bundle

A component is distributed as a bundle: a WASM component plus a manifest that declares its identity, its event subscriptions, and the capabilities it depends on.
The manifest is the bridge between packaging, the event system, and the runtime lifecycle.
See [ADR-0016](adr/0016-component-vocabulary.md) and [ADR-0020](adr/0020-retire-component-kind.md).

### Manifest (`component.toml`)

Every component ships with a manifest.
The file is named `component.toml`.
The operator config is a separate, trusted file: see [ADR-0001](adr/0001-operator-config-separate-and-trusted.md).

```toml
[component]
name    = "twap-monitor"
version = "0.3.0"

# Optional content pin: one sha256sum of the compiled .wasm.
digest = "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"

# Optional interface claim: the full id of one interface this component
# exports. The engine verifies the claim against the compiled exports,
# and only an [implements] row in engine.toml authorizes the load.
provides = "acme:pool/quoter@2.0.0"

# Per-component resource requests. Each field narrows the engine [policy]
# ceiling and never widens it.
[component.resources]
max_fuel_per_dispatch = 500_000_000

# What this component depends on. The engine cross-checks the component's
# WIT imports against this table before instantiation, and an import
# outside the declared set refuses the boot. A bareword key names a host
# capability. An entry carrying `interface` depends on another
# component's provided interface: the key is the alias the author's own
# code calls, and the value is the interface's compatibility track.
[dependencies]
chain       = {}
local-store = {}
logging     = {}
http        = { hosts = ["api.cow.fi"] }
quoter      = { interface = "acme:pool/quoter@2" }

# Event subscriptions: what the runtime feeds this component.
[[subscription]]
kind = "block"
chain_id = 42161

[[subscription]]
kind = "chain-log"
chain_id = 42161
address = "0xfdaFc9d1902f4e0b84f65F49f244b32b31013b74"
event_signature = "0x0000000000000000000000000000000000000000000000000000000000000000"
resume = true

[[subscription]]
kind = "cron"
schedule = "*/5 * * * *"

# Opaque config, handed to the guest as string pairs.
[config]
api_url = "https://api.cow.fi/arbitrum"
min_twap_interval_secs = 120
enable_alerts = true
```

Key design points:

- **`digest` is a verification pin, not a locator.**
  The engine hashes the bytes it read and compares them to the pin before it compiles them (`crates/nexum-runtime/src/supervisor/artifact.rs`).
  The pin is optional: an absent pin loads with a warning, unless `require_component_digest = true` under `[engine]` in `engine.toml` makes it a boot error.
  The warning is silent when an `[implements]` row already pins the same artifact, because the bytes are verified either way.
  A refusal names which of the two pins the bytes disagree with, so the operator knows which file to edit.
- **`[[subscription]]` blocks are declarative.**
  A component does not set up its own subscriptions imperatively.
  The runtime loads each component and runs its `init` first, then derives the subscription plan from the booted supervisor and opens the event sources.
  `call_init` runs during load in `crates/nexum-runtime/src/supervisor/load.rs`, and `subscription_plan` reads the already-booted supervisor in `crates/nexum-runtime/src/supervisor/subscriptions.rs`.
- **`[dependencies]` drives what the runtime links.**
  A bareword key names a core host capability, and its table carries the attributes that qualify it.
  A component that declares `http` imports `wasi:http/outgoing-handler`, the SDK's `http::fetch` helper wraps it, and the host checks every outgoing request against the `hosts` list on the `http` dependency.
  See `modules/examples/http-probe` for a complete example.
- **An entry carrying `interface` depends on a provided interface.**
  The key is the alias the author's own code calls, and the value is the interface's compatibility track, for example `quoter = { interface = "acme:pool/quoter@2" }`.
  The value is a track, never a full version: a consumer asks for compatibility, and the provider's exact version is the provider's business.
  A full version, or any value that is not a track, refuses with `invalid_interface_track`.
  A name resolves against the core capability table first, so an alias equal to a capability name refuses with `alias_shadows_capability`; rename the alias.
  A bareword that names a provider component refuses with `dependency_names_component` and prints the corrected `interface` line.
  A track no loaded component provides refuses at boot with `interface_not_provided`, blaming the consumer, before any artifact is compiled.
  A component's own `provides` claim never satisfies its own dependency; that refuses with `self_interface_dependency`.
  The `[policy].capabilities` allowlist bounds capability keys only; the operator authorizes a provided interface through the provider's `[implements]` row instead (see [ADR-0018](adr/0018-one-operator-policy-surface.md) and [ADR-0021](adr/0021-provides-and-implements.md)).
  Calling the provider is stage 3 of the epic (#206); until it lands, the dependency resolves and refuses correctly, and the interface is not yet callable from the consumer's world.
- **`provides` is a claim, not authorization.**
  The engine walks the compiled component's exports before instantiation and refuses a claim no interface-instance export satisfies.
  A verified claim still does not load on its own: the operator binds the interface's compatibility track to one `[[modules]].id` in the `engine.toml` `[implements]` table, with a digest pin for the artifact, and an unbound or unpinned implementer refuses at boot.
  A row whose component makes no matching claim refuses too, so deleting `provides` cannot disarm the operator's pin.
  See [ADR-0021](adr/0021-provides-and-implements.md).
- **The `[dependencies]` table is mandatory.**
  A manifest with no table at all is refused; an empty table is valid and grants nothing.
  `hosts` qualifies the `http` capability dependency and nothing else, and it is refused anywhere else, including on an interface entry, rather than silently dropped.
- **Chain ids are declared per subscription**, not in a top-level `[chains]` table.
  Each `[[subscription]]` names its own `chain_id`.
  If `engine.toml` carries no `[chains.<id>]` entry for a chain a subscription names, the engine refuses the boot in the prepass, before any component is compiled.
- **`[config]` is opaque to the runtime.**
  The guest receives `list<tuple<string, string>>`.
  The host flattens each TOML scalar to its text form on the way through, and renders an array or a table as its TOML representation.
  A typed `config-value` variant is a later change.

The declarable core capability names are `chain`, `identity`, `local-store`, `remote-store`, `logging`, and `http`, plus the gated WASI names `wasi-sockets` and `wasi-filesystem`.
`wasi:io`, `wasi:clocks`, `wasi:random`, and `wasi:cli` are ambient and are never declared.
Any other `wasi:` import is refused fail-closed.
An extension registers further names under its own namespace: see [the linker extension seam](design/linker-extension-seam.md).

> Resource ceilings are set in `engine.toml` `[policy]`: `max_fuel_per_dispatch` (default 1e9), `max_memory_bytes` (default 64 MiB), and `max_state_bytes` (default 50 MiB).
> The dispatch deadline is `[limits.dispatch].deadline_secs` (default 120).
> A `[policy.component.<id>]` row overrides them for one component, keyed on the `[[modules]].id` the operator writes.
> They resolve in `crates/nexum-runtime/src/engine_config/`.
> A `[component.resources]` field narrows one of them.
> It never replaces or widens one: the manifest is author-supplied, so the engine value is a ceiling and a request above it is clamped and logged (`crates/nexum-runtime/src/supervisor/store.rs`, `resolve_module_limits` and `clamp`).

### Bundle format

A bundle is a directory with a fixed layout:

```
twap-monitor/
|-- component.toml         # manifest
`-- component.wasm         # compiled component
```

The engine reads the artifact from the local filesystem path in the `[[modules]]` entry.
The manifest defaults to a `component.toml` beside the artifact, and the `manifest` key names one elsewhere.
An operator-owned manifest from outside the artifact directory, combined with `require_component_digest = true`, is what closes the gap against a compromised artifact store: the default sibling manifest sits in the same trust domain as the artifact it pins.

### Distribution

Distribution is out of scope for the runtime today.
The engine resolves no content address and fetches nothing: `[[modules]] path` is a local filesystem path, and `digest` verifies the bytes it reads there.

A content-addressed layer above the engine (a local content store fronting Swarm, IPFS, OCI, or plain HTTPS, keyed on the same sha256 the manifest already pins) fits without a manifest change, because the pin is already the trust anchor rather than the transport.
None of it is implemented here.
The `nexum:host/remote-store` interface exists in the WIT, but every method currently returns `unsupported`.

## Component lifecycle

Load is a straight-line function, not a state machine.
The persistent state is `Health` in `crates/nexum-runtime/src/supervisor/lifecycle.rs`: a lifecycle value plus a failure count and a sliding failure window.

```mermaid
stateDiagram-v2
    [*] --> Load: read bytes, verify digest, compile
    Load --> Dead: init returned a fault at boot
    Load --> Alive: init succeeded
    Alive --> Backoff: trap or deadline hit
    Backoff --> Alive: restart succeeded
    Backoff --> Poisoned: too many failures in the window
    Dead --> [*]
    Poisoned --> [*]
```

| Value | Meaning |
|---|---|
| **Alive** | Dispatchable. The only value that receives events. |
| **Backoff** | A trap or a deadline hit ended the run. The supervisor revives it when the backoff expires. |
| **Dead** | The boot-time `init` returned a fault. Permanent: the supervisor never restarts it. |
| **Poisoned** | Too many failures inside the poison window. Terminal for the process; only an operator restart clears it. |

Load itself is ordered: the prepass resolves every manifest, claims namespaces, and gates subscribed chains against `[chains]`; then modules load.

The backoff schedule doubles from 1 s and caps at 300 s: 1, 2, 4, 8, 16, 32, 64, 128, 256, then 300 s.
The poison policy is 5 failures inside 600 s by default, configurable under `[limits.poison]`.
A failed restart defers the next attempt and does not itself count toward the poison window.
A successful dispatch resets the module's failure count.

### Key lifecycle properties

- **State survives a restart.**
  The redb local store is external to the WASM instance, and each component sees its own isolated namespace of it.
  A restarted component picks up where it left off.
- **Memory does not survive a restart.**
  Each restart builds a fresh `Store`: clean linear memory, no stale pointers.
- **The compiled `Component` is cached.**
  Reading, digest verification, and compilation happen once at load.
  A restart re-instantiates the cached component on a fresh store and runs `init` again; it never re-reads the file.
- **Config is immutable for a loaded component.**
  Changing config requires an engine restart.
- **There is no hot reload.**
  The engine watches no path and detects no artifact change.
  Adding, changing, or removing a component means editing `engine.toml` and restarting the engine.
- **Poison recovery is operator work.**
  The failure ring is in memory and clears at process start.

## Event system

### Architecture

```mermaid
flowchart TD
    subgraph SRC["Event sources"]
        BS["Block streams (per chain)"]
        LW["Chain-log streams (per subscription)"]
        EX["Extension streams"]
    end

    subgraph NR["nexum-runtime"]
        SRC
        EL["Event loop\n(one select over every source)"]
        SUP["Supervisor\n(matches the subscription plan)"]
        MA["Component A"]
        MB["Component B"]
    end

    BS --> EL
    LW --> EL
    EX --> EL
    EL --> SUP
    SUP --> MA
    SUP --> MB
```

### Event sources

| Source | Trigger | Backed by |
|---|---|---|
| `block` | New block on a chain | `eth_subscribe("newHeads")` over an alloy provider, or polling on an HTTP URL |
| `chain-log` | Matching log emitted | An `eth_getLogs` block-range poller, alloy's `watch_canonical_logs_from`, reorg-aware and backfilling from the start block on open |
| `cron` | Not dispatched | Parsed and inert. The supervisor warns at load. |
| extension kinds | Whatever the extension opens | `Extension::events` |

Block streams are shared per chain.
If two components subscribe to blocks on chain 42161, the runtime opens one block subscription and fans it out to both.
A chain-log stream is opened per subscription and tagged with the owning component.

Only a dispatchable component contributes to the plan, so no stream opens for a dead or poisoned one.

### Dispatch

There is no router struct and no per-component inbox.
One `run` loop owns the supervisor, selects one event from the merged sources, and dispatches it before it selects again (`crates/nexum-runtime/src/runtime/event_loop.rs`).
For one event, the supervisor walks the matching components serially and awaits each in turn (`crates/nexum-runtime/src/supervisor/dispatch.rs`).

- **Serial, not concurrent.**
  A slow component delays the whole engine.
  The wall-clock dispatch deadline is what bounds that delay.
- **Ordered within a source.**
  A component sees block N before block N+1.
  Interleaving between two different sources is not deterministic.
- **Rate limited per component.**
  Each component holds a token bucket, checked before the guest is entered: `burst` 256 and `refill_per_sec` 128 by default, configurable under `[limits.dispatch]`.
  An event over the rate is dropped, counted in `nexum_runtime_dispatch_dropped_total`, and never retried.
  The bucket carries across a restart.
- **No acknowledgement.**
  A successful return from `on-event` is not an ack.
  A component that needs progress tracking writes it to the local store itself.
- **Cursors commit only after a successful dispatch.**
  A `resume` chain-log subscription persists its cursor after each successful dispatch, so a dropped or failed dispatch is re-delivered on the next boot rather than skipped.
- **Catch-up is the component's job, except for chain-log backfill.**
  The engine backfills the gap for a `resume` subscription on reconnect, capped by `max_lookback`.
  Anything else, for example a gap across a restart on a non-resume subscription, is for the component to detect in `init` and to backfill through `chain::request`.

### What bounds one dispatch

Two mechanisms, and no others:

1. **Fuel.**
   `consume_fuel` is on in `wasmtime_config` (`crates/nexum-runtime/src/builder.rs`), and `dispatch` refuels the store to the resolved budget before every dispatch (`crates/nexum-runtime/src/supervisor/dispatch.rs`).
2. **A wall-clock deadline.**
   The dispatch runs under `[limits.dispatch].deadline_secs`, which covers the host-call time fuel cannot meter.
   A deadline hit leaves the store unusable, so it is treated like a trap: the run ends and the component restarts.

The host does not use epoch interruption.
`wasmtime_config` sets `wasm_component_model` and `consume_fuel` and nothing else (`crates/nexum-runtime/src/builder.rs:148-149`).

### Event encoding

Events cross the WASM boundary as the `event` variant in `wit/nexum-host/types.wit`:

```wit
variant event {
    block(block),
    chain-logs(chain-logs),
    tick(tick),
    custom(custom-event),
}

record block {
    chain-id: chain-id,
    number: u64,
    hash: list<u8>,
    timestamp: u64,
}

record tick {
    fired-at: u64,
}

record custom-event {
    kind: string,
    payload: list<u8>,
}
```

The canonical ABI carries the data, handled by `bindgen!`.
Every `u64` timestamp in the package is milliseconds since the Unix epoch, UTC.
`custom` is the generic extension event: the core routes it by `kind` and never reads `payload`, and the subscribing component decodes it against the extension that emitted it.

## The `nexum:host` world

The universal package is `nexum:host@0.1.0`, and it is a leaf: it imports no other package.
`wit/nexum-host/event-module.wit` declares the world a module is built against.

```wit
world event-module {
    use types.{config, event, fault};

    import chain;
    import identity;
    import local-store;
    import remote-store;
    import logging;

    export init: func(config: config) -> result<_, fault>;
    export on-event: func(event: event) -> result<_, fault>;
}
```

Time, randomness, and outbound HTTP are WASI concerns rather than `nexum:host` interfaces.
`wasi:clocks` and `wasi:random` are linked into every store, and the host links `wasi:http/outgoing-handler` gated by the `hosts` list on the `http` dependency.

A component built through `#[nexum_sdk::module]` does not import the whole world.
`nexum-world` synthesizes a per-component world whose imports are exactly the interfaces its `[dependencies]` declare, so an undeclared capability is absent by construction rather than trapped at first use.

## Putting it together

An operator deploys a module:

```
1. The operator adds an entry to engine.toml:

   [[modules]]
   id   = "twap-monitor"
   path = "/var/nexum/twap-monitor/twap_monitor.wasm"

2. The prepass resolves the sibling component.toml, claims the name as a
   local-store namespace, and checks that every subscribed chain has a
   [chains.<id>] entry.

3. The supervisor reads the artifact, verifies sha256 against
   [component].digest, and compiles the Component.

4. It cross-checks the component's WIT imports against [dependencies] and
   refuses an import the manifest does not declare.

5. It builds a fresh Store under the resolved limits, instantiates, and
   calls init(config).

6. Once every component has loaded, the runtime derives the subscription
   plan and opens the event sources:
   - one block stream per subscribed chain
   - one chain-log stream per chain-log subscription, seeded from its
     durable cursor when resume = true

7. Events flow:
   block 19_000_001 on Arbitrum
   -> event loop -> supervisor matches the plan
   -> await on-event(event::block(...)) on each matching component
   -> the component calls chain::request and local-store::set
   -> Ok(()) commits the block marker and the loop selects again

8. On a trap:
   -> the run ends, the failure is counted and the component enters Backoff
   -> the supervisor revives it after the backoff: fresh Store, init again
   -> local-store data is intact, so the component resumes
   -> five failures inside the window instead leave it Poisoned
```
