//! wasi:http outgoing gate. [`HttpGate::send_request`] enforces the
//! per-module `[dependencies.http].hosts` list, clamps guest timeouts to the
//! `[limits.http]` maxima, and bounds the exchange with a total deadline and
//! response-body cap. Redirects are not followed; each hop re-enters the gate.
//! Before connecting, the target is refused if it is, or resolves onto, an
//! address this host will not reach. Only `[limits.http].permit_destinations`
//! admits one. That narrows DNS rebinding; it does not pin the connected
//! address.
//!
//! Unlike the `nexum:host` seams, http carries no synthesized world import;
//! the interface is linked for every component. This gate and the load time
//! capability check are the whole restriction.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use ipnet::IpNet;
use strum::IntoStaticStr;
use tokio::net::lookup_host;
use tracing::warn;
use wasmtime_wasi_http::p2::bindings::http::types::ErrorCode;
use wasmtime_wasi_http::p2::body::{HyperIncomingBody, HyperOutgoingBody};
use wasmtime_wasi_http::p2::types::{HostFutureIncomingResponse, OutgoingRequestConfig};
use wasmtime_wasi_http::p2::{HttpResult, WasiHttpHooks, default_send_request_handler};

use nexum_primitives::host_pattern::{HostPattern, host_allowed};
use nexum_runtime_config::OutboundHttpLimits;

/// Per-module outbound HTTP policy.
pub struct HttpGate {
    module: String,
    /// The author's `[dependencies.http].hosts`.
    allowlist: Vec<HostPattern>,
    /// `[policy.component.<id>].http_allow`; when present a host must
    /// match this list too, so the manifest cannot widen past it.
    operator_allow: Option<Vec<HostPattern>>,
    limits: OutboundHttpLimits,
    /// Operator-permitted addresses that would otherwise be refused.
    permitted: Vec<IpAddr>,
    /// `[policy].http_deny` ranges, refused after every allowlist.
    denied: Vec<IpNet>,
}

impl HttpGate {
    /// Gate for `module`.
    pub fn new(
        module: impl Into<String>,
        allowlist: Vec<HostPattern>,
        operator_allow: Option<Vec<HostPattern>>,
        limits: OutboundHttpLimits,
        permitted: Vec<IpAddr>,
        denied: Vec<IpNet>,
    ) -> Self {
        Self {
            module: module.into(),
            allowlist,
            operator_allow,
            limits,
            permitted,
            denied,
        }
    }
}

/// Which rule refused an outbound request, as the denial counter's `reason`.
#[derive(Clone, Copy, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum Refusal {
    Allowlist,
    Destination,
}

/// Count one refusal under the shared capability-denial series.
///
/// Every label is operator-written or fixed: a host or a URL is guest-chosen,
/// so labelling with one would let a module mint series at will.
fn count_refusal(module: &str, refusal: Refusal) {
    metrics::counter!(
        "nexum_runtime_capability_denials_total",
        "capability" => "http",
        "reason" => <&'static str>::from(refusal),
        "module" => module.to_owned(),
    )
    .increment(1);
}

impl WasiHttpHooks for HttpGate {
    fn send_request(
        &mut self,
        request: http::Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
    ) -> HttpResult<HostFutureIncomingResponse> {
        if let Err(code) = admit(
            request.uri(),
            &self.allowlist,
            self.operator_allow.as_deref(),
        ) {
            count_refusal(&self.module, Refusal::Allowlist);
            // Log the host only: paths and query strings are
            // guest-supplied and may carry credentials.
            warn!(
                module = %self.module,
                host = request.uri().host().unwrap_or("<none>"),
                "[http] outbound request denied by allowlist",
            );
            return Err(code.into());
        }
        Ok(send_with_limits(
            request,
            clamp(config, &self.limits),
            self.limits,
            self.module.clone(),
            self.permitted.clone(),
            self.denied.clone(),
        ))
    }
}

/// Clamp guest timeouts to the engine maxima, lowering never rejecting; each
/// maximum doubles as the effective default for an unset timeout.
fn clamp(mut config: OutgoingRequestConfig, limits: &OutboundHttpLimits) -> OutgoingRequestConfig {
    config.connect_timeout = config.connect_timeout.min(limits.connect_timeout_max);
    config.first_byte_timeout = config.first_byte_timeout.min(limits.first_byte_timeout_max);
    config.between_bytes_timeout = config
        .between_bytes_timeout
        .min(limits.between_bytes_timeout_max);
    config
}

/// Dispatch through the default backend under the total deadline and body
/// cap. The deadline is unconditional: it covers headers and, via
/// [`CappedBody`], the body, and the raced connection driver is aborted when
/// it fires even if the guest never reads the response.
fn send_with_limits(
    request: http::Request<HyperOutgoingBody>,
    config: OutgoingRequestConfig,
    limits: OutboundHttpLimits,
    module: String,
    permitted: Vec<IpAddr>,
    denied: Vec<IpNet>,
) -> HostFutureIncomingResponse {
    let handle = wasmtime_wasi::runtime::spawn(async move {
        let deadline = tokio::time::Instant::now() + limits.total_deadline;
        let uri = request.uri().clone();
        let sent = tokio::time::timeout_at(deadline, async move {
            if let Err(code) = reject_prohibited_destination(&uri, &permitted, &denied).await {
                count_refusal(&module, Refusal::Destination);
                // The host only, for the reason `send_request` gives.
                warn!(
                    module = %module,
                    host = uri.host().unwrap_or("<none>"),
                    "[http] outbound request denied by destination rules",
                );
                return Err(code);
            }
            default_send_request_handler(request, config).await
        })
        .await;
        let result = match sent {
            Ok(Ok(mut incoming)) => {
                // Dropping the inner worker handle aborts the hyper
                // connection driver, closing the socket at the
                // deadline regardless of guest polling. A guest drop
                // of the response still cascades: it drops this
                // wrapper handle, which aborts the race, which drops
                // the worker.
                incoming.worker = incoming.worker.map(|worker| {
                    wasmtime_wasi::runtime::spawn(async move {
                        let _ = tokio::time::timeout_at(deadline, worker).await;
                    })
                });
                incoming.resp = incoming.resp.map(|body| {
                    CappedBody::new(body, limits.response_body_max_bytes, deadline).boxed_unsync()
                });
                Ok(incoming)
            }
            Ok(Err(code)) => Err(code),
            Err(_) => Err(ErrorCode::ConnectionTimeout),
        };
        Ok(result)
    });
    HostFutureIncomingResponse::pending(handle)
}

/// Response-body wrapper enforcing the size cap and total deadline. Over-cap
/// yields `HttpResponseBodySize(cap)`; the deadline firing yields
/// `ConnectionReadTimeout`.
struct CappedBody {
    inner: HyperIncomingBody,
    /// Bytes still admissible under the cap.
    remaining: u64,
    /// Configured cap, echoed in the error payload.
    cap: u64,
    /// Sleep armed at the request's total deadline.
    deadline: Pin<Box<tokio::time::Sleep>>,
}

impl CappedBody {
    fn new(inner: HyperIncomingBody, cap: u64, deadline: tokio::time::Instant) -> Self {
        Self {
            inner,
            remaining: cap,
            cap,
            deadline: Box::pin(tokio::time::sleep_until(deadline)),
        }
    }
}

impl Body for CappedBody {
    type Data = Bytes;
    type Error = ErrorCode;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, ErrorCode>>> {
        let me = Pin::into_inner(self);
        if let Poll::Ready(()) = me.deadline.as_mut().poll(cx) {
            return Poll::Ready(Some(Err(ErrorCode::ConnectionReadTimeout)));
        }
        match Pin::new(&mut me.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let len = data.len() as u64;
                    if len > me.remaining {
                        return Poll::Ready(Some(Err(ErrorCode::HttpResponseBodySize(Some(
                            me.cap,
                        )))));
                    }
                    me.remaining -= len;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Allowlist decision for one request URI. Host-only, case-insensitive, exact
/// or `*.suffix` per [`host_allowed`]; IPv6 literals stay bracketed.
/// Name-based and pre-resolution, so it pins no address on its own.
/// `reject_prohibited_destination` applies the address rules after it.
///
/// The host must match the author list and, when the operator wrote one,
/// the `http_allow` list too: the effective set is the intersection, so
/// neither file can widen past the other.
fn admit(
    uri: &http::Uri,
    allowlist: &[HostPattern],
    operator_allow: Option<&[HostPattern]>,
) -> Result<(), ErrorCode> {
    let Some(host) = uri.host() else {
        return Err(ErrorCode::HttpRequestUriInvalid);
    };
    let admitted = host_allowed(host, allowlist)
        && operator_allow.is_none_or(|allow| host_allowed(host, allow));
    if admitted {
        Ok(())
    } else {
        Err(ErrorCode::HttpRequestDenied)
    }
}

/// Refuse a named target that resolves onto an address this host will not
/// reach. A resolution failure is not a denial: the connection resolves again
/// and reports its own error.
///
/// Two gaps remain. The connection performs its own lookup, so an answer can
/// change between the two. Closing that needs a pre-resolved connect address
/// with the original hostname kept for TLS, and
/// `default_send_request_handler` has no seam for it.
///
/// A literal is checked the same way. The allowlist naming it is
/// author-supplied (ADR-0001), so a module cannot reach a refused address by
/// writing it out. Only `[limits.http].permit_destinations`, which the
/// operator writes, admits one. A `[policy].http_deny` range is refused
/// unconditionally: the subtraction runs after every allow.
async fn reject_prohibited_destination(
    uri: &http::Uri,
    permitted: &[IpAddr],
    denied: &[IpNet],
) -> Result<(), ErrorCode> {
    let refused =
        |ip: &IpAddr| in_denied(denied, *ip) || (is_prohibited(*ip) && !permitted.contains(ip));
    let Some(host) = uri.host() else {
        return Ok(()); // `admit` already rejects a hostless URI before this runs.
    };
    if let Some(ip) = parse_ip_literal(host) {
        return if refused(&ip) {
            Err(ErrorCode::DestinationIpProhibited)
        } else {
            Ok(())
        };
    }
    let Ok(addrs) = lookup_host((host, 0)).await else {
        return Ok(());
    };
    if addrs.map(|addr| addr.ip()).any(|ip| refused(&ip)) {
        return Err(ErrorCode::DestinationIpProhibited);
    }
    Ok(())
}

/// `IpNet::contains` never crosses address families, so an IPv4-mapped
/// spelling is checked in both forms; without this, `::ffff:a.b.c.d`
/// renames its way past an IPv4 deny range, and the reverse.
fn in_denied(denied: &[IpNet], ip: IpAddr) -> bool {
    let alternate = match ip {
        IpAddr::V4(v4) => Some(IpAddr::V6(v4.to_ipv6_mapped())),
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4),
    };
    denied
        .iter()
        .any(|net| net.contains(&ip) || alternate.as_ref().is_some_and(|alt| net.contains(alt)))
}

/// `http::Uri::host()` keeps an IPv6 literal's brackets (see
/// `ipv6_literal_uses_bracketed_form`); strip them before parsing. Brackets
/// are only ever valid URI-authority syntax around an IPv6 literal, so
/// stripping them here never mistakes a name for one.
fn parse_ip_literal(host: &str) -> Option<IpAddr> {
    match host.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        Some(inner) => inner.parse::<Ipv6Addr>().ok().map(IpAddr::V6),
        None => host.parse::<IpAddr>().ok(),
    }
}

/// True for an address this host refuses by default: loopback, private
/// (RFC 1918), link-local (RFC 3927 - this covers the 169.254.169.254 cloud
/// metadata endpoint too, with no special case needed), carrier-grade NAT
/// space, unique-local, multicast, and unspecified/broadcast. An IPv4-mapped
/// IPv6 address is unwrapped and checked against the same IPv4 rules, so
/// `::ffff:127.0.0.1` cannot rename its way past the IPv6 branch.
fn is_prohibited(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_prohibited_v4(v4),
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(is_prohibited_v4)
            .unwrap_or_else(|| is_prohibited_v6(v6)),
    }
}

fn is_prohibited_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || is_shared_nat_v4(ip)
}

/// 100.64.0.0/10 (RFC 6598), carrier-grade NAT space; not covered by
/// `Ipv4Addr::is_private`, which is RFC 1918 only.
fn is_shared_nat_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (b & 0b1100_0000) == 0b0100_0000
}

fn is_prohibited_v6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || is_unique_local_v6(ip)
        || is_unicast_link_local_v6(ip)
}

/// fc00::/7 (RFC 4193).
fn is_unique_local_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

/// fe80::/10 (RFC 4291).
fn is_unicast_link_local_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use http_body_util::{Empty, Full};
    use nexum_runtime_testing::{Sample, block_on_current_thread, capture_metrics, samples_named};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wasmtime_wasi_http::p2::types::IncomingResponse;

    use super::*;

    const DENIALS: &str = "nexum_runtime_capability_denials_total";

    fn uri(s: &str) -> http::Uri {
        s.parse().expect("test URI parses")
    }

    fn allow(entries: &[&str]) -> Vec<HostPattern> {
        entries.iter().copied().map(HostPattern::from).collect()
    }

    /// The author-list-only form; the intersection cases call the real one.
    fn admit(uri: &http::Uri, allowlist: &[HostPattern]) -> Result<(), ErrorCode> {
        super::admit(uri, allowlist, None)
    }

    /// The no-deny form; the `http_deny` cases call the real one.
    async fn reject_prohibited_destination(
        uri: &http::Uri,
        permitted: &[IpAddr],
    ) -> Result<(), ErrorCode> {
        super::reject_prohibited_destination(uri, permitted, &[]).await
    }

    /// Generous limits so a test trips only the one it tightens.
    fn limits() -> OutboundHttpLimits {
        OutboundHttpLimits {
            connect_timeout_max: Duration::from_secs(10),
            first_byte_timeout_max: Duration::from_secs(10),
            between_bytes_timeout_max: Duration::from_secs(10),
            total_deadline: Duration::from_secs(10),
            response_body_max_bytes: 1 << 20,
        }
    }

    fn denied(u: &str, entries: &[&str]) -> bool {
        matches!(
            admit(&uri(u), &allow(entries)),
            Err(ErrorCode::HttpRequestDenied)
        )
    }

    #[test]
    fn exact_host_passes() {
        assert!(
            admit(
                &uri("https://api.acme.example/v1/x"),
                &allow(&["api.acme.example"])
            )
            .is_ok()
        );
        assert!(
            admit(
                &uri("http://api.acme.example/"),
                &allow(&["api.acme.example"])
            )
            .is_ok()
        );
    }

    #[test]
    fn off_list_host_is_denied() {
        assert!(denied("https://evil.example/", &["api.acme.example"]));
        assert!(denied(
            "https://api.acme.example.evil.example/",
            &["api.acme.example"]
        ));
    }

    #[test]
    fn empty_allowlist_denies_everything() {
        assert!(denied("https://api.acme.example/", &[]));
        assert!(denied("http://127.0.0.1/", &[]));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(
            admit(
                &uri("https://API.ACME.EXAMPLE/"),
                &allow(&["api.acme.example"])
            )
            .is_ok()
        );
        assert!(
            admit(
                &uri("https://api.acme.example/"),
                &allow(&["API.ACME.EXAMPLE"])
            )
            .is_ok()
        );
    }

    #[test]
    fn wildcard_matches_subdomains_but_not_the_suffix_itself() {
        let list = allow(&["*.discord.com"]);
        assert!(admit(&uri("https://gateway.discord.com/"), &list).is_ok());
        assert!(admit(&uri("https://a.b.discord.com/"), &list).is_ok());
        assert!(denied("https://discord.com/", &["*.discord.com"]));
        assert!(denied("https://notdiscord.com/", &["*.discord.com"]));
    }

    #[test]
    fn exact_entry_does_not_match_subdomains() {
        assert!(denied(
            "https://sub.api.acme.example/",
            &["api.acme.example"]
        ));
    }

    #[test]
    fn ipv4_literal_matches_only_when_listed() {
        assert!(admit(&uri("http://127.0.0.1/x"), &allow(&["127.0.0.1"])).is_ok());
        assert!(denied("http://127.0.0.2/x", &["127.0.0.1"]));
        // A listed name never admits an IP literal for that name.
        assert!(denied("http://93.184.216.34/", &["example.com"]));
    }

    #[test]
    fn ipv6_literal_uses_bracketed_form() {
        assert!(admit(&uri("http://[::1]:8080/x"), &allow(&["[::1]"])).is_ok());
        assert!(denied("http://[::1]/x", &["::1"]));
        assert!(denied("http://[2001:db8::1]/", &["[::1]"]));
    }

    #[test]
    fn ports_do_not_affect_matching() {
        let list = allow(&["api.acme.example"]);
        assert!(admit(&uri("https://api.acme.example:8443/v1"), &list).is_ok());
        assert!(admit(&uri("http://api.acme.example:80/v1"), &list).is_ok());
        assert!(denied("https://evil.example:443/", &["api.acme.example"]));
        // A port spelled in the allowlist entry never matches: entries
        // are hosts, not authorities.
        assert!(denied(
            "https://api.acme.example:8443/",
            &["api.acme.example:8443"]
        ));
    }

    //
    // `http::Uri` resolves the authority per RFC 3986 before `admit`
    // ever sees a host string, so these are regression guards on the
    // parser's behaviour, not on `admit` itself. Each case names the
    // trick and asserts the real target host - never the attacker's
    // decoy - is what `host_allowed` sees.

    #[test]
    fn userinfo_prefix_does_not_leak_a_different_host_into_the_allowlist() {
        // `http://allowed.com@evil.com/` - "allowed.com" is userinfo,
        // "evil.com" is the host. A parser that mistook the text before
        // `@` for the host would wrongly admit this against an
        // `allowed.com` allowlist entry.
        assert!(denied("http://allowed.com@evil.com/", &["allowed.com"]));
        assert_eq!(uri("http://allowed.com@evil.com/").host(), Some("evil.com"));
    }

    #[test]
    fn userinfo_matching_an_allowlist_entry_grants_nothing() {
        // `http://evil.com@allowed.com/` - the real host is
        // "allowed.com" and is correctly admitted; "evil.com" sitting in
        // userinfo must never itself satisfy an allowlist entry.
        assert!(
            admit(
                &uri("http://evil.com@allowed.com/"),
                &allow(&["allowed.com"])
            )
            .is_ok()
        );
        assert!(denied("http://evil.com@allowed.com/", &["evil.com"]));
    }

    #[test]
    fn backslash_in_the_authority_fails_to_parse_rather_than_bypassing() {
        // Backslash-as-slash confusion is a known SSRF trick against
        // parsers that normalize `\` to `/`. `http::Uri` does neither:
        // a backslash anywhere in the authority is rejected at parse
        // time. Checked against both entry points a backslash-bearing
        // authority could reach this gate through: the full-URI parser
        // (what this module's `uri()` test helper uses) and
        // `http::uri::Authority`, the type `wasmtime-wasi-http` builds
        // directly from the guest's `authority` string
        // (`Uri::builder().authority(...)`) - the seam a wasm guest
        // actually exercises. Both reject identically, so a request
        // built from one of these strings never reaches `admit`.
        for bad in [
            "evil.com\\allowed.com",
            "evil.com\\@allowed.com",
            "allowed.com\\.evil.com",
        ] {
            assert!(
                http::uri::Authority::try_from(bad).is_err(),
                "expected Authority::try_from to reject {bad:?}"
            );
            assert!(
                format!("http://{bad}/").parse::<http::Uri>().is_err(),
                "expected a full-URI parse error for {bad:?}"
            );
        }
    }

    #[test]
    fn numeric_ip_encodings_never_normalise_to_the_dotted_form_an_allowlist_names() {
        // `host_allowed` is an exact/wildcard string match with no IP
        // normalization (see `admit`'s doc comment). Decimal, octal, and
        // hex encodings of 127.0.0.1 are valid `http::Uri` hosts but are
        // different strings from "127.0.0.1", so none of them satisfy an
        // allowlist entry naming the dotted-quad form - locking in that
        // a future refactor doesn't "helpfully" start normalizing these
        // and turn a same-string match into an equivalent-address match.
        for evil in [
            "2130706433",
            "0177.0.0.1",
            "0x7f.0.0.1",
            "[::ffff:127.0.0.1]",
        ] {
            assert!(
                denied(&format!("http://{evil}/"), &["127.0.0.1"]),
                "{evil:?} must not satisfy a 127.0.0.1 allowlist entry"
            );
        }
    }

    #[test]
    fn fragment_and_query_after_the_host_do_not_influence_the_host_check() {
        // Historical bug: a naive host-extractor could
        // be fooled by a `/`-bearing query string or fragment appended
        // after the real host. `http::Uri::host` is unaffected by
        // either - the decoy text never becomes part of the host.
        assert!(
            admit(
                &uri("http://allowed.com#@evil.com/"),
                &allow(&["allowed.com"])
            )
            .is_ok()
        );
        assert!(
            admit(
                &uri("http://allowed.com?@evil.com/"),
                &allow(&["allowed.com"])
            )
            .is_ok()
        );
        assert_eq!(
            uri("http://allowed.com#@evil.com/").host(),
            Some("allowed.com")
        );
        assert_eq!(
            uri("http://allowed.com?@evil.com/").host(),
            Some("allowed.com")
        );
    }

    #[test]
    fn both_schemes_are_gated_identically() {
        for scheme in ["http", "https"] {
            assert!(
                admit(
                    &uri(&format!("{scheme}://api.acme.example/")),
                    &allow(&["api.acme.example"])
                )
                .is_ok()
            );
            assert!(denied(
                &format!("{scheme}://evil.example/"),
                &["api.acme.example"]
            ));
        }
    }

    #[test]
    fn uri_without_authority_is_invalid_not_denied() {
        assert!(matches!(
            admit(&uri("/relative/path"), &allow(&["api.acme.example"])),
            Err(ErrorCode::HttpRequestUriInvalid)
        ));
    }

    //
    // `reject_prohibited_destination` and its address-range helpers.

    #[test]
    fn parse_ip_literal_strips_ipv6_brackets_but_not_a_name() {
        assert_eq!(
            parse_ip_literal("127.0.0.1"),
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
        assert_eq!(
            parse_ip_literal("[::1]"),
            Some(IpAddr::V6(Ipv6Addr::LOCALHOST))
        );
        assert_eq!(parse_ip_literal("api.acme.example"), None);
        // A bracketed non-address is not silently accepted as something else.
        assert_eq!(parse_ip_literal("[not-an-address]"), None);
    }

    #[test]
    fn prohibited_v4_covers_loopback_private_link_local_and_shared_nat() {
        for ip in [
            "127.0.0.1",       // loopback
            "10.0.0.1",        // RFC 1918
            "172.16.0.1",      // RFC 1918
            "192.168.1.1",     // RFC 1918
            "169.254.1.1",     // RFC 3927 link-local
            "169.254.169.254", // cloud metadata endpoint, inside link-local
            "0.0.0.0",         // unspecified
            "255.255.255.255", // broadcast
            "224.0.0.1",       // multicast
            "100.64.0.1",      // RFC 6598 shared/CGNAT, lower bound
            "100.127.255.255", // RFC 6598 shared/CGNAT, upper bound
        ] {
            let addr: Ipv4Addr = ip.parse().expect("valid IPv4 literal");
            assert!(is_prohibited_v4(addr), "{ip} must be prohibited");
        }
    }

    #[test]
    fn shared_nat_v4_boundary_does_not_over_match() {
        // Just outside 100.64.0.0/10 on either side: ordinary public space.
        assert!(!is_shared_nat_v4("100.63.255.255".parse().unwrap()));
        assert!(!is_shared_nat_v4("100.128.0.0".parse().unwrap()));
    }

    #[test]
    fn public_v4_addresses_are_not_prohibited() {
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34"] {
            let addr: Ipv4Addr = ip.parse().expect("valid IPv4 literal");
            assert!(!is_prohibited_v4(addr), "{ip} must not be prohibited");
        }
    }

    #[test]
    fn prohibited_v6_covers_loopback_unique_local_link_local_and_multicast() {
        for ip in [
            "::1",          // loopback
            "::",           // unspecified
            "fc00::1",      // RFC 4193 unique-local, lower bound
            "fdff:ffff::1", // RFC 4193 unique-local, still inside fc00::/7
            "fe80::1",      // RFC 4291 link-local
            "febf:ffff::1", // RFC 4291 link-local, upper bound of fe80::/10
            "ff02::1",      // multicast
        ] {
            let addr: Ipv6Addr = ip.parse().expect("valid IPv6 literal");
            assert!(is_prohibited_v6(addr), "{ip} must be prohibited");
        }
    }

    #[test]
    fn unique_local_and_link_local_v6_boundaries_do_not_over_match() {
        // fe00::/7 is unique-local; fc00::/8 and fd00::/8 both fall inside it.
        // fc00::/7 starts one bit below fe80::/10 - a mask bug in either
        // would leak into the other's range.
        assert!(!is_unique_local_v6("fe00::1".parse().unwrap()));
        assert!(!is_unicast_link_local_v6("fec0::1".parse().unwrap()));
    }

    #[test]
    fn ipv4_mapped_ipv6_is_checked_against_the_same_v4_rules() {
        assert!(is_prohibited(IpAddr::V6(
            "::ffff:127.0.0.1".parse().unwrap()
        )));
        assert!(is_prohibited(IpAddr::V6(
            "::ffff:169.254.169.254".parse().unwrap()
        )));
        assert!(!is_prohibited(IpAddr::V6(
            "::ffff:8.8.8.8".parse().unwrap()
        )));
    }

    #[test]
    fn public_v6_addresses_are_not_prohibited() {
        assert!(!is_prohibited(IpAddr::V6(
            "2001:4860:4860::8888".parse().unwrap()
        )));
    }

    #[tokio::test]
    async fn reject_prohibited_destination_refuses_a_literal_the_operator_did_not_permit() {
        // The allowlist that named this literal is author-supplied, so
        // naming it is a request and not a grant.
        assert!(matches!(
            reject_prohibited_destination(&uri("http://169.254.169.254:1/x"), &[]).await,
            Err(ErrorCode::DestinationIpProhibited)
        ));
        assert!(matches!(
            reject_prohibited_destination(&uri("http://127.0.0.1:1/x"), &[]).await,
            Err(ErrorCode::DestinationIpProhibited)
        ));
    }

    #[tokio::test]
    async fn reject_prohibited_destination_admits_an_operator_permitted_literal() {
        let permitted = [IpAddr::V4(Ipv4Addr::LOCALHOST)];
        assert!(
            reject_prohibited_destination(&uri("http://127.0.0.1:1/x"), &permitted)
                .await
                .is_ok()
        );
        // Permitting one address does not permit its neighbours.
        assert!(matches!(
            reject_prohibited_destination(&uri("http://127.0.0.2:1/x"), &permitted).await,
            Err(ErrorCode::DestinationIpProhibited)
        ));
    }

    #[tokio::test]
    async fn reject_prohibited_destination_admits_an_operator_permitted_name() {
        // A name is admitted only when every address it resolves onto is
        // permitted, so "localhost" needs both families listed: one bad
        // answer among several is still a refusal.
        let permitted = [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ];
        assert!(
            reject_prohibited_destination(&uri("http://localhost:1/x"), &permitted)
                .await
                .is_ok()
        );
        // Permitting only one family leaves the other refused.
        assert!(matches!(
            reject_prohibited_destination(
                &uri("http://localhost:1/x"),
                &[IpAddr::V6(Ipv6Addr::LOCALHOST)]
            )
            .await,
            Err(ErrorCode::DestinationIpProhibited)
        ));
    }

    #[tokio::test]
    async fn reject_prohibited_destination_rejects_a_name_resolving_to_loopback() {
        // "localhost" resolves to a loopback address on every platform this
        // runs on, via /etc/hosts or the stub resolver, with no real network
        // access - the same property the existing loopback test-server helpers
        // below rely on implicitly.
        assert!(matches!(
            reject_prohibited_destination(&uri("http://localhost:1/x"), &[]).await,
            Err(ErrorCode::DestinationIpProhibited)
        ));
    }

    #[tokio::test]
    async fn reject_prohibited_destination_does_not_deny_on_its_own_resolution_failure() {
        // A name that cannot resolve is not itself a security denial: the
        // real send path resolves again and reports its own DNS error.
        assert!(
            reject_prohibited_destination(
                &uri("http://this-name-does-not-resolve.invalid.test:1/x"),
                &[],
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn send_request_rejects_an_allowlisted_hostname_that_resolves_to_loopback() {
        // "localhost" passes the string-match allowlist exactly as any other
        // allowlisted name would - the rejection has to come from resolving
        // it, not from `admit`, which never sees an IP at all here.
        let mut gate = HttpGate::new(
            "test-module",
            allow(&["localhost"]),
            None,
            limits(),
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            vec![],
        );
        let pending = gate
            .send_request(request("http://localhost:1/x"), config())
            .expect("hostname is allowlisted, so admit() alone passes it");
        let err = resolve(pending)
            .await
            .expect_err("resolving to loopback must still be rejected");
        assert!(matches!(err, ErrorCode::DestinationIpProhibited));
    }

    fn request(u: &str) -> http::Request<HyperOutgoingBody> {
        let body = Empty::<Bytes>::new()
            .map_err(|_| unreachable!("infallible body error"))
            .boxed_unsync();
        http::Request::builder()
            .method(http::Method::GET)
            .uri(u)
            .body(body)
            .expect("test request builds")
    }

    fn config() -> OutgoingRequestConfig {
        OutgoingRequestConfig {
            use_tls: false,
            connect_timeout: Duration::from_secs(1),
            first_byte_timeout: Duration::from_secs(1),
            between_bytes_timeout: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn send_request_denies_off_list_host_with_http_request_denied() {
        let mut gate = HttpGate::new(
            "test-module",
            allow(&["api.acme.example"]),
            None,
            limits(),
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            vec![],
        );
        let Err(err) = gate.send_request(request("http://evil.example/x"), config()) else {
            panic!("off-list host must be denied");
        };
        assert!(matches!(
            err.downcast_ref(),
            Some(ErrorCode::HttpRequestDenied)
        ));
    }

    #[tokio::test]
    async fn send_request_admits_listed_host() {
        // Nothing listens on 127.0.0.1:1; admission only hands the
        // request to the backend, so the returned future is pending.
        let mut gate = HttpGate::new(
            "test-module",
            allow(&["127.0.0.1"]),
            None,
            limits(),
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            vec![],
        );
        assert!(
            gate.send_request(request("http://127.0.0.1:1/x"), config())
                .is_ok()
        );
    }

    /// One denial carrying exactly these three labels: a fourth would be a new
    /// dimension on an operator-facing series, and a guest-chosen value there
    /// would let a module mint series at will.
    fn assert_one_denial(samples: &[Sample], reason: &str) {
        let hits = samples_named(samples, DENIALS);
        assert_eq!(hits.len(), 1, "one denial recorded: {samples:?}");
        let mut keys: Vec<&str> = hits[0].labels.iter().map(|(k, _)| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["capability", "module", "reason"]);
        assert!(hits[0].has_label("capability", "http"), "{:?}", hits[0]);
        assert!(hits[0].has_label("reason", reason), "{:?}", hits[0]);
        assert!(hits[0].has_label("module", "test-module"), "{:?}", hits[0]);
    }

    #[test]
    fn an_allowlist_denial_is_counted_against_the_module() {
        let ((), samples) = capture_metrics(|| {
            let mut gate = HttpGate::new(
                "test-module",
                allow(&["api.acme.example"]),
                None,
                limits(),
                vec![],
                vec![],
            );
            assert!(
                gate.send_request(request("http://evil.example/x"), config())
                    .is_err()
            );
        });
        assert_one_denial(&samples, "allowlist");
    }

    #[test]
    fn a_destination_denial_is_counted_against_the_module() {
        // The counter fires inside the spawned send task, so the capture has
        // to drive that task on the thread holding the recorder.
        let ((), samples) = capture_metrics(|| {
            block_on_current_thread(async {
                let mut gate = HttpGate::new(
                    "test-module",
                    allow(&["localhost"]),
                    None,
                    limits(),
                    vec![],
                    vec![],
                );
                let pending = gate
                    .send_request(request("http://localhost:1/x"), config())
                    .expect("the hostname is allowlisted");
                let err = resolve(pending).await.expect_err("loopback is refused");
                assert!(matches!(err, ErrorCode::DestinationIpProhibited));
            });
        });
        assert_one_denial(&samples, "destination");
    }

    #[test]
    fn a_transport_failure_is_not_counted_as_a_denial() {
        // Nothing listens on port 1, so the send fails at connect. The gate
        // admitted the request, and only a refusal counts.
        let ((), samples) = capture_metrics(|| {
            block_on_current_thread(async {
                let mut gate = HttpGate::new(
                    "test-module",
                    allow(&["127.0.0.1"]),
                    None,
                    limits(),
                    vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
                    vec![],
                );
                let pending = gate
                    .send_request(request("http://127.0.0.1:1/x"), config())
                    .expect("listed and operator-permitted");
                assert!(resolve(pending).await.is_err());
            });
        });
        assert!(samples_named(&samples, DENIALS).is_empty(), "{samples:?}");
    }

    /// The effective host set is the intersection: the manifest list alone
    /// admits nothing the operator row excludes, and the operator row
    /// grants nothing the manifest never asked for.
    #[test]
    fn operator_http_allow_intersects_the_author_list() {
        let author = allow(&["api.cow.fi", "evil.example"]);
        let operator = allow(&["api.cow.fi", "unrequested.example"]);
        let both = |host: &str| super::admit(&uri(host), &author, Some(&operator));
        assert!(both("https://api.cow.fi/x").is_ok());
        assert!(matches!(
            both("https://evil.example/x"),
            Err(ErrorCode::HttpRequestDenied)
        ));
        assert!(matches!(
            both("https://unrequested.example/x"),
            Err(ErrorCode::HttpRequestDenied)
        ));
        // An empty operator row denies everything: narrowing to nothing.
        assert!(matches!(
            super::admit(&uri("https://api.cow.fi/x"), &author, Some(&[])),
            Err(ErrorCode::HttpRequestDenied)
        ));
    }

    #[tokio::test]
    async fn http_deny_refuses_a_range_even_when_otherwise_permitted() {
        let denied = ["203.0.113.0/24".parse::<IpNet>().expect("test CIDR")];
        assert!(matches!(
            super::reject_prohibited_destination(&uri("http://203.0.113.9:1/x"), &[], &denied)
                .await,
            Err(ErrorCode::DestinationIpProhibited)
        ));
        // Outside the denied range the public address stays reachable.
        assert!(
            super::reject_prohibited_destination(&uri("http://203.0.114.9:1/x"), &[], &denied)
                .await
                .is_ok()
        );
        // The deny wins over permit_destinations: the subtraction is last.
        let permitted = [IpAddr::V4(Ipv4Addr::LOCALHOST)];
        let deny_loopback = ["127.0.0.0/8".parse::<IpNet>().expect("test CIDR")];
        assert!(matches!(
            super::reject_prohibited_destination(
                &uri("http://127.0.0.1:1/x"),
                &permitted,
                &deny_loopback
            )
            .await,
            Err(ErrorCode::DestinationIpProhibited)
        ));
    }

    #[tokio::test]
    async fn http_deny_matches_the_ipv4_mapped_spelling_in_both_directions() {
        let denied = ["203.0.113.0/24".parse::<IpNet>().expect("test CIDR")];
        assert!(matches!(
            super::reject_prohibited_destination(
                &uri("http://[::ffff:203.0.113.9]:1/x"),
                &[],
                &denied
            )
            .await,
            Err(ErrorCode::DestinationIpProhibited)
        ));
        assert!(
            super::reject_prohibited_destination(
                &uri("http://[::ffff:203.0.114.9]:1/x"),
                &[],
                &denied
            )
            .await
            .is_ok()
        );
        let mapped = ["::ffff:203.0.113.0/120"
            .parse::<IpNet>()
            .expect("test CIDR")];
        assert!(matches!(
            super::reject_prohibited_destination(&uri("http://203.0.113.9:1/x"), &[], &mapped)
                .await,
            Err(ErrorCode::DestinationIpProhibited)
        ));
    }

    fn config_with(timeout: Duration) -> OutgoingRequestConfig {
        OutgoingRequestConfig {
            use_tls: false,
            connect_timeout: timeout,
            first_byte_timeout: timeout,
            between_bytes_timeout: timeout,
        }
    }

    #[test]
    fn clamp_lowers_each_timeout_above_its_maximum() {
        // 600 s is also what the linked handler substitutes for unset
        // request-options, so this doubles as the unset case: unset
        // resolves to the engine maximum.
        let clamped = clamp(config_with(Duration::from_secs(600)), &limits());
        assert_eq!(clamped.connect_timeout, Duration::from_secs(10));
        assert_eq!(clamped.first_byte_timeout, Duration::from_secs(10));
        assert_eq!(clamped.between_bytes_timeout, Duration::from_secs(10));
    }

    #[test]
    fn clamp_keeps_timeouts_below_the_maximum() {
        let clamped = clamp(config_with(Duration::from_secs(1)), &limits());
        assert_eq!(clamped.connect_timeout, Duration::from_secs(1));
        assert_eq!(clamped.first_byte_timeout, Duration::from_secs(1));
        assert_eq!(clamped.between_bytes_timeout, Duration::from_secs(1));
    }

    #[test]
    fn clamp_keeps_timeouts_at_the_maximum() {
        let clamped = clamp(config_with(Duration::from_secs(10)), &limits());
        assert_eq!(clamped.connect_timeout, Duration::from_secs(10));
        assert_eq!(clamped.first_byte_timeout, Duration::from_secs(10));
        assert_eq!(clamped.between_bytes_timeout, Duration::from_secs(10));
    }

    #[test]
    fn clamp_applies_each_maximum_independently() {
        let mut l = limits();
        l.first_byte_timeout_max = Duration::from_millis(50);
        let clamped = clamp(config_with(Duration::from_secs(5)), &l);
        assert_eq!(clamped.connect_timeout, Duration::from_secs(5));
        assert_eq!(clamped.first_byte_timeout, Duration::from_millis(50));
        assert_eq!(clamped.between_bytes_timeout, Duration::from_secs(5));
    }

    /// A detached executor for test-server tasks.
    fn test_executor() -> nexum_tasks::TaskExecutor {
        nexum_tasks::TaskManager::new().executor()
    }

    /// One-connection loopback server; `hold_open` stalls instead of sending
    /// EOF.
    async fn spawn_server(response: Vec<u8>, hold_open: bool) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener has a local addr");
        test_executor().spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(&response).await;
            let _ = sock.flush().await;
            if hold_open {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
        addr
    }

    async fn resolve(pending: HostFutureIncomingResponse) -> Result<IncomingResponse, ErrorCode> {
        match pending {
            HostFutureIncomingResponse::Pending(handle) => {
                handle.await.expect("send task never traps")
            }
            _ => panic!("send_request returns a pending response"),
        }
    }

    async fn send_to(
        addr: std::net::SocketAddr,
        limits: OutboundHttpLimits,
    ) -> Result<IncomingResponse, ErrorCode> {
        let mut gate = HttpGate::new(
            "test-module",
            allow(&["127.0.0.1"]),
            None,
            limits,
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            vec![],
        );
        let pending = gate
            .send_request(request(&format!("http://{addr}/x")), config_10s())
            .expect("listed host admitted");
        resolve(pending).await
    }

    fn config_10s() -> OutgoingRequestConfig {
        config_with(Duration::from_secs(10))
    }

    #[tokio::test]
    async fn request_under_all_limits_succeeds() {
        let addr = spawn_server(
            b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello".to_vec(),
            false,
        )
        .await;
        let incoming = send_to(addr, limits()).await.expect("response arrives");
        assert_eq!(incoming.resp.status(), 200);
        let body = incoming
            .resp
            .into_body()
            .collect()
            .await
            .expect("body is under the cap");
        assert_eq!(body.to_bytes().as_ref(), b"hello");
    }

    #[tokio::test]
    async fn total_deadline_fires_on_a_stalled_server() {
        // Accepts, never responds; every per-phase maximum is 10 s, so
        // only the total deadline can end the wait.
        let addr = spawn_server(Vec::new(), true).await;
        let mut l = limits();
        l.total_deadline = Duration::from_millis(250);
        let err = send_to(addr, l).await.expect_err("deadline fires");
        assert!(matches!(err, ErrorCode::ConnectionTimeout));
    }

    #[tokio::test]
    async fn total_deadline_fires_while_the_body_stalls() {
        // Headers plus 16 of 100000 promised body bytes, then a stall:
        // the deadline covers body streaming via the CappedBody wrapper.
        let mut response = b"HTTP/1.1 200 OK\r\ncontent-length: 100000\r\n\r\n".to_vec();
        response.extend_from_slice(&[b'x'; 16]);
        let addr = spawn_server(response, true).await;
        let mut l = limits();
        l.total_deadline = Duration::from_millis(300);
        let incoming = send_to(addr, l).await.expect("headers arrive in time");
        let err = incoming
            .resp
            .into_body()
            .collect()
            .await
            .expect_err("deadline fires mid-body");
        assert!(matches!(err, ErrorCode::ConnectionReadTimeout));
    }

    #[tokio::test]
    async fn deadline_tears_down_a_parked_unread_response() {
        // The guest obtains the response and never polls the body, so
        // the body-side deadline never runs; the raced connection
        // driver alone must close the socket, observable server-side
        // as EOF on a blocking read.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener has a local addr");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        test_executor().spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100000\r\n\r\n")
                .await;
            let _ = sock.flush().await;
            loop {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            let _ = tx.send(());
        });
        let mut l = limits();
        l.total_deadline = Duration::from_millis(300);
        let parked = send_to(addr, l).await.expect("headers arrive in time");
        let closed = tokio::time::timeout(Duration::from_secs(5), rx).await;
        assert!(
            closed.is_ok(),
            "server must see the close at the deadline while the response is parked"
        );
        drop(parked);
    }

    #[tokio::test]
    async fn oversized_response_body_fails_with_the_cap_in_the_error() {
        let mut response = b"HTTP/1.1 200 OK\r\ncontent-length: 4096\r\n\r\n".to_vec();
        response.extend_from_slice(&[b'x'; 4096]);
        let addr = spawn_server(response, false).await;
        let mut l = limits();
        l.response_body_max_bytes = 1024;
        let incoming = send_to(addr, l).await.expect("headers arrive");
        let err = incoming
            .resp
            .into_body()
            .collect()
            .await
            .expect_err("body exceeds the cap");
        assert!(matches!(err, ErrorCode::HttpResponseBodySize(Some(1024))));
    }

    #[tokio::test]
    async fn body_at_exactly_the_cap_passes() {
        let inner: HyperIncomingBody = Full::new(Bytes::from(vec![b'a'; 64]))
            .map_err(|_| unreachable!("infallible body error"))
            .boxed_unsync();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let body = CappedBody::new(inner, 64, deadline);
        let collected = body.collect().await.expect("exact-cap body passes");
        assert_eq!(collected.to_bytes().len(), 64);
    }
}
