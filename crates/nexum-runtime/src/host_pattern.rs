//! The outbound-HTTP allowlist pattern and its matcher, parsed once from
//! the author manifest (`[dependencies.http].hosts`) at load and matched
//! at request time.

use std::convert::Infallible;
use std::fmt;
use std::str::FromStr;

/// One allowlist entry, lowercased and classified at parse so a request
/// check is a plain string comparison.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(from = "String")]
pub enum HostPattern {
    /// Matches exactly this host.
    Exact(Box<str>),
    /// A `*.suffix` entry: matches any subdomain of `suffix`, never
    /// `suffix` itself. Stored without the `*.`; the matcher checks the
    /// dot boundary, so a hand-built payload cannot widen matching.
    Suffix(Box<str>),
}

impl HostPattern {
    fn parse(entry: &str) -> Self {
        let entry = entry.to_ascii_lowercase();
        match entry.strip_prefix("*.") {
            Some(suffix) => Self::Suffix(suffix.into()),
            None => Self::Exact(entry.into_boxed_str()),
        }
    }

    /// Case-insensitive and allocation-free, and it holds for any payload,
    /// not only a [`Self::parse`]d one: a `Suffix` match requires a `.`
    /// before the suffix, so the bare suffix never matches.
    fn matches(&self, host: &str) -> bool {
        match self {
            Self::Exact(exact) => host.eq_ignore_ascii_case(exact),
            Self::Suffix(suffix) => {
                let (host, suffix) = (host.as_bytes(), suffix.as_bytes());
                host.len() > suffix.len()
                    && host[host.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
                    && host[host.len() - suffix.len() - 1] == b'.'
            }
        }
    }
}

/// Total: every entry an operator or author writes is a working pattern,
/// exactly as when the list was plain strings.
impl FromStr for HostPattern {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Infallible> {
        Ok(Self::parse(s))
    }
}

impl From<&str> for HostPattern {
    fn from(s: &str) -> Self {
        Self::parse(s)
    }
}

impl From<String> for HostPattern {
    fn from(s: String) -> Self {
        Self::parse(&s)
    }
}

/// The spelling an operator or author writes, lowercased.
impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(exact) => f.write_str(exact),
            Self::Suffix(suffix) => write!(f, "*.{suffix}"),
        }
    }
}

/// Whether `host` matches any allowlist pattern. Case-insensitive and
/// host-only (no scheme or port; IPv6 literals keep their brackets).
pub(crate) fn host_allowed(host: &str, allowlist: &[HostPattern]) -> bool {
    allowlist.iter().any(|pattern| pattern.matches(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns(entries: &[&str]) -> Vec<HostPattern> {
        entries.iter().copied().map(HostPattern::from).collect()
    }

    #[test]
    fn parse_lowercases_and_classifies() {
        assert_eq!(
            HostPattern::from("API.Acme.Example"),
            HostPattern::Exact("api.acme.example".into())
        );
        assert_eq!(
            HostPattern::from("*.Acme.Example"),
            HostPattern::Suffix("acme.example".into())
        );
        // Only a leading `*.` is a wildcard; anything else stays exact.
        assert_eq!(
            HostPattern::from("acme.*.example"),
            HostPattern::Exact("acme.*.example".into())
        );
    }

    #[test]
    fn host_allowed_exact_and_wildcard() {
        let allow = patterns(&["api.acme.example", "*.discord.com"]);
        assert!(host_allowed("api.acme.example", &allow));
        assert!(!host_allowed("evil.api.acme.example", &allow));
        assert!(host_allowed("foo.discord.com", &allow));
        assert!(host_allowed("a.b.discord.com", &allow));
        assert!(!host_allowed("discord.com", &allow));
        assert!(!host_allowed("nope.example", &allow));
    }

    #[test]
    fn suffix_pattern_never_matches_the_bare_suffix() {
        let allow = patterns(&["*.acme.example"]);
        assert!(host_allowed("api.acme.example", &allow));
        assert!(!host_allowed("acme.example", &allow));
        // A host merely ending in the suffix text is not a subdomain.
        assert!(!host_allowed("notacme.example", &allow));
    }

    /// The variants are publicly constructible, so the subdomain rule must
    /// hold for a payload no parse produced.
    #[test]
    fn hand_built_payloads_cannot_widen_matching() {
        let suffix = [HostPattern::Suffix("acme.example".into())];
        assert!(host_allowed("api.acme.example", &suffix));
        assert!(!host_allowed("acme.example", &suffix));
        assert!(!host_allowed("notacme.example", &suffix));
        // A mixed-case payload matches as its lowercased spelling would,
        // instead of being a silently dead entry.
        let exact = [HostPattern::Exact("API.Acme.Example".into())];
        assert!(host_allowed("api.acme.example", &exact));
        assert!(!host_allowed("evil.api.acme.example", &exact));
    }

    #[test]
    fn ipv6_literal_patterns_keep_their_brackets() {
        let allow = patterns(&["[::1]"]);
        assert!(host_allowed("[::1]", &allow));
        assert!(!host_allowed("::1", &allow));
        assert!(!host_allowed("[2001:db8::1]", &allow));
    }

    /// One classification for all three conversions, so no load site can
    /// drift from the others.
    #[test]
    fn from_str_agrees_with_the_from_conversions() {
        for entry in ["API.Acme.Example", "*.Acme.Example", "[::1]", ""] {
            let parsed: HostPattern = entry.parse().expect("infallible");
            assert_eq!(parsed, HostPattern::from(entry));
            assert_eq!(parsed, HostPattern::from(entry.to_owned()));
        }
    }

    #[test]
    fn host_allowed_is_case_insensitive_both_ways() {
        let upper = patterns(&["API.ACME.EXAMPLE"]);
        let lower = patterns(&["api.acme.example"]);
        assert!(host_allowed("api.acme.example", &upper));
        assert!(host_allowed("Api.Acme.Example", &lower));
        assert!(host_allowed(
            "FOO.Discord.COM",
            &patterns(&["*.DISCORD.com"])
        ));
    }

    #[test]
    fn host_allowed_matches_hosts_not_authorities() {
        // Entries are bare hosts; a port or userinfo in a pattern can
        // never match a host string.
        let allow = patterns(&["api.acme.example:8443", "u@api.acme.example"]);
        assert!(!host_allowed("api.acme.example", &allow));
    }

    #[test]
    fn display_keeps_the_wildcard_spelling() {
        assert_eq!(
            HostPattern::from("*.acme.example").to_string(),
            "*.acme.example"
        );
        assert_eq!(
            HostPattern::from("api.acme.example").to_string(),
            "api.acme.example"
        );
    }
}
