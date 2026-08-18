//! The outbound wasi:http gate for the Nexum runtime.

#![forbid(unsafe_code)]

mod http;

pub use self::http::HttpGate;
