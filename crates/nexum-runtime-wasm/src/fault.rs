//! Host-side projections of a [`Fault`] into log and metric fields.

use nexum_runtime_api::bindings::nexum::host::types::Fault;

/// Metric and log `kind`.
pub fn fault_label(fault: &Fault) -> &'static str {
    use nexum_world::FaultLabel as Label;
    match fault {
        Fault::Unsupported(_) => Label::Unsupported,
        Fault::Unavailable(_) => Label::Unavailable,
        Fault::Denied(_) => Label::Denied,
        Fault::RateLimited(_) => Label::RateLimited,
        Fault::Timeout => Label::Timeout,
        Fault::InvalidInput(_) => Label::InvalidInput,
        Fault::Internal(_) => Label::Internal,
    }
    .into()
}

/// Log `message`.
pub fn fault_message(fault: &Fault) -> std::borrow::Cow<'_, str> {
    match fault {
        Fault::Unsupported(m)
        | Fault::Unavailable(m)
        | Fault::Denied(m)
        | Fault::InvalidInput(m)
        | Fault::Internal(m) => std::borrow::Cow::Borrowed(m),
        Fault::RateLimited(rl) => match rl.retry_after_ms {
            Some(ms) => std::borrow::Cow::Owned(format!("rate limited, retry after {ms} ms")),
            None => std::borrow::Cow::Borrowed("rate limited"),
        },
        Fault::Timeout => std::borrow::Cow::Borrowed("timeout"),
    }
}
