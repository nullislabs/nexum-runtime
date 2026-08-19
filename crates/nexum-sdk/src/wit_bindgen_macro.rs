//! Declarative macro generating the `WitBindgenHost` adapter a module
//! ships in `lib.rs`: `struct WitBindgenHost;` plus the core trait
//! impls and the fault, chain-error, and level conversions.
//!
//! Capability-selected: `caps: [...]` emits only the pieces backed by
//! the listed capabilities (how `#[nexum_sdk::module]` invokes it); the
//! zero-argument form emits the full core set. Either way the
//! wit-bindgen output for the world must already be in scope, so
//! selecting a capability the world does not import is a compile error.
//! A domain SDK layers its own interfaces on the same `WitBindgenHost`,
//! or binds logging alone via [`crate::bind_host_logging_via_wit_bindgen!`].
//!
//! ```ignore
//! wit_bindgen::generate!({ /* ... */ });
//! nexum_sdk::bind_host_via_wit_bindgen!();
//! // or capability-selected:
//! nexum_sdk::bind_host_via_wit_bindgen!(caps: [chain, logging]);
//! // Call `install_tracing()` once at the top of `Guest::init`.
//! ```

/// Generate `WitBindgenHost`, the `*Host` trait impls, and the error /
/// level `From` impls for the selected capabilities. See module docs.
///
/// The generated names `WitBindgenHost`, `convert_chain_err`,
/// `HostLogSink`, and `install_tracing` are visible in the caller's
/// scope (`macro_rules!` is not hygienic for items).
#[macro_export]
macro_rules! bind_host_via_wit_bindgen {
    // Blanket-world form: every core interface is in scope, emit the
    // full adapter.
    () => {
        $crate::bind_host_via_wit_bindgen!(caps: [chain, local_store, logging]);
    };
    // Capability-selected form: the base pieces (which need only the
    // always-present `nexum:host/types`) plus one block per listed
    // capability.
    (caps: [$($cap:ident),* $(,)?]) => {
        /// Wraps the module's per-cdylib wit-bindgen imports. Carries a
        /// trait impl for each declared capability, so a module binds the
        /// seams it uses and not the composed `Host`.
        struct WitBindgenHost;

        /// Lift the wit-bindgen `types.fault` into the SDK's `Fault`.
        impl ::core::convert::From<nexum::host::types::Fault> for $crate::host::Fault {
            fn from(f: nexum::host::types::Fault) -> Self {
                match f {
                    nexum::host::types::Fault::Unsupported(s) => Self::Unsupported(s),
                    nexum::host::types::Fault::Unavailable(s) => Self::Unavailable(s),
                    nexum::host::types::Fault::Denied(s) => Self::Denied(s),
                    nexum::host::types::Fault::RateLimited => Self::RateLimited,
                    nexum::host::types::Fault::Timeout => Self::Timeout,
                    nexum::host::types::Fault::InvalidInput(s) => Self::InvalidInput(s),
                    nexum::host::types::Fault::Internal(s) => Self::Internal(s),
                }
            }
        }

        /// Lower the SDK `Fault` back into the wit-bindgen `Fault` for
        /// the export signature, via the closed `FaultParts` mirror so
        /// the match stays exhaustive: a future SDK case fails to
        /// compile instead of degrading to `internal`.
        impl ::core::convert::From<$crate::host::Fault> for nexum::host::types::Fault {
            fn from(f: $crate::host::Fault) -> Self {
                match $crate::host::FaultParts::from(f) {
                    $crate::host::FaultParts::Unsupported(s) => Self::Unsupported(s),
                    $crate::host::FaultParts::Unavailable(s) => Self::Unavailable(s),
                    $crate::host::FaultParts::Denied(s) => Self::Denied(s),
                    $crate::host::FaultParts::RateLimited => Self::RateLimited,
                    $crate::host::FaultParts::Timeout => Self::Timeout,
                    $crate::host::FaultParts::InvalidInput(s) => Self::InvalidInput(s),
                    $crate::host::FaultParts::Internal(s) => Self::Internal(s),
                }
            }
        }

        /// Rebuild the native alloy log from the wit-bindgen `log`
        /// record; assembly lives in `nexum_sdk::sol_events`.
        impl ::core::convert::From<nexum::host::types::Log> for $crate::sol_events::Log {
            fn from(log: nexum::host::types::Log) -> Self {
                $crate::sol_events::LogParts {
                    chain_id: log.chain_id,
                    address: &log.address,
                    topics: &log.topics,
                    data: &log.data,
                    block_hash: log.block_hash.as_deref(),
                    block_number: log.block_number,
                    block_timestamp: log.block_timestamp,
                    transaction_hash: log.transaction_hash.as_deref(),
                    transaction_index: log.transaction_index,
                    log_index: log.log_index,
                    removed: log.removed,
                }
                .into()
            }
        }


        $($crate::__bind_host_cap_via_wit_bindgen!($cap);)*
    };
}

/// One capability's slice of the `WitBindgenHost` adapter. Invoked by
/// [`bind_host_via_wit_bindgen!`]; not part of the public surface.
#[doc(hidden)]
#[macro_export]
macro_rules! __bind_host_cap_via_wit_bindgen {
    (chain) => {
        impl $crate::host::ChainHost for WitBindgenHost {
            fn request(
                &self,
                chain_id: u64,
                method: &str,
                params: &str,
            ) -> ::core::result::Result<::std::string::String, $crate::host::ChainError> {
                nexum::host::chain::request(chain_id, method, params).map_err(convert_chain_err)
            }
        }

        /// Lift the wit-bindgen `chain.chain-error` into the SDK's
        /// host-neutral `ChainError`.
        fn convert_chain_err(e: nexum::host::chain::ChainError) -> $crate::host::ChainError {
            match e {
                nexum::host::chain::ChainError::Fault(f) => {
                    $crate::host::ChainError::Fault(::core::convert::Into::into(f))
                }
                nexum::host::chain::ChainError::Rpc(r) => {
                    $crate::host::ChainError::Rpc($crate::host::RpcError {
                        code: r.code,
                        message: r.message,
                        data: r.data.map(::core::convert::Into::into),
                    })
                }
            }
        }
    };
    (local_store) => {
        impl $crate::host::LocalStoreHost for WitBindgenHost {
            fn get(
                &self,
                key: &str,
            ) -> ::core::result::Result<
                ::core::option::Option<::std::vec::Vec<u8>>,
                $crate::host::Fault,
            > {
                nexum::host::local_store::get(key).map_err($crate::host::Fault::from)
            }
            fn set(
                &self,
                key: &str,
                value: &[u8],
            ) -> ::core::result::Result<(), $crate::host::Fault> {
                nexum::host::local_store::set(key, value).map_err($crate::host::Fault::from)
            }
            fn delete(&self, key: &str) -> ::core::result::Result<(), $crate::host::Fault> {
                nexum::host::local_store::delete(key).map_err($crate::host::Fault::from)
            }
            // Overrides the trait's per-op fallback with the host's
            // atomic batch verb.
            fn apply(
                &self,
                ops: &[$crate::host::WriteOp],
            ) -> ::core::result::Result<(), $crate::host::Fault> {
                let ops: ::std::vec::Vec<nexum::host::local_store::WriteOp> = ops
                    .iter()
                    .map(|op| match op {
                        $crate::host::WriteOp::Set { key, value } => {
                            nexum::host::local_store::WriteOp::Set(
                                nexum::host::local_store::KeyValue {
                                    key: ::std::clone::Clone::clone(key),
                                    value: ::std::clone::Clone::clone(value),
                                },
                            )
                        }
                        $crate::host::WriteOp::Delete { key } => {
                            nexum::host::local_store::WriteOp::Delete(::std::clone::Clone::clone(
                                key,
                            ))
                        }
                    })
                    .collect();
                nexum::host::local_store::apply(&ops).map_err($crate::host::Fault::from)
            }
            fn list_keys(
                &self,
                prefix: &str,
            ) -> ::core::result::Result<::std::vec::Vec<::std::string::String>, $crate::host::Fault>
            {
                nexum::host::local_store::list_keys(prefix).map_err($crate::host::Fault::from)
            }
            fn contains(&self, key: &str) -> ::core::result::Result<bool, $crate::host::Fault> {
                nexum::host::local_store::contains(key).map_err($crate::host::Fault::from)
            }
            fn len(
                &self,
                key: &str,
            ) -> ::core::result::Result<::core::option::Option<u64>, $crate::host::Fault> {
                nexum::host::local_store::len(key).map_err($crate::host::Fault::from)
            }
            fn count(&self, prefix: &str) -> ::core::result::Result<u64, $crate::host::Fault> {
                nexum::host::local_store::count(prefix).map_err($crate::host::Fault::from)
            }
        }
    };
    (logging) => {
        $crate::bind_host_logging_via_wit_bindgen!();

        impl $crate::host::LoggingHost for WitBindgenHost {
            fn log(&self, level: $crate::Level, message: &str) {
                nexum::host::logging::log(nexum::host::logging::Level::from(level), message);
            }
        }
    };
}

/// Logging-only slice of [`bind_host_via_wit_bindgen!`]: needs only the
/// generated `nexum::host::logging` in scope, never `nexum::host::types`
/// or `WitBindgenHost`.
///
/// The generated names `HostLogSink` and `install_tracing` are visible
/// in the caller's scope (`macro_rules!` is not hygienic for items).
#[macro_export]
macro_rules! bind_host_logging_via_wit_bindgen {
    () => {
        /// Translate a `tracing_core::Level` into the wit-bindgen
        /// `logging::Level` wire enum.
        impl ::core::convert::From<$crate::Level> for nexum::host::logging::Level {
            fn from(level: $crate::Level) -> Self {
                if level == $crate::Level::ERROR {
                    Self::Error
                } else if level == $crate::Level::WARN {
                    Self::Warn
                } else if level == $crate::Level::INFO {
                    Self::Info
                } else if level == $crate::Level::DEBUG {
                    Self::Debug
                } else {
                    Self::Trace
                }
            }
        }

        /// Routes guest `tracing` events to the bound host logging call.
        struct HostLogSink;

        impl $crate::tracing::LogSink for HostLogSink {
            fn log(&self, level: $crate::Level, message: &str) {
                nexum::host::logging::log(::core::convert::From::from(level), message);
            }
        }

        /// Install the guest tracing facade and panic hook over the
        /// bound host logging call. Call once at the top of `Guest::init`.
        fn install_tracing() {
            $crate::tracing::init(HostLogSink);
        }
    };
}
