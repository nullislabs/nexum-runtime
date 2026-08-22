//! Host traits, the seam between module logic and the wit-bindgen
//! shims a module generates per-cdylib. Each trait mirrors one nexum
//! host interface ([`ChainHost`], [`LocalStoreHost`], [`LoggingHost`]);
//! [`Host`] bundles them all.
//!
//! Module logic written against these traits runs host-free against
//! the `nexum-sdk-test` mocks. The traits are world-neutral over this
//! module's [`Fault`], mirroring the per-module `Fault` that
//! `wit_bindgen::generate!` emits, so modules wire a one-line converter
//! between the two.

use alloy_primitives::Bytes;
use strum::IntoStaticStr;
use tracing_core::Level;

/// Shared cross-domain failure vocabulary, mirrored from
/// `nexum:host/types.fault`. Typed per-interface errors embed it as a
/// case so a caller recovers the structured cause. `#[non_exhaustive]`:
/// the WIT can grow a case.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[non_exhaustive]
pub enum Fault {
    /// Capability declared but not provisioned by the operator.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Capability temporarily unavailable (RPC down, etc).
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Capability declined the request (auth, allowlist, …).
    #[error("denied: {0}")]
    Denied(String),
    /// Rate-limited by an upstream service.
    #[error("rate limited")]
    RateLimited,
    /// Operation took too long.
    #[error("timeout")]
    Timeout,
    /// Caller-supplied input did not parse / validate.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Catch-all for host-side bugs.
    #[error("internal: {0}")]
    Internal(String),
}

/// Constructors for the free-text cases, lifting any detail that is
/// `Into<String>`. The unit cases need none.
impl Fault {
    /// [`Fault::Unsupported`].
    pub fn unsupported(detail: impl Into<String>) -> Self {
        Self::Unsupported(detail.into())
    }

    /// [`Fault::Unavailable`].
    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self::Unavailable(detail.into())
    }

    /// [`Fault::Denied`].
    pub fn denied(detail: impl Into<String>) -> Self {
        Self::Denied(detail.into())
    }

    /// [`Fault::InvalidInput`].
    pub fn invalid_input(detail: impl Into<String>) -> Self {
        Self::InvalidInput(detail.into())
    }

    /// [`Fault::Internal`].
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal(detail.into())
    }
}

/// Closed mirror of [`Fault`], the shape the bind macro lowers to the
/// wire enum. [`Fault`] is `#[non_exhaustive]`, so a match outside this
/// crate needs a wildcard arm; matching this type instead keeps the
/// lowering exhaustive, and a new [`Fault`] case fails to compile
/// rather than degrading at the wire boundary.
///
/// Hidden: macro plumbing only, `pub` so the expansion can name it.
/// Matching it elsewhere would defeat `#[non_exhaustive]` on [`Fault`],
/// as the macro's arms are regenerated with the SDK but downstream
/// matches are not.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FaultParts {
    /// [`Fault::Unsupported`].
    Unsupported(String),
    /// [`Fault::Unavailable`].
    Unavailable(String),
    /// [`Fault::Denied`].
    Denied(String),
    /// [`Fault::RateLimited`].
    RateLimited,
    /// [`Fault::Timeout`].
    Timeout,
    /// [`Fault::InvalidInput`].
    InvalidInput(String),
    /// [`Fault::Internal`].
    Internal(String),
}

impl From<Fault> for FaultParts {
    fn from(f: Fault) -> Self {
        // Exhaustive: `#[non_exhaustive]` does not bind the defining
        // crate, so a new `Fault` case is a compile error here.
        match f {
            Fault::Unsupported(s) => Self::Unsupported(s),
            Fault::Unavailable(s) => Self::Unavailable(s),
            Fault::Denied(s) => Self::Denied(s),
            Fault::RateLimited => Self::RateLimited,
            Fault::Timeout => Self::Timeout,
            Fault::InvalidInput(s) => Self::InvalidInput(s),
            Fault::Internal(s) => Self::Internal(s),
        }
    }
}

/// Sealing markers for [`Host`] and [`HostFault`]: implement alongside
/// the trait.
#[doc(hidden)]
pub mod sealed {
    pub trait SealedHost {}
    pub trait SealedHostFault {}
}

impl<T> sealed::SealedHost for T where T: ChainHost + LocalStoreHost + LoggingHost {}

impl sealed::SealedHostFault for Fault {}
impl sealed::SealedHostFault for ChainError {}

/// Recovers the shared [`Fault`] and a stable snake_case label from a
/// richer per-interface error. Sealed.
pub trait HostFault: sealed::SealedHostFault {
    /// The embedded fault, when this value represents one.
    fn fault(&self) -> Option<&Fault>;
    /// Stable snake_case label for logs and metrics.
    fn label(&self) -> &'static str;
}

impl HostFault for Fault {
    fn fault(&self) -> Option<&Fault> {
        Some(self)
    }

    fn label(&self) -> &'static str {
        self.into()
    }
}

/// A structured JSON-RPC error response, mirrored from
/// `nexum:host/chain.rpc-error`. `data` holds the host-decoded
/// `error.data` revert bytes, ready for a revert decoder.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("rpc error {code}: {message}")]
pub struct RpcError {
    /// JSON-RPC error code from the node.
    pub code: i32,
    /// Human-readable detail.
    pub message: String,
    /// Decoded `error.data` bytes, when the node returned a hex payload.
    pub data: Option<Bytes>,
}

/// Failure of a `nexum:host/chain` call, mirrored from
/// `nexum:host/chain.chain-error`: a shared host [`Fault`] or a
/// structured JSON-RPC [`RpcError`]. [`HostFault`] recovers the
/// embedded [`Fault`], present only on the `Fault` case.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ChainError {
    /// A shared host fault.
    #[error(transparent)]
    Fault(#[from] Fault),
    /// A structured JSON-RPC error response.
    #[error(transparent)]
    Rpc(#[from] RpcError),
}

impl HostFault for ChainError {
    fn fault(&self) -> Option<&Fault> {
        match self {
            ChainError::Fault(f) => Some(f),
            ChainError::Rpc(_) => None,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            ChainError::Fault(f) => f.label(),
            ChainError::Rpc(_) => "rpc",
        }
    }
}

/// Fold a [`ChainError`] into the shared [`Fault`]: the `Fault` case
/// passes through; an [`RpcError`] becomes [`Fault::Internal`] carrying
/// the code, message, and any revert bytes as a `0x` hex suffix.
impl From<ChainError> for Fault {
    fn from(err: ChainError) -> Self {
        match err {
            ChainError::Fault(fault) => fault,
            ChainError::Rpc(rpc) => {
                let mut message = format!("rpc error {}: {}", rpc.code, rpc.message);
                if let Some(data) = rpc.data {
                    message.push_str(" (");
                    message.push_str(&alloy_primitives::hex::encode_prefixed(data));
                    message.push(')');
                }
                Fault::Internal(message)
            }
        }
    }
}

/// `nexum:host/chain` - raw JSON-RPC dispatch.
pub trait ChainHost {
    /// Execute a JSON-RPC request against the given chain.
    fn request(&self, chain_id: u64, method: &str, params: &str) -> Result<String, ChainError>;
}

/// One write in a [`LocalStoreHost::apply`] batch, mirrored from
/// `nexum:host/local-store.write-op`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WriteOp {
    /// Insert or overwrite `key` with `value`.
    Set {
        /// Key to write.
        key: String,
        /// Value bytes.
        value: Vec<u8>,
    },
    /// Delete `key`; a no-op if absent.
    Delete {
        /// Key to delete.
        key: String,
    },
}

/// Host-side test on a candidate value. Data, not a predicate: the host
/// cannot run guest code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueFilter<'a> {
    /// Keep values starting with these bytes.
    HasPrefix(&'a [u8]),
    /// Keep values not starting with these bytes.
    LacksPrefix(&'a [u8]),
}

impl ValueFilter<'_> {
    /// Whether `value` survives the filter.
    #[must_use]
    pub fn keeps(&self, value: &[u8]) -> bool {
        match self {
            Self::HasPrefix(p) => value.starts_with(p),
            Self::LacksPrefix(p) => !value.starts_with(p),
        }
    }
}

/// One [`LocalStoreHost::list_entries`] request.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListQuery<'a> {
    /// Key prefix to scan.
    pub prefix: &'a str,
    /// Exclusive resume key; empty starts at the beginning.
    pub start_after: &'a str,
    /// Entries returned at most; zero takes the host's cap.
    pub limit: u32,
    /// Entries examined at most; zero takes the host's cap. Separate
    /// from `limit` because a filtered page can examine many and return
    /// none.
    pub scan_limit: u32,
    /// Host-side value test; `None` keeps every entry.
    pub filter: Option<ValueFilter<'a>>,
}

/// One page of [`LocalStoreHost::list_entries`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntryPage {
    /// Surviving entries in key order.
    pub entries: Vec<(String, Vec<u8>)>,
    /// Last key examined, not the last returned: a filtered page can be
    /// empty mid-scan and must still resume.
    pub last_examined: Option<String>,
    /// Whether the scan reached the end of the prefix range.
    pub exhausted: bool,
}

/// Page `rows`, which must be in key order, against `query`. The paging
/// an in-memory [`LocalStoreHost`] needs; the real host does this itself
/// and caps what a zero `limit` or `scan_limit` means, which is why zero
/// is unbounded here.
pub fn page_entries(
    query: &ListQuery<'_>,
    rows: impl IntoIterator<Item = (String, Vec<u8>)>,
) -> EntryPage {
    let limit = if query.limit == 0 {
        usize::MAX
    } else {
        query.limit as usize
    };
    let scan_limit = if query.scan_limit == 0 {
        usize::MAX
    } else {
        query.scan_limit as usize
    };
    let mut page = EntryPage {
        exhausted: true,
        ..EntryPage::default()
    };
    let mut examined = 0usize;
    for (key, value) in rows {
        if !key.starts_with(query.prefix) || key.as_str() <= query.start_after {
            continue;
        }
        if examined == scan_limit {
            page.exhausted = false;
            break;
        }
        if query.filter.is_none_or(|f| f.keeps(&value)) {
            if page.entries.len() == limit {
                page.exhausted = false;
                break;
            }
            page.entries.push((key.clone(), value));
        }
        examined += 1;
        page.last_examined = Some(key);
    }
    page
}

/// `nexum:host/local-store` - per-module key-value persistence.
pub trait LocalStoreHost {
    /// Fetch a value. `Ok(None)` when the key is absent.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Fault>;
    /// Insert or overwrite.
    fn set(&self, key: &str, value: &[u8]) -> Result<(), Fault>;
    /// Delete. No-op if the key is absent.
    fn delete(&self, key: &str) -> Result<(), Fault>;
    /// Enumerate keys whose raw form starts with `prefix`.
    fn list_keys(&self, prefix: &str) -> Result<Vec<String>, Fault>;
    /// One page of entries under `query`, in key order. One boundary
    /// crossing on the real host adapter, which overrides this with the
    /// host's `list-entries` verb; the default is a per-key fallback for
    /// arbitrary impls such as mocks. The host keeps no cursor, so a
    /// caller resumes from [`EntryPage::last_examined`]: a filtered page
    /// can be empty while the scan is unfinished, and only
    /// [`EntryPage::exhausted`] ends it.
    fn list_entries(&self, query: &ListQuery<'_>) -> Result<EntryPage, Fault> {
        let mut keys = self.list_keys(query.prefix)?;
        keys.sort();
        let mut rows = Vec::new();
        for key in keys {
            if let Some(value) = self.get(&key)? {
                rows.push((key, value));
            }
        }
        Ok(page_entries(query, rows))
    }
    /// Apply a batch of writes; later ops on a key supersede earlier
    /// ones. Atomic (every op lands or none does) only on the real
    /// host adapter, which overrides this with the host's `apply`
    /// verb; the default is a per-op `set`/`delete` fallback for
    /// arbitrary impls such as mocks, so a mid-batch failure leaves
    /// the earlier ops applied.
    fn apply(&self, ops: &[WriteOp]) -> Result<(), Fault> {
        for op in ops {
            match op {
                WriteOp::Set { key, value } => self.set(key, value)?,
                WriteOp::Delete { key } => self.delete(key)?,
            }
        }
        Ok(())
    }
    /// Whether `key` exists.
    fn contains(&self, key: &str) -> Result<bool, Fault> {
        Ok(self.get(key)?.is_some())
    }
    /// Value byte length, `Ok(None)` when absent.
    fn len(&self, key: &str) -> Result<Option<u64>, Fault> {
        Ok(self.get(key)?.map(|v| v.len() as u64))
    }
    /// Number of keys starting with `prefix`.
    fn count(&self, prefix: &str) -> Result<u64, Fault> {
        Ok(self.list_keys(prefix)?.len() as u64)
    }
}

/// `nexum:host/logging` - structured runtime logs.
pub trait LoggingHost {
    /// Emit a log line at the given [`Level`].
    fn log(&self, level: Level, message: &str);
}

/// Supertrait bundling the core host interfaces. Module functions
/// take `<H: Host>` (or bound exactly the interfaces they exercise) and
/// run against `nexum_sdk_test::MockHost` in tests. Blanket-implemented
/// for any type carrying them all; sealed, so that impl is the only one.
pub trait Host: sealed::SealedHost + ChainHost + LocalStoreHost + LoggingHost {}
impl<T> Host for T where T: ChainHost + LocalStoreHost + LoggingHost {}

#[cfg(test)]
mod tests {
    use super::{ChainError, Fault, HostFault, RpcError};

    #[test]
    fn local_store_metadata_defaults_derive_from_required_methods() {
        use super::LocalStoreHost;

        /// Two fixed rows; only the four required methods are written.
        struct TwoRows;
        impl LocalStoreHost for TwoRows {
            fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Fault> {
                Ok(match key {
                    "a" => Some(b"abc".to_vec()),
                    "b" => Some(Vec::new()),
                    _ => None,
                })
            }
            fn set(&self, _: &str, _: &[u8]) -> Result<(), Fault> {
                Ok(())
            }
            fn delete(&self, _: &str) -> Result<(), Fault> {
                Ok(())
            }
            fn list_keys(&self, prefix: &str) -> Result<Vec<String>, Fault> {
                Ok(["a", "b"]
                    .iter()
                    .filter(|k| k.starts_with(prefix))
                    .map(|k| (*k).to_owned())
                    .collect())
            }
        }

        assert!(TwoRows.contains("a").unwrap());
        assert!(!TwoRows.contains("missing").unwrap());
        assert_eq!(TwoRows.len("a").unwrap(), Some(3));
        assert_eq!(TwoRows.len("b").unwrap(), Some(0));
        assert_eq!(TwoRows.len("missing").unwrap(), None);
        assert_eq!(TwoRows.count("").unwrap(), 2);
        assert_eq!(TwoRows.count("a").unwrap(), 1);
        assert_eq!(TwoRows.count("z").unwrap(), 0);

        use super::{ListQuery, ValueFilter};
        let page = TwoRows
            .list_entries(&ListQuery {
                filter: Some(ValueFilter::HasPrefix(b"ab")),
                ..ListQuery::default()
            })
            .unwrap();
        assert_eq!(page.entries, vec![("a".to_owned(), b"abc".to_vec())]);
        // "b" was examined and filtered out, so the resume key is past it.
        assert_eq!(page.last_examined.as_deref(), Some("b"));
        assert!(page.exhausted);

        let capped = TwoRows
            .list_entries(&ListQuery {
                limit: 1,
                ..ListQuery::default()
            })
            .unwrap();
        assert_eq!(capped.entries.len(), 1);
        assert_eq!(capped.last_examined.as_deref(), Some("a"));
        assert!(!capped.exhausted);
    }

    #[test]
    fn local_store_default_apply_falls_back_to_one_call_per_op() {
        use std::cell::RefCell;

        use super::{LocalStoreHost, WriteOp};

        /// Records each call; only the four required methods are written.
        #[derive(Default)]
        struct Recorder(RefCell<Vec<String>>);
        impl LocalStoreHost for Recorder {
            fn get(&self, _: &str) -> Result<Option<Vec<u8>>, Fault> {
                Ok(None)
            }
            fn set(&self, key: &str, _: &[u8]) -> Result<(), Fault> {
                self.0.borrow_mut().push(format!("set {key}"));
                Ok(())
            }
            fn delete(&self, key: &str) -> Result<(), Fault> {
                self.0.borrow_mut().push(format!("delete {key}"));
                Ok(())
            }
            fn list_keys(&self, _: &str) -> Result<Vec<String>, Fault> {
                Ok(Vec::new())
            }
        }

        let recorder = Recorder::default();
        recorder
            .apply(&[
                WriteOp::Set {
                    key: "a".into(),
                    value: b"1".to_vec(),
                },
                WriteOp::Delete { key: "b".into() },
                WriteOp::Set {
                    key: "c".into(),
                    value: b"2".to_vec(),
                },
            ])
            .unwrap();
        assert_eq!(
            recorder.0.into_inner(),
            ["set a", "delete b", "set c"].map(str::to_owned)
        );
    }

    #[test]
    fn fault_labels_match_the_single_source_vocabulary() {
        use nexum_world::FaultLabel as Label;
        let cases: [(Fault, &str); 7] = [
            (Fault::Unsupported(String::new()), Label::Unsupported.into()),
            (Fault::Unavailable(String::new()), Label::Unavailable.into()),
            (Fault::Denied(String::new()), Label::Denied.into()),
            (Fault::RateLimited, Label::RateLimited.into()),
            (Fault::Timeout, Label::Timeout.into()),
            (
                Fault::InvalidInput(String::new()),
                Label::InvalidInput.into(),
            ),
            (Fault::Internal(String::new()), Label::Internal.into()),
        ];
        for (fault, label) in cases {
            assert_eq!(fault.label(), label);
            assert_eq!(fault.fault(), Some(&fault));
        }
    }

    #[test]
    fn host_fault_is_object_safe() {
        let boxed: Box<dyn HostFault> = Box::new(Fault::Timeout);
        assert_eq!(boxed.label(), "timeout");
    }

    #[test]
    fn chain_error_recovers_embedded_fault() {
        let fault = ChainError::Fault(Fault::Timeout);
        assert_eq!(fault.fault(), Some(&Fault::Timeout));
        assert_eq!(fault.label(), "timeout");

        let rpc = ChainError::Rpc(RpcError {
            code: -32000,
            message: "execution reverted".into(),
            data: Some(vec![0xde, 0xad].into()),
        });
        assert_eq!(rpc.fault(), None);
        assert_eq!(rpc.label(), "rpc");
    }

    #[test]
    fn chain_error_rpc_folds_to_internal_fault_with_hex_data() {
        let fault = Fault::from(ChainError::Rpc(RpcError {
            code: -32000,
            message: "execution reverted".into(),
            data: Some(vec![0x08, 0xc3, 0x79, 0xa0].into()),
        }));
        let Fault::Internal(message) = fault else {
            panic!("rpc folds to internal, got {fault:?}");
        };
        assert!(message.contains("-32000"));
        assert!(message.contains("0x08c379a0"));
    }

    #[test]
    fn chain_error_fault_folds_through_unchanged() {
        let fault = Fault::from(ChainError::Fault(Fault::Unavailable("rpc down".into())));
        assert_eq!(fault, Fault::Unavailable("rpc down".into()));
    }
}
