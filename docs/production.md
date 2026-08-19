# Production deployment

Operator handbook for running the `nexum` engine in production: systemd unit, state backup, and observability wiring.
The engine is the bare binary from `crates/nexum-cli`.
A downstream composition root that registers extensions runs the same way, under its own binary name.

## 1. Pre-flight

- The engine built in release: `cargo build -p nexum-cli --release` gives `target/release/nexum`.
- Every component `.wasm` artifact present on a path the service user can read.
- An `engine.toml` with `state_dir` on a persistent path (never `/tmp`), `log_level = "info"`, `[engine.metrics] enabled = true` with `bind_addr = "127.0.0.1:9100"`, one `[chains.<id>]` per triggered chain with a paid RPC URL, and one `[[modules]]` per module, each with an operator-written `id`.
- `require_component_digest = true` under `[engine]`, with every manifest carrying a `[component].digest` pin.
- A `digest` on each `[[modules]]` entry, set to the artifact's sha256.
  This is the operator's own pin, in trusted config: the default sibling manifest lives in the same trust domain as the artifact, so its `[component].digest` does not hold against a compromised artifact store.
  An operator-owned manifest outside the artifact directory, named by the `manifest` key on the entry and combined with `require_component_digest = true`, closes the same gap; `[[modules]].digest` is the direct form and needs no separate manifest path.
  Both pins are verified against the exact bytes handed to the compiler, and a mismatch refuses the boot naming which pin failed and the file it is in.
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

The runtime writes one kind of key, best-effort after a successful dispatch, under a host-owned `host/<name>` namespace beside each component's own:

- `chainlog_cursor:<hex>`, the resume cursor for a `resume = true` event trigger.
  The engine reads it once at boot, re-opens at that block, and backfills the gap on reconnect, capped by `max_lookback`.
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

`/metrics` binds when `[engine.metrics] enabled = true`.
Always bind loopback and never `0.0.0.0`: Prometheus scrapes over the loopback or the container network.
The bare `nexum` binary registers the exporter itself, through the `CoreRuntime` preset.
With `enabled = false` the recorder is still installed, so call sites stay live, but no listener binds and no sample is readable.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `nexum_runtime_boot_refusals_total` | counter | `error_kind` | Boot refusals by error kind. |
| `nexum_runtime_dispatch_latency_seconds` | histogram | `module`, `trigger_kind` | Wall-clock seconds to dispatch one trigger. |
| `nexum_runtime_dispatch_dropped_total` | counter | `module`, `trigger_kind`, `reason` | Triggers dropped before dispatch. `reason = "rate_limited"` is the per-component dispatch rate limit (`[limits.dispatch]`, default `burst = 256` and `refill_per_sec = 128`). `reason = "shutdown"` is a stop landing mid fan-out: the fan-out follows `[[modules]]` order, so the same trailing modules are skipped at every stop. A block is not replayed; an event is, from its cursor. |
| `nexum_runtime_module_errors_total` | counter | `module`, `error_kind` | Module faults and traps. `error_kind = "trap"` is a wasmtime trap; other values are fault labels. |
| `nexum_runtime_module_restarts_total` | counter | `module` | Module restart attempts. |
| `nexum_runtime_module_poisoned` | gauge | `module` | `1` once a module crosses `[limits.poison]` (default 5 failures in 600 s). Stays `1` until the process restarts. |
| `nexum_runtime_chain_request_total` | counter | `chain_id`, `method`, `outcome` | Every `chain::request`. A method outside the read surface is counted as `method="<denied>"` with `outcome="err"`. The `outcome="err"` rate is the RPC-degraded signal. |
| `nexum_runtime_chain_response_capped_total` | counter | `chain_id`, `method` | Responses rejected for exceeding `[limits.chain] response_body_max_bytes` (default 1 MiB). |
| `nexum_runtime_source_reconnects_total` | counter | `source_kind`, `chain_id`, `module` | Source reconnects. `source_kind="block"` is per chain; `source_kind="chain-log"` also carries `module`. |

`crates/nexum-runtime-metrics/src/lib.rs` is the single source of the name set, and a test refuses any emitted name the table does not carry.

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

      - alert: NexumDown
        expr: up{job="nexum"} == 0
        for: 2m
        labels: { severity: page }
        annotations:
          summary: "Nexum is down (metrics scrape failing)"
```

`page` wakes on-call for a poisoned component or a down engine; `ticket` routes during business hours.

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

Resource ceilings live in `engine.toml` `[policy]` and apply to every component; a `[policy.component.<id>]` row, keyed on `[[modules]].id`, overrides them for one.
`[policy.total].max_memory_bytes` bounds the summed reservations, and an overcommitted set refuses at boot naming the entry that crossed it.
A `[component.resources]` field in a manifest narrows a ceiling for one component and can never widen it.
A component that consistently traps on fuel exhaustion is a bug, not a tuning miss.

## 9. Runbook

Tail one component:

```bash
journalctl -u nexum -f --output=json \
  | jq 'select(.MESSAGE | fromjson? | .fields.module == "twap-monitor")'
```

Recover a poisoned component: fix the underlying bug, rebuild the artifact, update the `[component].digest` pin and the entry's `[[modules]].digest` pin, then `sudo systemctl restart nexum`.
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
- Swap the binary and `sudo systemctl restart nexum`.
- Watch `journalctl -u nexum -f` for new ERROR and WARN lines for at least 5 minutes.

## References

- [ADR-0001](adr/0001-operator-config-separate-and-trusted.md): the operator config is separate and trusted.
- [ADR-0003](adr/0003-local-store-namespacing.md): local-store namespacing.
- [ADR-0014](adr/0014-local-store-durability-model.md): the local-store durability model.
- [ADR-0016](adr/0016-component-vocabulary.md): the `[component]` and `[dependencies]` vocabulary.
- [ADR-0024](adr/0024-blocks-are-clocks-and-host-keys-leave-the-module-namespace.md): logs replay, blocks do not, and host keys live outside a component's namespace.
- [Component lifecycle, trigger system, and packaging](02-modules-triggers-packaging.md).
