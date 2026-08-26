# Production deployment

Operator handbook for running the `nexum` engine in production: systemd unit, state backup, and observability wiring.
The engine is the bare binary from `crates/nexum-cli`.
A downstream composition root that registers extensions runs the same way, under its own binary name.

## 1. Pre-flight

- The engine built in release: `cargo build -p nexum-cli --release` gives `target/release/nexum`.
- Every component `.wasm` artifact present on a path the service user can read.
- An `engine.toml` with `state_dir` on a persistent path (never `/tmp`), `log_level = "info"`, `[engine.metrics] enabled = true` with `bind_addr = "127.0.0.1:9100"`, one `[chains.<id>]` per triggered chain with a paid RPC URL, and one `[[modules]]` per module, each with an operator-written `id`.
- A `digest` on each `[[modules]]` entry, set to the line `nexum digest <artifact>` prints for that file.
  The engine requires this by default and refuses to boot an entry without one, so there is no `[engine]` key to set here: leave `require_component_digest` alone ([ADR-0025](adr/0025-the-required-digest-is-the-operator-pin.md)).
  The command reads the artifact and prints its `sha256:<64 hex chars>` pin on stdout, with nothing around it, so the value pastes into `[[modules]].digest` unedited.
  The refusal for a missing pin reports the same value, so a first boot on a new artifact also gives you the line.
  This is the operator's own pin, in trusted config: the default sibling manifest lives in the same trust domain as the artifact, so its `[component].digest` is evidence of intent and not of authorization.
- An operator-owned `component.toml` outside every artifact directory, named by the `manifest` key on each `[[modules]]` entry, if the artifact store is not trusted.
  `[[modules]].digest` alone does not close artifact-store compromise.
  It fixes the artifact bytes and says nothing about the manifest beside them, and that manifest is the sole HTTP allowlist, the `[config]` source, and the state-namespace selector.
  Only relocating the manifest out of the store closes the gap.
- A `[component].digest` in each manifest is optional and independent.
  Both pins are verified against the exact bytes handed to the compiler, and a mismatch refuses the boot naming which pin failed and the file it is in.
  An author pin never substitutes for the operator pin.
  A module carrying neither pin loads with nothing checking its bytes: the `supervisor ready` line then reports `verified` below `modules`, and `nexum_runtime_module_unverified` names each such module for the life of the process.
- The `state_dir` exists and is writable by the service user.
- A Prometheus instance scraping `/metrics` (section 6) with the alert rules in section 7.
- A log aggregator ingesting the engine's JSON stdout (section 5).

`engine.toml` substitutes `${VAR_NAME}` from the environment before it parses the TOML, so an RPC URL with a key in it stays out of the file.
A missing variable is a fatal boot error that names the variable.

## 2. systemd unit

`/etc/systemd/system/nexum.service`:

```ini
[Unit]
Description=Nexum WASM component runtime
Documentation=https://github.com/nullislabs/nexum-runtime
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=nexum
Group=nexum
WorkingDirectory=/opt/nexum
ExecStart=/opt/nexum/bin/nexum --engine-config /etc/nexum/engine.toml

# TimeoutStopSec must exceed the engine's resolved shutdown drain, which
# the `supervisor ready` line reports as shutdown_drain_secs. It defaults to
# [limits.dispatch] deadline_secs + 30, so raising the deadline raises it too.
# (150s untuned), or SIGKILL pre-empts the drain.
KillSignal=SIGINT
TimeoutStopSec=180s

# Hardening.
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ReadWritePaths=/var/lib/nexum
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
LockPersonality=true
MemoryDenyWriteExecute=false   # wasmtime JIT needs writable-executable pages

# The supervisor restarts trapped components itself; this restarts the host
# process on a non-zero exit. RestartSec avoids a fast loop on config errors.
Restart=on-failure
RestartSec=5s

# Defence in depth on top of the per-component wasmtime caps.
LimitNOFILE=65536
MemoryMax=2G
CPUQuota=200%

Environment=RUST_BACKTRACE=1
# RUST_LOG overrides engine.toml log_level; leave it unset so the config
# is the single auditable source.

[Install]
WantedBy=multi-user.target
```

A stop halts dispatch at the next guest-call boundary, drains the one call in flight, commits its cursor, and exits 0.
Modules the halt cut out of a block fan-out do not receive that block; an undispatched event replays at the next start through its `resume` cursor (section 4).
The drain is bounded by `[limits.shutdown] drain_secs`, which defaults to `deadline_secs` plus 30 s, so an untuned drain outlasts the one deadline-bounded call it can be left waiting on.
A drain past the bound therefore means a wedged task, not a long dispatch, and it forces exit 1 so `Restart=on-failure` restarts the engine.
Keep `TimeoutStopSec` above the resolved bound, or systemd's SIGKILL pre-empts the forced exit.
The engine logs that bound as `shutdown_drain_secs` on the `supervisor ready` line at every start, so read it there rather than recomputing it.
Raising `deadline_secs` raises the drain default with it, and the unit below does not follow.

Bring it up:

```bash
sudo useradd -r -s /usr/sbin/nologin -d /var/lib/nexum nexum
sudo install -d -o nexum -g nexum /var/lib/nexum
sudo install -d -o nexum -g nexum /opt/nexum/bin
sudo install -m 0755 -o nexum -g nexum target/release/nexum /opt/nexum/bin/
sudo install -d /etc/nexum
sudo install -m 0644 engine.toml /etc/nexum/
sudo systemctl daemon-reload
sudo systemctl enable --now nexum
```

Tail the logs:

```bash
journalctl -u nexum -f --output=json | jq '.MESSAGE | fromjson?'
```

The repository ships no container image and no compose file.

## 3. State backup (redb)

The local store is a single redb file at `<state_dir>/local-store.redb`, and `state_dir` defaults to `./data` beside the working directory.
Every component's isolated namespace lives inside that one file, so one file is the whole engine's state.
Losing it forces a from-scratch resync as each component rediscovers its state.

Durability is per host call, not per event.
Each write is its own fsynced transaction, so a returned `ok` is durable, and a trap or a crash freezes state at the last completed call.
The `apply` batch verb is the only atomic multi-write unit.
See [ADR-0014](adr/0014-local-store-durability-model.md).

Cold backup, which is the one to use before an upgrade.
The engine writes to redb only during a dispatch, and the graceful shutdown drains the in-flight dispatch, so a stopped file is quiescent:

```bash
sudo systemctl stop nexum
sudo cp /var/lib/nexum/local-store.redb \
    /backup/nexum-$(date -u +%Y%m%dT%H%M%SZ).redb
sudo systemctl start nexum
```

Live copy.
A plain `cp` under a live writer can capture an in-flight commit, so pause the process first:

```bash
kill -STOP <pid>
cp /var/lib/nexum/local-store.redb /backup/...
kill -CONT <pid>
```

The pause window is sub-second on a small store, and the WS connections survive it.
Restore by stopping the engine, copying the snapshot back, and restarting.
If a restored file does not open, roll forward from the previous snapshot, or start with an empty `state_dir` and accept the resync.

## 4. Cursors the runtime writes

The runtime writes one kind of key, best-effort after a successful dispatch and after each completed bulk chunk, under a host-owned `host/<name>` namespace beside each component's own:

- `chainlog_cursor:<hex>`, the resume cursor for a `resume = true` event trigger.
  The engine reads it once at boot, re-opens at that block, and backfills the gap on reconnect, capped by `max_lookback`.
  A large finalized gap first backfills in bulk `eth_getLogs` chunks (section 8).
  The cursor commits after each chunk, so an interrupted catch-up resumes where it stopped.
  A failed dispatch stops the per-chunk commits for that trigger, and a restart then replays the logs the module missed.
  A reorg retraction pulls the cursor back.

Every key in a component's own namespace is the component's own, and `max_state_bytes` measures that namespace alone.
A component cannot reach a `host/` namespace, because a component name cannot contain `/`.

A forced exit (a drain past `[limits.shutdown] drain_secs`) terminates the process before the in-flight dispatch commits its cursor, and the cursor stays at the last committed dispatch.
A `resume = true` trigger then replays the in-flight log at the next start; a block is not replayed.
See [ADR-0024](adr/0024-blocks-are-clocks-and-host-keys-leave-the-module-namespace.md) for why blocks are not replayed and no host gap primitive exists.

## 5. Logs

The engine emits JSON `tracing` events on stdout, one flat object per line.
`--pretty-logs` switches to the human format.
Every event carries `timestamp`, `level`, `target` (the crate and module path), and `message`.
A guest log line is mirrored into host tracing at the guest's own level with `module`, `run`, and `channel` fields, and guest stdout and stderr are captured line by line.
Production should not see `ERROR` from `nexum_runtime::*`.

`RUST_LOG` wins over `[engine] log_level`, which is itself a full `EnvFilter` directive rather than a bare level.
Leave `RUST_LOG` unset in the unit so the config file is the single auditable source.

Aggregate stdout into your log stack.
A journald source that parses the JSON `message` field and routes on `level` is the usual pattern.

## 6. Metrics

`[engine.metrics] enabled = true` binds one listener on `bind_addr` serving `/metrics`, `/healthz` and `/readyz`.
Always bind loopback and never `0.0.0.0`: Prometheus scrapes over the loopback or the container network.
A Kubernetes `httpGet` probe is the one case that cannot, and Probes below says what it costs.
The bare `nexum` binary registers the recorder itself, through the `CoreRuntime` preset.
With `enabled = false` the recorder is still installed, so call sites stay live, but no listener binds and no sample or probe answer is readable.
An address already in use refuses the launch rather than leaving a running engine with a dead endpoint.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `nexum_runtime_boot_refusals_total` | counter | `error_kind` | Boot refusals by error kind. |
| `nexum_runtime_dispatch_latency_seconds` | histogram | `module`, `trigger_kind`, `outcome` | Wall-clock seconds to dispatch one trigger, sampled on every dispatch that reached the guest. `outcome` is one of `ok`, `fault`, `trap` and `deadline`, so a p95 read over the bare metric covers the failing dispatches too; sum `outcome` away to read one module's whole distribution, as `NexumDispatchLatency` below does. A trigger dropped before the guest runs records no latency and is counted in `nexum_runtime_dispatch_dropped_total` instead. `outcome` is bounded at those four values, so it multiplies the series count by at most four: each `module`, `trigger_kind` and `outcome` triple carries the eleven buckets plus `+Inf`, `_sum` and `_count`. |
| `nexum_runtime_dispatch_dropped_total` | counter | `module`, `trigger_kind`, `reason` | Triggers dropped before dispatch. `reason = "rate_limited"` is the per-component dispatch rate limit (`[limits.dispatch]`, default `burst = 256` and `refill_per_sec = 128`). `reason = "shutdown"` is a stop landing mid fan-out: the fan-out follows `[[modules]]` order, so the same trailing modules are skipped at every stop. `reason = "fuel_set_failed"` is the per-dispatch fuel budget failing to apply: the module stays alive and the trigger is skipped. A block is not replayed; an event is, from its cursor. |
| `nexum_runtime_module_errors_total` | counter | `module`, `error_kind` | Module faults and traps. `error_kind = "trap"` is a wasmtime trap and `"deadline"` is a dispatch cut off at `[limits.dispatch] deadline_secs`; other values are fault labels. The trap and deadline values are the same strings the latency histogram uses for `outcome`, so the two metrics agree about one event. |
| `nexum_runtime_module_restarts_total` | counter | `module` | Module restart attempts. |
| `nexum_runtime_module_poisoned` | gauge | `module` | `1` once a module crosses `[limits.poison]` (default 5 failures in 600 s) or its event source reports an unrecoverable condition. Stays `1` until the process restarts. |
| `nexum_runtime_module_state` | gauge | `module`, `state` | The module's lifecycle state, `1` on the state it is in and `0` on the other three. `state` is one of `alive`, `backoff`, `dead` and `poisoned`, so each module costs four series. `alive` is the dispatchable one and the one `/readyz` counts; `backoff` is a module awaiting its scheduled restart, `dead` a module whose boot-time `init` failed, and `poisoned` a quarantined one. `nexum_runtime_module_poisoned` stays as its own series so the `NexumModulePoisoned` alert below keeps reading one metric. |
| `nexum_runtime_module_unverified` | gauge | `module` | `1` for a module loaded with neither a `[[modules]].digest` nor a `[component].digest`, so nothing checked its bytes. Set once at boot and never cleared; a pinned module emits no series, so `sum` is the fleet's unverified count. Reaching this state at all needs `require_component_digest = false`, or a single-wasm command-line override. |
| `nexum_runtime_module_fuel_consumed` | gauge | `module` | Fuel the module's last dispatch spent, out of the ceiling that dispatch was granted: `[component.resources] max_fuel_per_dispatch` clamped to the `[policy]` value of the same name. The ceiling is config rather than a series, so an alert compares this against the number you set. The gauge holds the last dispatch and is stale between scrapes for a module that triggers rarely, which is what makes it answer "is anything near its ceiling" directly. |
| `nexum_runtime_module_memory_bytes` | gauge | `module` | Linear memory the module holds as of its last dispatch, against `[component.resources] max_memory_bytes` clamped to the `[policy]` value. Read on the growth path itself, and wasm linear memory never shrinks, so the value is the size the module reached rather than a sample of it. A restart rebuilds the store, so the series drops back to the reloaded module's first growth. |
| `nexum_runtime_capability_denials_total` | counter | `capability`, `reason`, `module` | Capability requests the host refused. `capability = "http"` is the outbound gate. `reason = "allowlist"` is a host outside the effective allowlist, which is the intersection of `[dependencies.http].hosts` and `[policy.component.<id>].http_allow`. `reason = "destination"` is a target that is, or resolves onto, an address the host will not reach: a default-refused range such as loopback, RFC 1918 or link-local, or a `[policy].http_deny` range. Every label is host-side, so a module cannot mint series by varying the host it asks for. A rate here is a module asking for what the manifest and the operator policy do not grant. |
| `nexum_runtime_run_end_total` | counter | `reason` | Why the event loop returned, counted once per run. `shutdown` and `nothing_live` are clean stops. `source_terminal` is a shared source reporting an unrecoverable condition. `stream_ended` is a source pump that panicked or was aborted, which is the abnormal one: alert on it. |
| `nexum_runtime_chain_request_total` | counter | `chain_id`, `method`, `outcome` | Every `chain::request`. A method outside the read surface is counted as `method="<denied>"` with `outcome="err"`. A request for a chain outside `[chains]` is counted as `chain_id="unconfigured"`, which bounds the series set to the configured chains plus one and shows that a module is requesting chains the operator has not configured. The `outcome="err"` rate is the RPC-degraded signal. |
| `nexum_runtime_chain_response_capped_total` | counter | `chain_id`, `method` | Responses rejected for exceeding `[limits.chain] response_body_max_bytes` (default 1 MiB). |
| `nexum_runtime_source_reconnects_total` | counter | `source_kind`, `chain_id`, `module` | Source reconnects. `source_kind="block"` is per chain; `source_kind="chain-log"` also carries `module`. Both source tasks carry the same value in a `source_kind` log field, so the value `NexumReconnectStorm` reports greps the logs. |
| `nexum_runtime_log_records_dropped_total` | counter | `module`, `channel` | Module log records dropped whole by the per-component log rate limit (`[policy]`, default `max_log_burst = 256` and `max_log_records_per_sec = 128`). `channel` is the capture point the record entered by, one of `host_interface`, `stdout` or `stderr`; all of them spend one bucket per run. A sustained rate here is a module flooding the log sink. |
| `nexum_runtime_log_records_truncated_total` | counter | `module`, `channel` | Module log records shortened to fit `[policy] max_log_record_bytes` (default 8 KiB). The message is kept and marked `...[truncated]`; the file is cut first, and the target holds a 128-byte allowance ahead of the message so an oversized record still names its subsystem. A captured stdout or stderr line past the cap is cut the same way. |
| `nexum_runtime_log_fields_dropped_total` | counter | `module`, `channel` | Structured log fields dropped past the same per-record cap, last-recorded first, so the earliest context survives. |
| `nexum_runtime_log_records_filtered_total` | counter | `module`, `channel` | Module log records dropped by the `[policy]` log filter, which is an operator choice rather than a loss. A record kept out of the console but not out of retention is not counted here. |

`crates/nexum-runtime-metrics/src/lib.rs` is the single source of the name set, and a test refuses any emitted name the table does not carry.

### Probes

`/healthz` answers `200` for as long as the process serves at all, so it is the liveness probe: a wedged process stops answering, and that is the condition a restart fixes.

`/readyz` answers `200` when at least one module is dispatchable and `503` otherwise, so it is the readiness probe.
A module in backoff, a dead module and a quarantined one are all undispatchable, but one of them among several must not pull an engine still serving the rest out of rotation.
Its body carries the per-module detail the aggregate flattens, one `name: state` line per module under a `ready:` line, so an operator sees which module is in backoff or quarantine without reading logs.
Before the supervisor has booted, `/readyz` answers `503` with no module lines, which is how a probe tells starting apart from degraded.

The kubelet dials the pod address, not the container's loopback, so the `httpGet` probes below need `bind_addr` on the pod interface and a `NetworkPolicy` holding the port to the scraper.
An `exec` probe curling `127.0.0.1` keeps the loopback bind instead, at the cost of a shell and a client in the image.

```yaml
livenessProbe:
  httpGet: { path: /healthz, port: 9100 }
readinessProbe:
  httpGet: { path: /readyz, port: 9100 }
```

Prometheus scrape:

```yaml
scrape_configs:
  - job_name: nexum
    scrape_interval: 15s
    static_configs:
      - targets: ["127.0.0.1:9100"]
```

## 7. Alerting

`prometheus-rules.yml`:

```yaml
groups:
  - name: nexum
    interval: 30s
    rules:
      - alert: NexumModulePoisoned
        expr: nexum_runtime_module_poisoned > 0
        for: 1m
        labels: { severity: page }
        annotations:
          summary: "Nexum module {{ $labels.module }} is poisoned"

      # A clean stop counts `shutdown` or `nothing_live`, and this end still
      # exits zero, so the metric is the only signal.
      - alert: NexumEventLoopDied
        expr: increase(nexum_runtime_run_end_total{reason="stream_ended"}[10m]) > 0
        labels: { severity: page }
        annotations:
          summary: "Nexum event loop ended on a dead source pump; restart the engine"

      - alert: NexumModuleUnverified
        expr: nexum_runtime_module_unverified > 0
        for: 1m
        labels: { severity: ticket }
        annotations:
          summary: "Nexum module {{ $labels.module }} is running unverified; pin its digest in engine.toml"

      # A deadline is a limit doing its job, so it is deliberately not here.
      # Alert on error_kind="deadline" separately if a module should never
      # reach one.
      - alert: NexumModuleTraps
        expr: rate(nexum_runtime_module_errors_total{error_kind="trap"}[5m]) > 0
        for: 5m
        labels: { severity: ticket }
        annotations:
          summary: "Nexum module {{ $labels.module }} trapping"

      - alert: NexumRpcErrorRate
        expr: |
          sum by (chain_id) (rate(nexum_runtime_chain_request_total{outcome="err"}[5m]))
            / sum by (chain_id) (rate(nexum_runtime_chain_request_total[5m])) > 0.05
        for: 10m
        labels: { severity: ticket }
        annotations:
          summary: "Nexum RPC error rate above 5% on chain {{ $labels.chain_id }}"

      - alert: NexumReconnectStorm
        expr: rate(nexum_runtime_source_reconnects_total[5m]) > 0.1
        for: 5m
        labels: { severity: ticket }
        annotations:
          summary: "Nexum WS reconnecting frequently"

      - alert: NexumDispatchLatency
        expr: |
          histogram_quantile(0.95,
            sum by (module, le) (rate(nexum_runtime_dispatch_latency_seconds_bucket[10m]))) > 5
        for: 15m
        labels: { severity: ticket }
        annotations:
          summary: "Nexum module {{ $labels.module }} p95 latency above 5s"

      # A single refusal clears inside the rate window, so only a module
      # that keeps asking holds this for 10m.
      - alert: NexumCapabilityDenied
        expr: |
          sum by (module, capability, reason)
            (rate(nexum_runtime_capability_denials_total[5m])) > 0
        for: 10m
        labels: { severity: ticket }
        annotations:
          summary: "Nexum module {{ $labels.module }} denied {{ $labels.capability }} ({{ $labels.reason }})"

      - alert: NexumDown
        expr: up{job="nexum"} == 0
        for: 2m
        labels: { severity: page }
        annotations:
          summary: "Nexum is down (metrics scrape failing)"
```

`page` wakes on-call for a poisoned component or a down engine; `ticket` routes during business hours.
A `NexumRpcErrorRate` alert on `chain_id="unconfigured"` points at a module that requests a chain outside `[chains]`, not at a node: every request under that label is an error by construction.
A `NexumCapabilityDenied` alert is either a manifest that asks for more than the operator grants, or a module probing the gate.
Read the `reason` label first: `allowlist` names a host the policy excludes, and `destination` names an address the host will not reach.

## 8. RPC selection

The engine opens one provider per `[chains.<id>]` entry at boot, and a failure there is fatal.
Public nodes throttle `eth_subscribe` and `eth_call`, so production needs a paid endpoint.
Prefer `wss://` where it is offered: a WebSocket pushes new heads through `eth_subscribe(newHeads)`, while an HTTP URL polls at the chain's average block time.
Both work; push is lower latency.
An `http(s)://` URL is not dialled at boot, so a bad HTTP endpoint surfaces on first use rather than at start.

Every provider carries a retry layer (10 attempts, 300 ms base backoff).
A `request_timeout_secs` under `[chains.<id>]` bounds one request and defaults to 30 s.
The same value bounds each open of a block or event source: the head fetch, the `eth_subscribe(newHeads)` handshake, and the tail-hash probe on reconnect.
An elapsed deadline there retries on the source backoff and does not surface to a guest.
The retry warning carries a `timed_out` field, which separates a deadline from a transport error.
The chain interface has no batch verb; the guest SDK lowers a batch of RPC requests to sequential single requests, each with the full timeout, so a batch's worst case is the entry count times that timeout.
`nexum_runtime_chain_request_total{outcome="err"}` is the degradation signal.

A `max_log_range_blocks` under `[chains.<id>]` declares the maximum block range the endpoint accepts for one `eth_getLogs` request, and defaults to 1000.
The engine uses it as the chunk size of the bulk backfill.
The bulk backfill runs when the finalized part of a resume gap is 1000 blocks or more.
That threshold is fixed in the engine and is separate from this key.
The engine fetches that finalized part in chunks of `max_log_range_blocks`, then hands off to the per-block poller for the last 64 blocks and for the live tail.
It commits the resume cursor after each chunk.

The limit is a property of the endpoint, not of the chain, so read it from the provider's JSON-RPC documentation.
Providers state it as a maximum block range for `eth_getLogs`, and some also cap the number of results one request can return.
Take the smaller of the two if both apply, and keep the default when the documentation states neither.
A self-hosted archive node applies no limit, so raise the value until one chunk's response time is no longer acceptable.
To check a raised value, send one `eth_getLogs` at that range over blocks whose logs you know, and confirm the endpoint returns all of them.

Do not set a value above what the endpoint serves.
Some endpoints answer a too-wide range with a partial result or an empty result instead of an error.
The engine cannot tell such an answer from a truthful one.
It commits the cursor past those blocks, and the logs in them never reach the module.
A value below the endpoint's limit is always safe, and costs only more requests for the same catch-up.

A failing chunk retries on the source backoff.
Five consecutive failures on one chunk abandon the bulk backfill for that catch-up, which then continues per block, and the engine logs the abandonment.
The bulk backfill logs the chunk size, the blocks remaining, and the catch-up rate, so a long recovery reads as progress rather than as a stall.

Resource ceilings live in `engine.toml` `[policy]` and apply to every component; a `[policy.component.<id>]` row, keyed on `[[modules]].id`, overrides them for one.
`[policy.total].max_memory_bytes` bounds the summed reservations, and an overcommitted set refuses at boot naming the entry that crossed it.
A `[component.resources]` field in a manifest narrows a ceiling for one component and can never widen it.
`[policy]` also bounds the module logging path: `max_log_record_bytes` caps one record and `max_log_burst` with `max_log_records_per_sec` caps the rate, both per component.
The `nexum:host/logging` verbs and the captured stdout and stderr lines spend one bucket per run, so the rate is records per second for the component however it writes them.
The supervisor's death record is host-synthesized and stays ungated.
A module past either bound loses records rather than the host, and the three `nexum_runtime_log_*` counters in section 6 say which bound it crossed and on which channel.
`[policy]` also filters that path by level and target, on two thresholds: `log_print_level` gates the console and `log_retain_level` gates what `nexum logs` keeps, with a `[policy.log_targets]` table lifting named targets above `log_print_level`.
`log_retain_level` defaults to `trace` and `log_print_level` defaults to whatever retention resolves to, so setting retention alone quietens both and setting `log_print_level` alone prints less while keeping everything; a console louder than retention refuses at load, target rows included.
A captured stdout or stderr line carries no target, so it is filtered on its channel level alone, `INFO` for stdout and `WARN` for stderr.
The filter runs before the bound, so a record it drops spends no token of the run's bucket.
A `[policy.log_targets]` key matches the guest-reported target exactly, which for an SDK module is the emitting Rust module path unless the guest passes an explicit `target`.
`[policy] log_print_level` gates the host record alone, and the process subscriber applies `[engine] log_level` (default `info`) on top of it, so a target lifted to `debug` also needs the engine level at `debug` before it reaches a terminal.
A component that consistently traps on fuel exhaustion is a bug, not a tuning miss.

## 9. Runbook

Tail one component:

```bash
journalctl -u nexum -f --output=json \
  | jq 'select(.MESSAGE | fromjson? | .module == "twap-monitor")'
```

Recover a poisoned component: fix the underlying bug, rebuild the artifact, update the `[component].digest` pin and the entry's `[[modules]].digest` pin, then `sudo systemctl restart nexum`.
A component can also be poisoned by its event source: the log line `module poisoned - its event source is unrecoverable` tells the two causes apart.
In that case the component itself is healthy: `sudo systemctl restart nexum` alone recovers it, and ingestion resumes from the persisted cursor.
A `digest_mismatch` refusal names the pin that failed and the file it is in, so it says which of the two to edit.
The failure ring is in memory and clears at boot.
The engine reads `[[modules]]` at boot only, and it detects no artifact change while running, so adding, changing, or removing a component means editing `engine.toml` and restarting.
A logging-level change also needs a restart.

## 10. Pre-upgrade

- Read the changelog for breaking config or manifest changes.
- Diff the installed unit against the unit in section 2, apply the differences, and `sudo systemctl daemon-reload`.
  A `TimeoutStopSec` below the release's drain bound turns every stop into a SIGKILL with no log line.
- Cold-backup the local store (section 3).
- Stage the new binary, run it once against the production `engine.toml`, and confirm it boots before you stop it.
  Section 11 lists the refusals that trial boot can produce and the config each one needs.
- Swap the binary and `sudo systemctl restart nexum`.
- Watch `journalctl -u nexum -f` for new ERROR and WARN lines for at least 5 minutes.

## 11. Refusals an upgrade meets

Section 8 describes the `[policy]` resource and logging dials.
This section answers the other question an upgrade asks: which of them can refuse a boot that succeeded before, and what to write to get the earlier behaviour back on purpose.
Every refusal below is reachable from an `engine.toml` that ran under an earlier release.
The trial boot in section 10 surfaces all of them except the two that fire on an outbound request.

A ceiling breach means a component asked for more than the operator granted.
A validation refusal means the config does not resolve, so the engine refuses at load rather than running on a value it had to guess.
The spelling is `[policy.component.<id>]`, and `<id>` is the `[[modules]].id` of the entry the row binds to.

| Refusal | Kind | What to write |
| --- | --- | --- |
| `DigestUnpinned`: a `[[modules]]` entry carries no `digest` | Validation refusal, at load, before any compile | A `digest` on that entry, taken from `nexum digest <artifact>`. `[engine] require_component_digest = false` relaxes every entry at once. |
| `RetiredKey`: `[limits] fuel_per_event`, `memory_bytes` or `state_bytes` | Validation refusal, at load | The value under the `[policy]` key the message names. The old key refuses rather than being ignored, because a dead knob reads as an applied cap. |
| `CapabilityNotPermitted`: a manifest declares a dependency `[policy].capabilities` excludes | Ceiling breach, at boot | The capability name in `[policy].capabilities`, or in `[policy.component.<id>].capabilities`, which replaces that list for one component rather than extending it. An absent key permits every capability. |
| `ChainTriggerNotPermitted`: a block or event trigger under a capability set without `chain` | Ceiling breach, at boot | `"chain"` in the same list. A chain trigger delivers chain data without an import, so the same grant gates it. |
| `HttpRequestDenied`: a host outside the manifest `hosts` list intersected with `http_allow` | Ceiling breach, at the request | The host in `[policy.component.<id>].http_allow`, when that list is the side excluding it. An absent key leaves the manifest list as the only name gate, and no operator key widens past that list. |
| `DestinationIpProhibited`: a destination that resolves into a `[policy].http_deny` range, or into loopback or private space | Ceiling breach, at the request | A narrower range, or no range. The deny list subtracts after every allowlist, so no allow entry can override it. Loopback and private space are refused by a standing rule that only `[limits.http].permit_destinations` relaxes. |
| `InvalidHttpDeny`: a `[policy].http_deny` entry that is not an IP address or a CIDR block | Validation refusal, at load | An address or a CIDR block. A skipped deny entry would fail open. |
| `TotalMemoryExceeded`: summed reservations cross `[policy.total].max_memory_bytes` | Ceiling breach, at boot | A higher total, a lower per-component `max_memory_bytes`, or no total, which leaves the sum unbounded. The message names the entry that crossed it. |
| `ZeroField`: a `0` on a `[policy]` or `[policy.component.<id>]` numeric field | Validation refusal, at load | A positive value, or no key, which takes the default. `max_state_bytes` is the exception: `0` is legal and denies every local-store write. |
| `UnknownPolicyComponent`: a row keyed on an id no `[[modules]]` entry declares | Validation refusal, at load | The key corrected to a declared `[[modules]].id`. A narrowing row that binds to nothing fails open, so the engine refuses instead of ignoring it. |
| `EmptyComponentId`: a `[[modules]]` entry whose `id` is blank | Validation refusal, at load | A non-empty `id`, which is the `[policy.component]` join column. |
| `DuplicateComponentId`: two `[[modules]]` entries claim one `id` | Validation refusal, at load | A unique `id` per entry, or the policy join is ambiguous. |
| `LogRetentionTooStrict`: a console level louder than what retention keeps | Validation refusal, at load | A `log_retain_level` at least as loud as every console level, or a quieter `log_print_level` and no `[policy.log_targets]` row above it. Both default to `trace`, so this needs a retention level you set and a console or target level above it. |
| `InvalidLogLevel`: a value that is not one of the five level names | Validation refusal, at load | `trace`, `debug`, `info`, `warn` or `error`. |

Each breach at the request is also counted: `HttpRequestDenied` under `nexum_runtime_capability_denials_total{reason="allowlist"}`, and `DestinationIpProhibited` under `reason="destination"`.

The digest requirement is the one default that changed direction, so it fires on an `engine.toml` nobody edited.
`[engine].require_component_digest` defaults to `true`, and what it requires is the operator's pin on `[[modules]].digest`, not the author's pin in the manifest beside the artifact ([ADR-0025](adr/0025-the-required-digest-is-the-operator-pin.md)).
An upgrade therefore refuses at the first entry that carries no `digest`.
Section 1 gives the pin each entry needs and how to obtain it.
A single-wasm command-line override has no `[[modules]]` entry and stays exempt.

Omitting a `[policy.component.<id>]` row is a decision, not an oversight.
The component still takes the `[policy]` ceilings: 64 MiB of linear memory, 1e9 fuel per dispatch, 50 MiB of local-store bytes, an 8 KiB cap on one log record, and a 256-record burst that refills at 128 records per second.
It also still counts against `[policy.total].max_memory_bytes`.
What it does not take is a narrowing: an unset `capabilities` permits every capability the runtime supports, and an unset `http_allow` leaves the manifest `hosts` list as the only name-level gate.
An unnamed component is therefore bounded by capacity and unbounded by name.

`[engine] require_component_digest = false` is the only relaxation in the table that reaches more than one entry, and the only one that returns an earlier posture wholesale.
Write it deliberately or not at all: the relaxation is then auditable in the config, which is what a fail-closed default is for.

## References

- [ADR-0001](adr/0001-operator-config-separate-and-trusted.md): the operator config is separate and trusted.
- [ADR-0003](adr/0003-local-store-namespacing.md): local-store namespacing.
- [ADR-0014](adr/0014-local-store-durability-model.md): the local-store durability model.
- [ADR-0016](adr/0016-component-vocabulary.md): the `[component]` and `[dependencies]` vocabulary.
- [ADR-0024](adr/0024-blocks-are-clocks-and-host-keys-leave-the-module-namespace.md): logs replay, blocks do not, and host keys live outside a component's namespace.
- [ADR-0025](adr/0025-the-required-digest-is-the-operator-pin.md): the required digest is the operator pin.
- [Component lifecycle, trigger system, and packaging](02-modules-triggers-packaging.md).
