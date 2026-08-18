use std::net::IpAddr;
use std::num::NonZeroUsize;
use std::time::Duration;

use serde::Deserialize;

use crate::dispatch_rate::{
    DEFAULT_DISPATCH_BURST, DEFAULT_DISPATCH_REFILL_PER_SEC, DispatchRatePolicy,
};
use crate::poison_policy::{POISON_MAX_FAILURES, POISON_WINDOW, PoisonPolicy};

use super::error::{EngineConfigError, nonzero_secs, nonzero_u32, nonzero_usize, zero_field};
use super::nz_usize;

/// Default per-dispatch wall-clock deadline.
const DEFAULT_DISPATCH_DEADLINE: Duration = Duration::from_secs(120);

/// Added past the deadline to cover the final cursor commit and the
/// reconnect-task drain.
const SHUTDOWN_DRAIN_MARGIN: Duration = Duration::from_secs(30);

/// Default ceiling on the guest-settable connect timeout.
const DEFAULT_HTTP_CONNECT_TIMEOUT_MAX: Duration = Duration::from_secs(10);

/// Default ceiling on the guest-settable first-byte timeout.
const DEFAULT_HTTP_FIRST_BYTE_TIMEOUT_MAX: Duration = Duration::from_secs(30);

/// Default ceiling on the guest-settable between-bytes timeout.
const DEFAULT_HTTP_BETWEEN_BYTES_TIMEOUT_MAX: Duration = Duration::from_secs(30);

/// Default total deadline on one outgoing exchange, connect through body
/// streaming.
const DEFAULT_HTTP_TOTAL_DEADLINE: Duration = Duration::from_secs(60);

/// Default cap on one incoming response body (16 MiB).
const DEFAULT_HTTP_RESPONSE_BODY_MAX: u64 = 16 * 1024 * 1024;

/// Default cap on one chain JSON-RPC response body (1 MiB).
const DEFAULT_CHAIN_RESPONSE_MAX_BYTES: NonZeroUsize = nz_usize(1024 * 1024);

/// Ceiling for the `[limits.http]` millisecond knobs (24 h).
const HTTP_LIMIT_MS_MAX: u64 = 86_400_000;

/// Default per-run log ring budget (256 KiB).
const DEFAULT_LOG_BYTES_PER_RUN: NonZeroUsize = nz_usize(256 * 1024);

/// Default past runs retained per module (16).
const DEFAULT_LOG_RUNS_RETAINED: NonZeroUsize = nz_usize(16);

/// Serde shape of `[limits]`. Every field is optional; conversion into
/// [`ResolvedModuleLimits`] fills the built-in defaults and refuses
/// zeroes. Sections are documented on their own types.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleLimits {
    // Retired into `[policy]`; deserialized only to refuse naming the
    // replacement.
    pub(crate) fuel_per_event: Option<toml::Value>,
    pub(crate) memory_bytes: Option<toml::Value>,
    pub(crate) state_bytes: Option<toml::Value>,
    /// `[limits.http]`.
    #[serde(default)]
    pub http: HttpLimitsSection,
    /// `[limits.chain]`.
    #[serde(default)]
    pub chain: ChainLimitsSection,
    /// `[limits.logs]`.
    #[serde(default)]
    pub logs: LogLimitsSection,
    /// `[limits.poison]`.
    #[serde(default)]
    pub poison: PoisonLimitsSection,
    /// `[limits.dispatch]`.
    #[serde(default)]
    pub dispatch: DispatchLimitsSection,
    /// `[limits.shutdown]`.
    #[serde(default)]
    pub shutdown: ShutdownLimitsSection,
}

/// `[limits]` resolved once at load: every optional knob replaced by its
/// override or built-in default. The [`TryFrom<ModuleLimits>`] conversion
/// refuses zeroes, so no consumer clamps on read.
#[derive(Debug, Clone)]
pub struct ResolvedModuleLimits {
    /// Wall-clock deadline for a dispatch, covering host-call time fuel
    /// cannot meter.
    pub dispatch_deadline: Duration,
    /// Bound on the shutdown drain of the in-flight dispatch; defaults to
    /// `dispatch_deadline` plus a margin so an untuned drain outlasts the
    /// one call left in flight.
    pub shutdown_drain: Duration,
    /// Cap on one chain JSON-RPC response body.
    pub chain_response_max_bytes: NonZeroUsize,
    /// Outbound wasi:http limits.
    pub http: OutboundHttpLimits,
    /// Addresses the operator permits despite falling in a refused range.
    /// Empty by default, so every refused range stays refused.
    pub http_permit_destinations: Vec<IpAddr>,
    /// Per-run log retention limits.
    pub logs: LogRetentionLimits,
    /// Poison-pill quarantine thresholds.
    pub poison: PoisonPolicy,
    /// Per-module dispatch rate-limit policy.
    pub dispatch: DispatchRatePolicy,
}

impl Default for ResolvedModuleLimits {
    fn default() -> Self {
        match Self::try_from(ModuleLimits::default()) {
            Ok(resolved) => resolved,
            Err(_) => unreachable!("the built-in limit defaults are non-zero"),
        }
    }
}

/// Millisecond knob resolved to a `Duration`: zero refused, a value above
/// [`HTTP_LIMIT_MS_MAX`] saturates down so timer arithmetic cannot
/// overflow at request time.
fn nonzero_ms_capped(
    field: &str,
    value: Option<u64>,
    default: Duration,
) -> Result<Duration, EngineConfigError> {
    match value {
        Some(0) => Err(zero_field(field)),
        Some(ms) => Ok(Duration::from_millis(ms.min(HTTP_LIMIT_MS_MAX))),
        None => Ok(default),
    }
}

impl TryFrom<ModuleLimits> for ResolvedModuleLimits {
    type Error = EngineConfigError;

    /// Refuses every zero that would disable the mechanism its field
    /// bounds; any other override resolves to exactly the value written.
    fn try_from(raw: ModuleLimits) -> Result<Self, EngineConfigError> {
        for (present, key, replacement) in [
            (
                raw.fuel_per_event.is_some(),
                "limits.fuel_per_event",
                "policy.max_fuel_per_dispatch",
            ),
            (
                raw.memory_bytes.is_some(),
                "limits.memory_bytes",
                "policy.max_memory_bytes",
            ),
            (
                raw.state_bytes.is_some(),
                "limits.state_bytes",
                "policy.max_state_bytes",
            ),
        ] {
            if present {
                return Err(EngineConfigError::RetiredKey { key, replacement });
            }
        }
        let http = OutboundHttpLimits {
            connect_timeout_max: nonzero_ms_capped(
                "limits.http.connect_timeout_max_ms",
                raw.http.connect_timeout_max_ms,
                DEFAULT_HTTP_CONNECT_TIMEOUT_MAX,
            )?,
            first_byte_timeout_max: nonzero_ms_capped(
                "limits.http.first_byte_timeout_max_ms",
                raw.http.first_byte_timeout_max_ms,
                DEFAULT_HTTP_FIRST_BYTE_TIMEOUT_MAX,
            )?,
            between_bytes_timeout_max: nonzero_ms_capped(
                "limits.http.between_bytes_timeout_max_ms",
                raw.http.between_bytes_timeout_max_ms,
                DEFAULT_HTTP_BETWEEN_BYTES_TIMEOUT_MAX,
            )?,
            total_deadline: nonzero_ms_capped(
                "limits.http.total_deadline_ms",
                raw.http.total_deadline_ms,
                DEFAULT_HTTP_TOTAL_DEADLINE,
            )?,
            // Zero stays legal: a zero cap refuses every response body,
            // which is an enforceable operator choice, not a wedge.
            response_body_max_bytes: raw
                .http
                .response_body_max_bytes
                .unwrap_or(DEFAULT_HTTP_RESPONSE_BODY_MAX),
        };
        let logs = LogRetentionLimits {
            bytes_per_run: nonzero_usize(
                "limits.logs.bytes_per_run",
                raw.logs.bytes_per_run,
                DEFAULT_LOG_BYTES_PER_RUN,
            )?,
            runs_retained: nonzero_usize(
                "limits.logs.runs_retained",
                raw.logs.runs_retained,
                DEFAULT_LOG_RUNS_RETAINED,
            )?,
        };
        let poison = PoisonPolicy::new(
            nonzero_u32(
                "limits.poison.max_failures",
                raw.poison.max_failures,
                POISON_MAX_FAILURES,
            )?,
            nonzero_secs(
                "limits.poison.window_secs",
                raw.poison.window_secs,
                POISON_WINDOW,
            )?,
        );
        let dispatch = DispatchRatePolicy::new(
            nonzero_u32(
                "limits.dispatch.burst",
                raw.dispatch.burst,
                DEFAULT_DISPATCH_BURST,
            )?,
            nonzero_u32(
                "limits.dispatch.refill_per_sec",
                raw.dispatch.refill_per_sec,
                DEFAULT_DISPATCH_REFILL_PER_SEC,
            )?,
        );
        let dispatch_deadline = nonzero_secs(
            "limits.dispatch.deadline_secs",
            raw.dispatch.deadline_secs,
            DEFAULT_DISPATCH_DEADLINE,
        )?;
        Ok(Self {
            dispatch_deadline,
            shutdown_drain: nonzero_secs(
                "limits.shutdown.drain_secs",
                raw.shutdown.drain_secs,
                // Saturate: `Add` panics near `u64::MAX` seconds, and the
                // default is computed even under an explicit override.
                dispatch_deadline.saturating_add(SHUTDOWN_DRAIN_MARGIN),
            )?,
            chain_response_max_bytes: nonzero_usize(
                "limits.chain.response_body_max_bytes",
                raw.chain.response_body_max_bytes.map(|b| b as usize),
                DEFAULT_CHAIN_RESPONSE_MAX_BYTES,
            )?,
            http,
            http_permit_destinations: raw.http.permit_destinations,
            logs,
            poison,
            dispatch,
        })
    }
}

/// `[limits.http]` outbound limits. All optional; a zero millisecond value
/// refuses at load, one above 24 h saturates down. The `*_timeout_max_ms`
/// fields are ceilings on the matching guest-settable `request-options`
/// timeouts: a higher guest value is clamped down, an unset one inherits
/// the ceiling.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpLimitsSection {
    /// Ceiling on the guest-settable connect timeout, in milliseconds.
    pub connect_timeout_max_ms: Option<u64>,
    /// Ceiling on the guest-settable first-byte timeout, in milliseconds.
    pub first_byte_timeout_max_ms: Option<u64>,
    /// Ceiling on the guest-settable between-bytes timeout, in milliseconds.
    pub between_bytes_timeout_max_ms: Option<u64>,
    /// Deadline on one whole exchange, in milliseconds.
    pub total_deadline_ms: Option<u64>,
    /// Cap on one incoming response body, in bytes.
    pub response_body_max_bytes: Option<u64>,
    /// Addresses in otherwise-refused ranges that this deployment permits.
    /// The operator writes these; a module manifest cannot add to them.
    #[serde(default)]
    pub permit_destinations: Vec<IpAddr>,
}

/// `[limits.chain]` chain JSON-RPC response size limit. Optional; defaults
/// to 1 MiB.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainLimitsSection {
    /// Cap on one chain JSON-RPC response body, in bytes.
    pub response_body_max_bytes: Option<u64>,
}

/// Resolved outbound HTTP limits the wasi:http gate enforces per request.
#[derive(Debug, Clone, Copy)]
pub struct OutboundHttpLimits {
    /// Ceiling on the guest-settable connect timeout.
    pub connect_timeout_max: Duration,
    /// Ceiling on the guest-settable first-byte timeout.
    pub first_byte_timeout_max: Duration,
    /// Ceiling on the guest-settable between-bytes timeout.
    pub between_bytes_timeout_max: Duration,
    /// Total deadline on one exchange, connect through body streaming.
    pub total_deadline: Duration,
    /// Cap on one incoming response body.
    pub response_body_max_bytes: u64,
}

/// `[limits.logs]` per-run retention knobs. Both optional; a zero refuses
/// at load.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogLimitsSection {
    /// Byte budget for one run's in-memory ring.
    pub bytes_per_run: Option<usize>,
    /// Number of past runs retained per module.
    pub runs_retained: Option<usize>,
}

/// `[limits.poison]` quarantine thresholds. Both optional; a zero refuses
/// at load. A module reaching `max_failures` traps within
/// a sliding `window_secs` is quarantined and no longer dispatched until
/// an operator-driven engine restart.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoisonLimitsSection {
    /// Maximum traps within the window before a module is poisoned.
    pub max_failures: Option<u32>,
    /// Sliding window the traps are counted across, in seconds.
    pub window_secs: Option<u64>,
}

/// `[limits.dispatch]` per-module dispatch knobs. All optional; omitted
/// values resolve to the production defaults, and a zero refuses at load.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchLimitsSection {
    /// Burst allowance: the token-bucket capacity.
    pub burst: Option<u32>,
    /// Sustained dispatch ceiling: tokens replenished per second.
    pub refill_per_sec: Option<u32>,
    /// Wall-clock deadline (s) for a dispatch, covering host-call time
    /// fuel cannot meter.
    pub deadline_secs: Option<u64>,
}

/// `[limits.shutdown]`. Process-scoped, unlike its `[limits.dispatch]`
/// neighbours: there is one process and one stop.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownLimitsSection {
    /// Bound (s) on the drain of the in-flight dispatch.
    pub drain_secs: Option<u64>,
}

/// Resolved log retention limits the in-memory store enforces. Non-zero
/// by type: a zero budget would retain nothing.
#[derive(Debug, Clone, Copy)]
pub struct LogRetentionLimits {
    /// Byte budget for one run's ring; the newest record is never evicted
    /// to nothing.
    pub bytes_per_run: NonZeroUsize,
    /// Runs retained per module; the oldest run evicts first.
    pub runs_retained: NonZeroUsize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EngineConfig, RawEngineConfig};

    #[test]
    fn http_limits_default_when_absent() {
        let http = ResolvedModuleLimits::default().http;
        assert_eq!(http.connect_timeout_max, Duration::from_secs(10));
        assert_eq!(http.first_byte_timeout_max, Duration::from_secs(30));
        assert_eq!(http.between_bytes_timeout_max, Duration::from_secs(30));
        assert_eq!(http.total_deadline, Duration::from_secs(60));
        assert_eq!(http.response_body_max_bytes, 16 * 1024 * 1024);
    }

    #[test]
    fn http_limits_parse_with_partial_overrides() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.http]
connect_timeout_max_ms  = 5_000
total_deadline_ms       = 90_000
response_body_max_bytes = 1_024
"#,
        )
        .expect("limits.http parses");
        let http = cfg.limits.http;
        assert_eq!(http.connect_timeout_max, Duration::from_millis(5_000));
        assert_eq!(http.total_deadline, Duration::from_millis(90_000));
        assert_eq!(http.response_body_max_bytes, 1_024);
        // Unset fields keep the built-in defaults.
        assert_eq!(http.first_byte_timeout_max, Duration::from_secs(30));
        assert_eq!(http.between_bytes_timeout_max, Duration::from_secs(30));
    }

    #[test]
    fn permit_destinations_defaults_to_empty_and_parses_both_families() {
        let bare: EngineConfig = toml::from_str("[limits]\n").expect("bare limits parse");
        assert!(
            bare.limits.http_permit_destinations.is_empty(),
            "an absent list permits nothing"
        );

        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.http]
permit_destinations = ["10.0.5.7", "::1"]
"#,
        )
        .expect("permit_destinations parses");
        assert_eq!(
            cfg.limits.http_permit_destinations,
            vec![
                IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 5, 7)),
                IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            ]
        );
    }

    #[test]
    fn permit_destinations_refuses_a_value_that_is_not_an_address() {
        let err = toml::from_str::<EngineConfig>(
            r#"
[limits.http]
permit_destinations = ["10.0.5.0/24"]
"#,
        )
        .expect_err("a CIDR range is not an address");
        // Serde's own type mismatch, not a message we threaded through it.
        assert!(err.to_string().contains("permit_destinations"), "{err}");
    }

    #[test]
    fn chain_limits_default_when_absent() {
        assert_eq!(
            ResolvedModuleLimits::default()
                .chain_response_max_bytes
                .get(),
            1024 * 1024,
        );
    }

    #[test]
    fn chain_limits_parse_with_override() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.chain]
response_body_max_bytes = 2_048
"#,
        )
        .expect("limits.chain parses");
        assert_eq!(cfg.limits.chain_response_max_bytes.get(), 2_048);
    }

    /// A retired `[limits]` scalar refuses and the message names the
    /// `[policy]` key that replaces it.
    #[test]
    fn a_retired_limits_scalar_refuses_naming_the_replacement() {
        for (toml, key, replacement) in [
            (
                "[limits]\nfuel_per_event = 7\n",
                "limits.fuel_per_event",
                "policy.max_fuel_per_dispatch",
            ),
            (
                "[limits]\nmemory_bytes = 7\n",
                "limits.memory_bytes",
                "policy.max_memory_bytes",
            ),
            (
                "[limits]\nstate_bytes = 7\n",
                "limits.state_bytes",
                "policy.max_state_bytes",
            ),
        ] {
            let raw = toml::from_str::<RawEngineConfig>(toml)
                .expect("the raw parse only decides the TOML is well formed");
            let err = EngineConfig::try_from(raw).expect_err("a retired key must not validate");
            assert!(
                matches!(
                    err,
                    EngineConfigError::RetiredKey { key: k, replacement: r }
                        if k == key && r == replacement
                ),
                "{key}: {err:?}",
            );
            let err = toml::from_str::<EngineConfig>(toml).expect_err("must not parse");
            assert!(err.to_string().contains(replacement), "{key}: {err}");
        }
    }

    #[test]
    fn http_limits_saturate_a_millisecond_value_above_the_ceiling() {
        // u64::MAX would overflow timer arithmetic at request time, so it
        // saturates down to the 24 h ceiling at load.
        let limits = ModuleLimits {
            http: HttpLimitsSection {
                total_deadline_ms: Some(u64::MAX),
                ..Default::default()
            },
            ..Default::default()
        };
        let resolved = ResolvedModuleLimits::try_from(limits).expect("saturating value resolves");
        assert_eq!(
            resolved.http.total_deadline,
            Duration::from_millis(86_400_000)
        );
    }

    #[test]
    fn log_limits_default_when_absent() {
        let logs = ResolvedModuleLimits::default().logs;
        assert_eq!(logs.bytes_per_run.get(), 256 * 1024);
        assert_eq!(logs.runs_retained.get(), 16);
    }

    #[test]
    fn log_limits_parse_with_overrides() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.logs]
bytes_per_run = 4_096
runs_retained = 3
"#,
        )
        .expect("limits.logs parses");
        let logs = cfg.limits.logs;
        assert_eq!(logs.bytes_per_run.get(), 4_096);
        assert_eq!(logs.runs_retained.get(), 3);
    }

    #[test]
    fn poison_limits_default_when_absent() {
        let poison = ResolvedModuleLimits::default().poison;
        assert_eq!(poison.max_failures, POISON_MAX_FAILURES);
        assert_eq!(poison.window, POISON_WINDOW);
    }

    #[test]
    fn poison_limits_parse_with_overrides() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.poison]
max_failures = 3
window_secs  = 60
"#,
        )
        .expect("limits.poison parses");
        let poison = cfg.limits.poison;
        assert_eq!(poison.max_failures.get(), 3);
        assert_eq!(poison.window, Duration::from_secs(60));
    }

    #[test]
    fn dispatch_rate_default_when_absent() {
        let policy = ResolvedModuleLimits::default().dispatch;
        assert_eq!(policy.capacity, DEFAULT_DISPATCH_BURST);
        assert_eq!(policy.refill_per_sec, DEFAULT_DISPATCH_REFILL_PER_SEC);
    }

    /// A drain shorter than the deadline it drains loses the in-flight
    /// dispatch's cursor commit on SIGTERM.
    #[test]
    fn shutdown_drain_defaults_past_the_dispatch_deadline() {
        let resolved = ResolvedModuleLimits::default();
        assert_eq!(
            resolved.shutdown_drain,
            resolved.dispatch_deadline + Duration::from_secs(30),
        );

        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.dispatch]
deadline_secs = 300
"#,
        )
        .expect("limits.dispatch parses");
        assert_eq!(cfg.limits.shutdown_drain, Duration::from_secs(330));
    }

    #[test]
    fn shutdown_drain_parses_with_override() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.shutdown]
drain_secs = 45
"#,
        )
        .expect("limits.shutdown parses");
        assert_eq!(cfg.limits.shutdown_drain, Duration::from_secs(45));

        toml::from_str::<EngineConfig>("[limits.shutdown]\ndrain_secs = 0\n")
            .expect_err("a zero drain would forbid the final flush");

        toml::from_str::<EngineConfig>("[limits.dispatch]\nshutdown_drain_secs = 45\n")
            .expect_err("the retired spelling refuses under deny_unknown_fields");
    }

    /// The drain default is computed even under an override, so a maximal
    /// deadline must saturate rather than panic on the added margin.
    #[test]
    fn a_maximal_deadline_saturates_the_drain_default() {
        let toml_max = format!("[limits.dispatch]\ndeadline_secs = {}\n", u64::MAX);
        let cfg: EngineConfig = toml::from_str(&toml_max).expect("a maximal deadline parses");
        assert_eq!(cfg.limits.shutdown_drain, Duration::MAX);

        let cfg: EngineConfig =
            toml::from_str(&format!("{toml_max}\n[limits.shutdown]\ndrain_secs = 60\n"))
                .expect("an explicit drain beside a maximal deadline parses");
        assert_eq!(cfg.limits.shutdown_drain, Duration::from_secs(60));
    }

    #[test]
    fn dispatch_rate_parse_with_overrides() {
        let cfg: EngineConfig = toml::from_str(
            r#"
[limits.dispatch]
burst          = 8
refill_per_sec = 4
"#,
        )
        .expect("limits.dispatch parses");
        let policy = cfg.limits.dispatch;
        assert_eq!(policy.capacity.get(), 8);
        assert_eq!(policy.refill_per_sec.get(), 4);
    }
}
