//! One role vocabulary for the load pass, the namespace ledger, the
//! metric names, and the compile-time tracing field keys.

use std::time::Duration;

use anyhow::Error;
use tracing::{error, info, warn};

use super::lifecycle::{Deferral, TrapVerdict};
use crate::digest::ContentDigest;
use crate::module_id::ModuleId;
use crate::runtime::poison_policy::PoisonPolicy;

/// Keys the per-role metric names and the compile-time tracing field keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::IntoStaticStr)]
pub(super) enum Role {
    Module,
    Adapter,
}

impl Role {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Adapter => "adapter",
        }
    }

    /// The manifest-facing spelling: an adapter entry loads a provider manifest.
    pub(super) const fn manifest_role(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Adapter => "provider",
        }
    }

    /// The ledger spelling: `engine.toml` names the section `[[adapters]]`.
    pub(super) const fn claim_role(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Adapter => "adapter",
        }
    }

    pub(super) const fn load_context(self) -> &'static str {
        match self {
            Self::Module => "load module",
            Self::Adapter => "load provider",
        }
    }

    pub(super) const fn errors_total(self) -> &'static str {
        match self {
            Self::Module => "nexum_runtime_module_errors_total",
            Self::Adapter => "nexum_runtime_adapter_errors_total",
        }
    }

    pub(super) const fn restarts_total(self) -> &'static str {
        match self {
            Self::Module => "nexum_runtime_module_restarts_total",
            Self::Adapter => "nexum_runtime_adapter_restarts_total",
        }
    }

    pub(super) const fn poisoned_gauge(self) -> &'static str {
        match self {
            Self::Module => "nexum_runtime_module_poisoned",
            Self::Adapter => "nexum_runtime_adapter_poisoned",
        }
    }

    /// A provider reinstall is a fresh instance, so its curve resets; a
    /// module recovers in place and keeps climbing.
    pub(super) const fn resets_failure_count(self) -> bool {
        matches!(self, Self::Adapter)
    }
}

/// A recorded trap: the death log, the error counter, and the poison
/// transition when the verdict crossed.
pub(super) fn report_trap(
    role: Role,
    name: &ModuleId,
    verdict: &TrapVerdict,
    policy: PoisonPolicy,
) {
    match role {
        Role::Module => warn!(
            module = %name,
            failure_count = verdict.failure_count,
            backoff_ms = verdict.backoff.as_millis() as u64,
            "module trapped - marked dead; will restart after backoff",
        ),
        Role::Adapter => warn!(
            adapter = %name,
            failure_count = verdict.failure_count,
            backoff_ms = verdict.backoff.as_millis() as u64,
            "adapter trapped - marked dead; will restart after backoff",
        ),
    }
    metrics::counter!(
        role.errors_total(),
        role.label() => name.clone(),
        "error_kind" => "trap",
    )
    .increment(1);
    if let Some(recent) = verdict.poisoned {
        report_poison(role, name, recent, policy.window, None);
    }
}

/// The poison-transition trio: quarantine warn plus gauge; `last_error`
/// rides only when the caller held the trap itself.
pub(super) fn report_poison(
    role: Role,
    name: &ModuleId,
    recent_failures: u32,
    window: Duration,
    last_error: Option<String>,
) {
    let window_secs = window.as_secs();
    match (role, last_error) {
        (Role::Module, Some(last_error)) => warn!(
            module = %name,
            recent_failures,
            window_secs,
            last_error,
            "module poisoned - quarantined; remove from engine.toml + restart to clear",
        ),
        (Role::Module, None) => warn!(
            module = %name,
            recent_failures,
            window_secs,
            "module poisoned - quarantined; remove from engine.toml + restart to clear",
        ),
        (Role::Adapter, Some(last_error)) => warn!(
            adapter = %name,
            recent_failures,
            window_secs,
            last_error,
            "adapter poisoned - quarantined; remove from engine.toml + restart to clear",
        ),
        (Role::Adapter, None) => warn!(
            adapter = %name,
            recent_failures,
            window_secs,
            "adapter poisoned - quarantined; remove from engine.toml + restart to clear",
        ),
    }
    metrics::gauge!(
        role.poisoned_gauge(),
        role.label() => name.clone(),
    )
    .set(1.0);
}

pub(super) fn report_restart_attempt(
    role: Role,
    name: &ModuleId,
    failure_count: u32,
    digest: ContentDigest,
) {
    match role {
        Role::Module => info!(
            module = %name,
            failure_count,
            digest = %digest,
            "restart attempt",
        ),
        Role::Adapter => info!(
            adapter = %name,
            failure_count,
            digest = %digest,
            "adapter restart attempt",
        ),
    }
    metrics::counter!(
        role.restarts_total(),
        role.label() => name.clone(),
    )
    .increment(1);
}

pub(super) fn report_restart_outcome(
    role: Role,
    name: &ModuleId,
    outcome: Result<(), (Deferral, Error)>,
) {
    match (role, outcome) {
        (Role::Module, Ok(())) => info!(module = %name, "restart succeeded"),
        (Role::Adapter, Ok(())) => info!(adapter = %name, "adapter restart succeeded"),
        (Role::Module, Err((deferral, e))) => {
            // A string field, not a `Display` one: it carries the full
            // context chain through the string visitor.
            let error = format!("{e:#}");
            error!(
                module = %name,
                failure_count = deferral.failure_count,
                backoff_ms = deferral.backoff.as_millis() as u64,
                error,
                "restart failed - will retry after backoff",
            );
        }
        (Role::Adapter, Err((deferral, e))) => {
            let error = format!("{e:#}");
            error!(
                adapter = %name,
                failure_count = deferral.failure_count,
                backoff_ms = deferral.backoff.as_millis() as u64,
                error,
                "adapter restart failed - will retry after backoff",
            );
        }
    }
}
