//! Small constructors that wrap the WIT `HostError` shape, used by
//! every `Host` trait impl, plus the lowercase hex encoder shared by
//! the `cow-api` submission path.

use crate::bindings::HostError;
use crate::bindings::nexum::host::types::HostErrorKind;

/// `Unsupported` (HTTP 501-style) error for capabilities the engine
/// reference build does not implement yet.
pub(crate) fn unimplemented(domain: &str, detail: impl Into<String>) -> HostError {
    HostError {
        domain: domain.into(),
        kind: HostErrorKind::Unsupported,
        code: 501,
        message: detail.into(),
        data: None,
    }
}

/// `Internal` (HTTP 500-style) error for unexpected backend failures.
pub(crate) fn internal_error(domain: &str, detail: impl Into<String>) -> HostError {
    HostError {
        domain: domain.into(),
        kind: HostErrorKind::Internal,
        code: 0,
        message: detail.into(),
        data: None,
    }
}

/// Lowercase hex encoder. Kept in the engine binary rather than
/// pulling a `hex` crate just for one call site. Writes into the
/// pre-allocated buffer to avoid the per-byte `String` allocation
/// `format!("{b:02x}")` would do.
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").expect("writing to String never fails");
    }
    s
}
