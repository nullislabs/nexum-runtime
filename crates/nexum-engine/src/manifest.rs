//! `module.toml` parser and capability-enforcement helpers (0.2 scope).
//!
//! 0.2 intentionally ships a slim subset of the manifest spec:
//!
//! - `[capabilities].required` is parsed and validated (names must be in
//!   the known capability set; the 0.2 reference engine always provides
//!   all of them, so this is a sanity check + future-proofing).
//! - `[capabilities].optional` is parsed and logged; trap-stub fallback
//!   for absent optionals is deferred to 0.3.
//! - `[capabilities.http].allow` is parsed and consulted by the `http`
//!   host impl before any outbound call.
//! - `[config]` is flattened to `Vec<(String, String)>` and passed to the
//!   module's `init`. Typed `config-value` variant is deferred to 0.3.
//!
//! When the manifest file is missing or has no `[capabilities]` section,
//! a deprecation warning is emitted and the engine falls back to 0.1
//! behaviour (treat every linked capability as required). This fallback
//! will be removed in 0.3.

use std::collections::HashSet;
use std::path::Path;

use serde::Deserialize;

/// Capability names recognised by the 0.2 reference engine. Matches the
/// interfaces the `shepherd` world links into the linker.
pub const KNOWN_CAPABILITIES: &[&str] = &[
    "chain",
    "identity",
    "local-store",
    "remote-store",
    "messaging",
    "logging",
    "clock",
    "random",
    "http",
    // Domain-extension caps (provided by the shepherd world only):
    "cow-api",
];

#[derive(Debug, Deserialize, Default)]
pub struct Manifest {
    #[serde(default)]
    pub module: ModuleSection,
    #[serde(default)]
    pub capabilities: Option<CapabilitiesSection>,
    #[serde(default)]
    pub config: toml::Table,
    /// Event subscriptions the runtime wires before calling
    /// `_init`. See `docs/02-modules-events-packaging.md` for the
    /// schema; 0.2 implements `block` and `log` kinds, `cron` is
    /// parsed and ignored (deferred to 0.3).
    #[serde(default, rename = "subscription")]
    pub subscriptions: Vec<Subscription>,
}

/// One `[[subscription]]` table in `module.toml`.
///
/// The discriminator is the `kind` field; remaining fields are
/// validated per-kind by the supervisor. Unknown kinds are surfaced
/// at load time so a typo does not silently disable an event source.
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Subscription {
    /// New-block events. Fan-out is shared per chain — the
    /// supervisor opens one subscription per chain id and routes to
    /// every module that asked for blocks on that chain.
    Block {
        /// EVM chain id.
        chain_id: u64,
    },
    /// Log events matching `address` + topic-0. Fan-out is
    /// per-module — the supervisor opens one subscription per
    /// `[[subscription]]` entry and tags emitted events with the
    /// owning module.
    Log {
        /// EVM chain id.
        chain_id: u64,
        /// Contract address as `0x`-prefixed 20-byte hex. Optional.
        #[serde(default)]
        address: Option<String>,
        /// Topic-0 of the event the module wants to consume. `0x`-
        /// prefixed 32-byte hex. Optional — when absent the
        /// subscription matches every event from the address(es).
        #[serde(default)]
        event_signature: Option<String>,
    },
    /// Cron-scheduled tick. 0.2 parses but does not dispatch; the
    /// supervisor emits a warning so the operator knows the
    /// declaration is currently inert. `schedule` is preserved so a
    /// 0.3 dispatcher can pick it up without re-parsing the manifest.
    Cron {
        /// Standard 5-field cron expression.
        #[allow(dead_code)]
        schedule: String,
    },
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)] // version + component parsed for future 0.3 hash-verification.
pub struct ModuleSection {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub component: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct CapabilitiesSection {
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default)]
    pub optional: Vec<String>,
    #[serde(default)]
    pub http: Option<HttpSection>,
}

#[derive(Debug, Deserialize, Default)]
pub struct HttpSection {
    #[serde(default)]
    pub allow: Vec<String>,
}

/// Errors returned while loading or validating a manifest.
#[derive(Debug)]
pub enum ParseError {
    Io(std::io::Error),
    Toml(toml::de::Error),
    UnknownCapability(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "manifest: i/o: {e}"),
            Self::Toml(e) => write!(f, "manifest: parse: {e}"),
            Self::UnknownCapability(name) => write!(
                f,
                "manifest: unknown capability {:?} in [capabilities].required (known: {})",
                name,
                KNOWN_CAPABILITIES.join(", ")
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// Loaded + validated manifest, plus its source path for diagnostics.
#[derive(Debug)]
pub struct LoadedManifest {
    pub manifest: Manifest,
    /// Hosts to allow for `http::fetch`. Each entry is either an exact
    /// hostname or a `*.suffix` wildcard.
    pub http_allowlist: Vec<String>,
    /// `[config]` flattened to `(key, stringified-value)` pairs ready to
    /// hand to a module's `init`. TOML scalars (string, integer, float,
    /// boolean) become their text form. Arrays and tables are rendered as
    /// their TOML representation.
    pub config: Vec<(String, String)>,
}

/// Error returned when a component's WIT imports exceed its declared capabilities.
#[derive(Debug)]
pub struct CapabilityViolation {
    /// Capability name (e.g. `"remote-store"`).
    pub capability: String,
    /// Full WIT import name as it appeared in the component (e.g.
    /// `"nexum:host/remote-store@0.2.0"`).
    pub wit_import: String,
}

impl std::fmt::Display for CapabilityViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "component imports `{}` ({}) but it is not listed in \
             [capabilities].required or [capabilities].optional",
            self.capability, self.wit_import
        )
    }
}

impl std::error::Error for CapabilityViolation {}

/// Check that every capability-bearing WIT import of the component is covered
/// by the module's manifest declarations. Call this after loading the
/// component but before instantiation.
///
/// When `[capabilities]` is absent the manifest is in 0.1-fallback mode and
/// all imports are allowed; the caller is expected to have already emitted
/// a deprecation warning.
///
/// `component_imports` should be the iterator returned by
/// `component.component_type().imports(&engine)` — pass the **name** part
/// (`&str`) of each `(&str, ComponentItem)` tuple.
pub fn enforce_capabilities<'a>(
    loaded: &LoadedManifest,
    component_imports: impl Iterator<Item = &'a str>,
) -> Result<(), CapabilityViolation> {
    let caps = match loaded.manifest.capabilities.as_ref() {
        None => return Ok(()), // 0.1-fallback: no enforcement
        Some(c) => c,
    };

    let declared: HashSet<&str> = caps
        .required
        .iter()
        .chain(caps.optional.iter())
        .map(String::as_str)
        .collect();

    for import_name in component_imports {
        if let Some(cap) = wit_import_to_cap(import_name) {
            if !declared.contains(cap) {
                return Err(CapabilityViolation {
                    capability: cap.to_owned(),
                    wit_import: import_name.to_owned(),
                });
            }
        }
    }
    Ok(())
}

/// Map a WIT import name to a capability name, or `None` for non-capability
/// imports.
///
/// Returns `Some` only for functional interfaces that appear in
/// `KNOWN_CAPABILITIES`. Type-only packages (e.g. `nexum:host/types`) and
/// WASI system interfaces are treated as non-capability and ignored.
///
/// Examples:
/// - `"nexum:host/chain@0.2.0"`      → `Some("chain")`
/// - `"shepherd:cow/cow-api@0.2.0"`  → `Some("cow-api")`
/// - `"nexum:host/types@0.2.0"`      → `None` (type-only, not a capability)
/// - `"wasi:io/streams@0.2.0"`       → `None`
fn wit_import_to_cap(import_name: &str) -> Option<&str> {
    let without_version = import_name.split('@').next().unwrap_or(import_name);
    let iface = if let Some(i) = without_version.strip_prefix("nexum:host/") {
        i
    } else if let Some(i) = without_version.strip_prefix("shepherd:cow/") {
        i
    } else {
        return None;
    };
    // Only return Some for functional capabilities. Type-only packages
    // (like nexum:host/types) are shared data definitions, not capabilities.
    if KNOWN_CAPABILITIES.contains(&iface) { Some(iface) } else { None }
}

/// Read `module.toml` from `path`, parse, validate, and emit a deprecation
/// warning if `[capabilities]` is absent (0.1-compat fallback).
pub fn load(path: &Path) -> Result<LoadedManifest, ParseError> {
    let raw = std::fs::read_to_string(path).map_err(ParseError::Io)?;
    let manifest: Manifest = toml::from_str(&raw).map_err(ParseError::Toml)?;

    let caps = manifest.capabilities.as_ref();
    if caps.is_none() {
        eprintln!(
            "[deprecation] no [capabilities] section in module.toml — \
             defaulting to all-required (0.1 behaviour). This default \
             will be removed in 0.3; add an explicit [capabilities] block."
        );
    }

    if let Some(c) = caps {
        let known: HashSet<&str> = KNOWN_CAPABILITIES.iter().copied().collect();
        for name in c.required.iter().chain(c.optional.iter()) {
            if !known.contains(name.as_str()) {
                return Err(ParseError::UnknownCapability(name.clone()));
            }
        }
        if !c.required.is_empty() {
            eprintln!(
                "[manifest] required capabilities: {}",
                c.required.join(", ")
            );
        }
        if !c.optional.is_empty() {
            eprintln!(
                "[manifest] optional capabilities (advisory in 0.2; trap-stub fallback \
                 ships in 0.3): {}",
                c.optional.join(", ")
            );
        }
    }

    let http_allowlist = caps
        .and_then(|c| c.http.as_ref())
        .map(|h| h.allow.clone())
        .unwrap_or_default();
    if !http_allowlist.is_empty() {
        eprintln!("[manifest] http allowlist: {}", http_allowlist.join(", "));
    }

    let config = manifest
        .config
        .iter()
        .map(|(k, v)| (k.clone(), stringify_toml_value(v)))
        .collect();

    Ok(LoadedManifest {
        manifest,
        http_allowlist,
        config,
    })
}

/// Synthesise a "0.1 fallback" manifest for when no `module.toml` is found.
/// Emits the same deprecation warning as a missing-section manifest.
pub fn fallback_manifest() -> LoadedManifest {
    eprintln!(
        "[deprecation] no module.toml found — defaulting to all-required \
         (0.1 behaviour). This default will be removed in 0.3; ship a \
         module.toml alongside your component."
    );
    LoadedManifest {
        manifest: Manifest::default(),
        http_allowlist: Vec::new(),
        config: Vec::new(),
    }
}

/// Check whether `host` matches any pattern in the allowlist. Patterns are
/// either exact (`api.example.com`) or `*.suffix` wildcards which match
/// any subdomain of `suffix` (but not `suffix` itself).
pub fn host_allowed(host: &str, allowlist: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    allowlist.iter().any(|pat| {
        let pat = pat.to_ascii_lowercase();
        if let Some(suffix) = pat.strip_prefix("*.") {
            host.ends_with(&format!(".{suffix}"))
        } else {
            host == pat
        }
    })
}

/// Extract the host component from a URL. Returns `None` for non-http(s)
/// schemes or malformed input. Intentionally simple — adds no `url`
/// crate dependency.
pub fn extract_host(url: &str) -> Option<&str> {
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host_end = after_scheme
        .find('/')
        .or_else(|| after_scheme.find('?'))
        .unwrap_or(after_scheme.len());
    let host = &after_scheme[..host_end];
    // strip optional user-info and port.
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() { None } else { Some(host) }
}

fn stringify_toml_value(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        toml::Value::Array(_) | toml::Value::Table(_) => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_host_handles_common_shapes() {
        assert_eq!(
            extract_host("https://api.example.com/v1/x"),
            Some("api.example.com")
        );
        assert_eq!(extract_host("http://example.com"), Some("example.com"));
        assert_eq!(
            extract_host("https://user:pw@host.example.com:8443/x"),
            Some("host.example.com")
        );
        assert_eq!(extract_host("https://example.com?q=1"), Some("example.com"));
        assert_eq!(extract_host("ftp://example.com"), None);
        assert_eq!(extract_host("not a url"), None);
    }

    #[test]
    fn host_allowed_exact_and_wildcard() {
        let allow = vec!["api.cow.fi".to_string(), "*.discord.com".to_string()];
        assert!(host_allowed("api.cow.fi", &allow));
        assert!(!host_allowed("evil.api.cow.fi", &allow));
        assert!(host_allowed("foo.discord.com", &allow));
        assert!(host_allowed("a.b.discord.com", &allow));
        assert!(!host_allowed("discord.com", &allow));
        assert!(!host_allowed("nope.example", &allow));
    }

    // ── capability enforcement ────────────────────────────────────────────

    #[test]
    fn wit_import_to_cap_nexum_host() {
        assert_eq!(wit_import_to_cap("nexum:host/chain@0.2.0"), Some("chain"));
        assert_eq!(
            wit_import_to_cap("nexum:host/local-store@0.2.0"),
            Some("local-store")
        );
        assert_eq!(wit_import_to_cap("nexum:host/http@0.2.0"), Some("http"));
    }

    #[test]
    fn wit_import_to_cap_shepherd_cow() {
        assert_eq!(
            wit_import_to_cap("shepherd:cow/cow-api@0.2.0"),
            Some("cow-api")
        );
    }

    #[test]
    fn wit_import_to_cap_wasi_is_none() {
        assert_eq!(wit_import_to_cap("wasi:io/streams@0.2.0"), None);
        assert_eq!(wit_import_to_cap("wasi:cli/stdin@0.2.0"), None);
    }

    fn manifest_with_caps(required: &[&str], optional: &[&str]) -> LoadedManifest {
        LoadedManifest {
            manifest: Manifest {
                capabilities: Some(CapabilitiesSection {
                    required: required.iter().map(|s| s.to_string()).collect(),
                    optional: optional.iter().map(|s| s.to_string()).collect(),
                    http: None,
                }),
                ..Default::default()
            },
            http_allowlist: vec![],
            config: vec![],
        }
    }

    fn manifest_no_caps() -> LoadedManifest {
        LoadedManifest {
            manifest: Manifest::default(),
            http_allowlist: vec![],
            config: vec![],
        }
    }

    #[test]
    fn enforce_passes_when_caps_absent() {
        let loaded = manifest_no_caps();
        let imports = ["nexum:host/chain@0.2.0", "nexum:host/remote-store@0.2.0"];
        assert!(enforce_capabilities(&loaded, imports.into_iter()).is_ok());
    }

    #[test]
    fn enforce_passes_when_all_imports_declared() {
        let loaded = manifest_with_caps(&["chain", "cow-api"], &["http"]);
        let imports = [
            "nexum:host/chain@0.2.0",
            "shepherd:cow/cow-api@0.2.0",
            "nexum:host/http@0.2.0",
            "wasi:io/streams@0.2.0",
        ];
        assert!(enforce_capabilities(&loaded, imports.into_iter()).is_ok());
    }

    #[test]
    fn enforce_rejects_undeclared_import() {
        let loaded = manifest_with_caps(&["chain"], &[]);
        let imports = ["nexum:host/chain@0.2.0", "nexum:host/remote-store@0.2.0"];
        let err = enforce_capabilities(&loaded, imports.into_iter()).unwrap_err();
        assert_eq!(err.capability, "remote-store");
    }

    #[test]
    fn enforce_optional_caps_are_also_allowed() {
        let loaded = manifest_with_caps(&["chain"], &["remote-store"]);
        let imports = ["nexum:host/chain@0.2.0", "nexum:host/remote-store@0.2.0"];
        assert!(enforce_capabilities(&loaded, imports.into_iter()).is_ok());
    }

    // ── manifest parsing ──────────────────────────────────────────────────

    #[test]
    fn load_parses_block_and_log_subscriptions() {
        let toml = r#"
[module]
name = "twap-monitor"

[capabilities]
required = ["chain", "local-store"]

[[subscription]]
kind     = "block"
chain_id = 1

[[subscription]]
kind     = "log"
chain_id = 1
address  = "0xC92E8bdf79f0507f65a392b0ab4667716BFE0110"
event_signature = "0x00000000000000000000000000000000000000000000000000000000deadbeef"
"#;
        let manifest: Manifest = toml::from_str(toml).expect("parse");
        assert_eq!(manifest.module.name, "twap-monitor");
        assert_eq!(manifest.subscriptions.len(), 2);
        assert!(matches!(
            &manifest.subscriptions[0],
            Subscription::Block { chain_id: 1 }
        ));
        if let Subscription::Log { chain_id, address, .. } = &manifest.subscriptions[1] {
            assert_eq!(*chain_id, 1);
            assert!(address.is_some());
        } else {
            panic!("expected Log subscription");
        }
    }

    #[test]
    fn load_parses_cron_subscription() {
        let toml = r#"
[module]
name = "scheduler"

[[subscription]]
kind     = "cron"
schedule = "*/5 * * * *"
"#;
        let manifest: Manifest = toml::from_str(toml).expect("parse");
        assert!(matches!(
            &manifest.subscriptions[0],
            Subscription::Cron { .. }
        ));
    }

    #[test]
    fn load_rejects_unknown_capability() {
        let toml = r#"
[module]
name = "bad"

[capabilities]
required = ["chain", "not-a-real-cap"]
"#;
        // Write to a temp file so load() can read it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("module.toml");
        std::fs::write(&path, toml).unwrap();
        let err = load(&path).unwrap_err();
        assert!(matches!(err, ParseError::UnknownCapability(ref name) if name == "not-a-real-cap"));
    }

    #[test]
    fn load_parses_config_table() {
        let toml = r#"
[module]
name = "example"

[config]
chain_id = 1
label    = "mainnet"
enabled  = true
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("module.toml");
        std::fs::write(&path, toml).unwrap();
        let loaded = load(&path).unwrap();
        let config: std::collections::HashMap<_, _> = loaded.config.into_iter().collect();
        assert_eq!(config.get("chain_id").map(String::as_str), Some("1"));
        assert_eq!(config.get("label").map(String::as_str), Some("mainnet"));
        assert_eq!(config.get("enabled").map(String::as_str), Some("true"));
    }
}
