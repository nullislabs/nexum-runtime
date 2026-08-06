//! One role vocabulary for the load pass, the namespace ledger, the
//! metric names, and the compile-time tracing field keys.

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
