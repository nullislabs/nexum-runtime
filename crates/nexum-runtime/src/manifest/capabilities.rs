//! Capability enforcement: cross-checks a component's WIT imports against
//! its `[dependencies]` declarations.
//!
//! The core `nexum:host` namespace is built in; each extension registers
//! its own via [`CapabilityRegistry::register`]. `wasi:http` is gated by
//! the `http` capability, `wasi:sockets` and `wasi:filesystem` by `wasi-*`;
//! io/clocks/random and `wasi:cli` are ambient; any other `wasi:` interface
//! is refused fail-closed.

use std::collections::HashSet;

use strum::VariantNames;

use super::error::{CapabilityError, CapabilityViolation};
use super::types::{CORE_CAPABILITIES, LoadedManifest};

/// A WIT namespace prefix plus the interface names under it that are
/// capabilities.
#[derive(Clone, Copy)]
pub struct NamespaceCaps {
    /// Interface-name prefix, e.g. `"nexum:host/"`.
    pub prefix: &'static str,
    /// Interface names under `prefix` that are capabilities.
    pub ifaces: &'static [&'static str],
}

/// The core namespace: the interfaces the `event-module` world links.
pub const CORE_NAMESPACE: NamespaceCaps = NamespaceCaps {
    prefix: "nexum:host/",
    ifaces: CORE_CAPABILITIES,
};

/// Interfaces a provider world links: the scoped transport plus `logging`.
/// `http` is gated by the registry, as in the core set.
pub const PROVIDER_CAPABILITIES: &[&str] = &[
    nexum_world::Cap::Chain.as_str(),
    nexum_world::Cap::Logging.as_str(),
];

/// The provider namespace: `nexum:host/` scoped to the transport
/// interfaces, so a provider declaring a core-only interface (e.g.
/// `local-store`) is rejected as unknown.
pub const PROVIDER_NAMESPACE: NamespaceCaps = NamespaceCaps {
    prefix: "nexum:host/",
    ifaces: PROVIDER_CAPABILITIES,
};

/// Import prefix of the `wasi:http` package; every interface under it is
/// gated by [`HTTP_CAPABILITY`].
const WASI_HTTP_PREFIX: &str = "wasi:http/";

/// Capability name a module declares to import any `wasi:http/*`
/// interface; the per-module `[dependencies.http].hosts` list scopes it.
const HTTP_CAPABILITY: &str = nexum_world::Cap::Http.as_str();

/// Gated WASI capability names; declaring one grants the matching `wasi:`
/// interface group. See [`classify_wasi`].
const WASI_CAPABILITIES: &[&str] = WasiCap::VARIANTS;

/// A gated WASI capability; the single source of the `wasi-*` name set.
#[derive(Clone, Copy, strum::IntoStaticStr, strum::VariantNames, strum::VariantArray)]
enum WasiCap {
    #[strum(serialize = "wasi-sockets")]
    Sockets,
    #[strum(serialize = "wasi-filesystem")]
    Filesystem,
}

impl WasiCap {
    const ALL: &'static [Self] = <Self as strum::VariantArray>::VARIANTS;

    fn as_str(self) -> &'static str {
        self.into()
    }

    /// The `wasi:` interface prefix this capability gates.
    const fn gated_prefix(self) -> &'static str {
        match self {
            Self::Sockets => "wasi:sockets/",
            Self::Filesystem => "wasi:filesystem/",
        }
    }
}

/// Always-linked `wasi:` prefixes: io, clocks, random, stdio/exit/terminal.
const AMBIENT_WASI_PREFIXES: &[&str] = &["wasi:io/", "wasi:clocks/", "wasi:random/", "wasi:cli/"];

/// A `wasi:` import (other than `wasi:http`) classified against the gate.
enum WasiGate {
    /// Always linked, never declared.
    Ambient,
    /// Usable only when the capability is declared.
    Gated(WasiCap),
    /// Unrecognized `wasi:` interface: refused fail-closed.
    Unknown,
}

/// Classify a non-http `wasi:` interface id, ignoring any `@version` suffix.
fn classify_wasi(import_name: &str) -> WasiGate {
    let iface = import_name.split('@').next().unwrap_or(import_name);
    if AMBIENT_WASI_PREFIXES.iter().any(|p| iface.starts_with(p)) {
        return WasiGate::Ambient;
    }
    WasiCap::ALL
        .iter()
        .find(|cap| iface.starts_with(cap.gated_prefix()))
        .map_or(WasiGate::Unknown, |&cap| WasiGate::Gated(cap))
}

/// Capability namespaces recognized by enforcement: the core namespace plus
/// every registered extension.
#[derive(Clone)]
pub struct CapabilityRegistry {
    namespaces: Vec<NamespaceCaps>,
}

impl Default for CapabilityRegistry {
    fn default() -> Self {
        Self::core()
    }
}

impl CapabilityRegistry {
    /// The registry with the core `nexum:host/` namespace.
    pub fn core() -> Self {
        Self {
            namespaces: vec![CORE_NAMESPACE],
        }
    }

    /// The registry a provider validates against: the scoped transport plus
    /// `logging` and `http`. A provider manifest declaring a core-only
    /// capability (e.g. `local-store`) fails as unknown.
    pub fn provider() -> Self {
        Self {
            namespaces: vec![PROVIDER_NAMESPACE],
        }
    }

    /// Add an extension's namespace.
    pub fn register(&mut self, ns: NamespaceCaps) {
        self.namespaces.push(ns);
    }

    /// Whether `name` is a capability under any registered namespace.
    pub fn is_known(&self, name: &str) -> bool {
        name == HTTP_CAPABILITY
            || WASI_CAPABILITIES.contains(&name)
            || self.namespaces.iter().any(|ns| ns.ifaces.contains(&name))
    }

    /// Comma-joined recognized capability names, for error messages.
    pub fn known_names(&self) -> String {
        self.namespaces
            .iter()
            .flat_map(|ns| ns.ifaces.iter().copied())
            .chain(std::iter::once(HTTP_CAPABILITY))
            .chain(WASI_CAPABILITIES.iter().copied())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Map a WIT import name to a capability name. `Some(iface)` for an
    /// interface under a registered namespace, `Some("http")` for anything
    /// under `wasi:http/`, and `None` for a non-capability import (type-only
    /// packages, ungated `wasi:*`).
    pub fn wit_import_to_cap<'a>(&self, import_name: &'a str) -> Option<&'a str> {
        let without_version = import_name.split('@').next().unwrap_or(import_name);
        if without_version.starts_with(WASI_HTTP_PREFIX) {
            return Some(HTTP_CAPABILITY);
        }
        for ns in &self.namespaces {
            if let Some(iface) = without_version.strip_prefix(ns.prefix)
                && ns.ifaces.contains(&iface)
            {
                return Some(iface);
            }
        }
        None
    }
}

/// Deny every gated import the manifest does not declare. Runs before
/// instantiation.
pub fn enforce_capabilities<'a>(
    loaded: &LoadedManifest,
    component_imports: impl Iterator<Item = &'a str>,
    registry: &CapabilityRegistry,
) -> Result<(), CapabilityError> {
    let declared: HashSet<&str> = loaded.dependencies.keys().map(String::as_str).collect();

    for import_name in component_imports {
        let without_version = import_name.split('@').next().unwrap_or(import_name);
        // `wasi:http` is gated by the registry below; the rest of the WASI
        // surface is gated here.
        if without_version.starts_with("wasi:") && !without_version.starts_with(WASI_HTTP_PREFIX) {
            match classify_wasi(import_name) {
                WasiGate::Ambient => {}
                WasiGate::Gated(cap) if declared.contains(cap.as_str()) => {}
                WasiGate::Gated(cap) => {
                    return Err(CapabilityViolation {
                        capability: cap.as_str().to_owned(),
                        wit_import: import_name.to_owned(),
                    }
                    .into());
                }
                WasiGate::Unknown => {
                    return Err(CapabilityError::UnknownWasi {
                        wit_import: import_name.to_owned(),
                    });
                }
            }
            continue;
        }
        if let Some(cap) = registry.wit_import_to_cap(import_name)
            && !declared.contains(cap)
        {
            return Err(CapabilityViolation {
                capability: cap.to_owned(),
                wit_import: import_name.to_owned(),
            }
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::types::{ComponentKind, Dependency, ResourceSection};

    /// A registry with one extension namespace registered, mirroring
    /// what a composition root assembles.
    fn registry_with_ext() -> CapabilityRegistry {
        let mut r = CapabilityRegistry::core();
        r.register(NamespaceCaps {
            prefix: "test:acme/",
            ifaces: &["acme-api"],
        });
        r
    }

    #[test]
    fn wit_import_to_cap_nexum_host() {
        let r = CapabilityRegistry::core();
        assert_eq!(r.wit_import_to_cap("nexum:host/chain@0.1.0"), Some("chain"));
        assert_eq!(
            r.wit_import_to_cap("nexum:host/local-store@0.1.0"),
            Some("local-store")
        );
    }

    #[test]
    fn wit_import_to_cap_wasi_http_maps_to_http() {
        let r = CapabilityRegistry::core();
        assert_eq!(
            r.wit_import_to_cap("wasi:http/outgoing-handler@0.2.12"),
            Some("http")
        );
        assert_eq!(r.wit_import_to_cap("wasi:http/types@0.2.12"), Some("http"));
        // Version-agnostic: the prefix decides, not the pinned version.
        assert_eq!(
            r.wit_import_to_cap("wasi:http/outgoing-handler@0.2.0"),
            Some("http")
        );
        assert_eq!(r.wit_import_to_cap("wasi:http/types"), Some("http"));
    }

    #[test]
    fn http_is_a_known_capability_name() {
        let r = CapabilityRegistry::core();
        assert!(r.is_known("http"));
        assert!(r.known_names().split(", ").any(|n| n == "http"));
    }

    #[test]
    fn wit_import_to_cap_extension_needs_registration() {
        // Core registry does not recognize an extension namespace.
        let core = CapabilityRegistry::core();
        assert_eq!(core.wit_import_to_cap("test:acme/acme-api@0.1.0"), None);
        // Once registered, it resolves.
        let r = registry_with_ext();
        assert_eq!(
            r.wit_import_to_cap("test:acme/acme-api@0.1.0"),
            Some("acme-api")
        );
    }

    #[test]
    fn wit_import_to_cap_non_http_wasi_is_none() {
        let r = registry_with_ext();
        assert_eq!(r.wit_import_to_cap("wasi:io/streams@0.2.0"), None);
        assert_eq!(r.wit_import_to_cap("wasi:cli/stdin@0.2.0"), None);
        assert_eq!(r.wit_import_to_cap("wasi:sockets/tcp@0.2.0"), None);
    }

    fn test_module_id() -> crate::module_id::ModuleId {
        crate::module_id::ModuleId::parse("test").expect("valid module name")
    }

    fn manifest_with_caps(required: &[&str]) -> LoadedManifest {
        LoadedManifest {
            name: test_module_id(),
            kind: ComponentKind::Module,
            component_digest: None,
            resources: ResourceSection::default(),
            dependencies: required
                .iter()
                .map(|s| ((*s).to_owned(), Dependency::default()))
                .collect(),
            http_allowlist: vec![],
            config: vec![],
            subscriptions: vec![],
            extensions: Default::default(),
        }
    }

    fn manifest_no_caps() -> LoadedManifest {
        manifest_with_caps(&[])
    }

    #[test]
    fn enforce_rejects_registry_import_when_caps_absent() {
        let loaded = manifest_no_caps();
        let r = registry_with_ext();
        let err =
            enforce_capabilities(&loaded, ["nexum:host/chain@0.1.0"].into_iter(), &r).unwrap_err();
        let CapabilityError::Undeclared(v) = err else {
            panic!("expected undeclared: {err:?}")
        };
        assert_eq!(v.capability, "chain");
        assert_eq!(v.wit_import, "nexum:host/chain@0.1.0");
    }

    #[test]
    fn enforce_accepts_ambient_wasi_when_caps_absent() {
        let loaded = manifest_no_caps();
        let r = registry_with_ext();
        let imports = ["wasi:io/streams@0.2.6", "wasi:clocks/wall-clock@0.2.6"];
        assert!(enforce_capabilities(&loaded, imports.into_iter(), &r).is_ok());
    }

    #[test]
    fn enforce_rejects_wasi_http_when_caps_absent() {
        let loaded = manifest_no_caps();
        let r = registry_with_ext();
        let err = enforce_capabilities(
            &loaded,
            ["wasi:http/outgoing-handler@0.2.12"].into_iter(),
            &r,
        )
        .unwrap_err();
        let CapabilityError::Undeclared(v) = err else {
            panic!("expected undeclared: {err:?}")
        };
        assert_eq!(v.capability, "http");
    }

    #[test]
    fn enforce_passes_when_all_imports_declared() {
        let loaded = manifest_with_caps(&["chain", "acme-api", "http"]);
        let imports = [
            "nexum:host/chain@0.1.0",
            "test:acme/acme-api@0.1.0",
            "wasi:http/outgoing-handler@0.2.12",
            "wasi:io/streams@0.2.0", // non-http wasi is always skipped
        ];
        let r = registry_with_ext();
        assert!(enforce_capabilities(&loaded, imports.into_iter(), &r).is_ok());
    }

    #[test]
    fn enforce_rejects_wasi_http_import_without_declaration() {
        let loaded = manifest_with_caps(&["chain"]);
        let imports = [
            "nexum:host/chain@0.1.0",
            "wasi:http/outgoing-handler@0.2.12",
        ];
        let r = registry_with_ext();
        let err = enforce_capabilities(&loaded, imports.into_iter(), &r).unwrap_err();
        let CapabilityError::Undeclared(v) = err else {
            panic!("expected undeclared: {err:?}")
        };
        assert_eq!(v.capability, "http");
        assert_eq!(v.wit_import, "wasi:http/outgoing-handler@0.2.12");
    }

    #[test]
    fn enforce_accepts_wasi_http_when_http_declared() {
        let loaded = manifest_with_caps(&["http"]);
        let imports = [
            "wasi:http/outgoing-handler@0.2.12",
            "wasi:http/types@0.2.12",
        ];
        let r = registry_with_ext();
        assert!(enforce_capabilities(&loaded, imports.into_iter(), &r).is_ok());
    }

    #[test]
    fn enforce_rejects_undeclared_import() {
        let loaded = manifest_with_caps(&["chain"]);
        // module imports remote-store but didn't declare it
        let imports = ["nexum:host/chain@0.1.0", "nexum:host/remote-store@0.1.0"];
        let r = registry_with_ext();
        let err = enforce_capabilities(&loaded, imports.into_iter(), &r).unwrap_err();
        let CapabilityError::Undeclared(v) = err else {
            panic!("expected undeclared: {err:?}")
        };
        assert_eq!(v.capability, "remote-store");
    }

    #[test]
    fn provider_registry_knows_the_scoped_set_and_no_core_only_caps() {
        // The scoped transport plus logging and http are known; the
        // core-only interfaces a provider must not reach are not, so a
        // manifest declaring them fails validation as unknown.
        let r = CapabilityRegistry::provider();
        assert!(r.is_known("chain"));
        assert!(r.is_known("logging"));
        assert!(r.is_known("http"));
        assert!(!r.is_known("local-store"));
        assert!(!r.is_known("remote-store"));
        assert!(!r.is_known("identity"));
    }

    #[test]
    fn provider_registry_maps_scoped_imports_but_not_core_only() {
        let r = CapabilityRegistry::provider();
        assert_eq!(r.wit_import_to_cap("nexum:host/chain@0.1.0"), Some("chain"));
        assert_eq!(
            r.wit_import_to_cap("nexum:host/logging@0.1.0"),
            Some("logging")
        );
        assert_eq!(
            r.wit_import_to_cap("wasi:http/outgoing-handler@0.2.12"),
            Some("http")
        );
        // A core-only interface is not a recognized provider capability.
        assert_eq!(r.wit_import_to_cap("nexum:host/local-store@0.1.0"), None);
    }

    #[test]
    fn provider_enforce_refuses_an_undeclared_logging_import() {
        let loaded = manifest_with_caps(&["chain"]);
        let r = CapabilityRegistry::provider();
        let err = enforce_capabilities(&loaded, ["nexum:host/logging@0.1.0"].into_iter(), &r)
            .unwrap_err();
        let CapabilityError::Undeclared(v) = err else {
            panic!("expected undeclared: {err:?}")
        };
        assert_eq!(v.capability, "logging");
        assert_eq!(v.wit_import, "nexum:host/logging@0.1.0");
    }

    #[test]
    fn provider_enforce_admits_a_declared_logging_import() {
        let loaded = manifest_with_caps(&["logging"]);
        let r = CapabilityRegistry::provider();
        assert!(
            enforce_capabilities(&loaded, ["nexum:host/logging@0.1.0"].into_iter(), &r).is_ok()
        );
    }

    #[test]
    fn provider_manifest_declaring_a_core_only_cap_is_unknown() {
        // The load path validates declared names against the registry; an
        // provider declaring `local-store` must surface as unknown.
        let r = CapabilityRegistry::provider();
        assert!(!r.is_known("local-store"));
        assert!(r.known_names().split(", ").all(|n| n != "local-store"));
    }

    #[test]
    fn ambient_wasi_needs_no_declaration() {
        let loaded = manifest_with_caps(&["logging"]);
        let imports = [
            "wasi:io/streams@0.2.6",
            "wasi:io/poll@0.2.6",
            "wasi:clocks/monotonic-clock@0.2.6",
            "wasi:clocks/wall-clock@0.2.6",
            "wasi:random/random@0.2.6",
            "wasi:cli/stdout@0.2.6",
            "wasi:cli/stdin@0.2.6",
            "wasi:cli/stderr@0.2.6",
            "wasi:cli/exit@0.2.6",
            "wasi:cli/terminal-stdout@0.2.6",
            "wasi:cli/environment@0.2.6",
        ];
        let r = registry_with_ext();
        assert!(enforce_capabilities(&loaded, imports.into_iter(), &r).is_ok());
    }

    #[test]
    fn undeclared_gated_wasi_is_refused() {
        let loaded = manifest_with_caps(&["logging"]);
        let r = registry_with_ext();
        for (import, cap) in [
            ("wasi:sockets/tcp@0.2.6", "wasi-sockets"),
            ("wasi:filesystem/types@0.2.6", "wasi-filesystem"),
        ] {
            let err = enforce_capabilities(&loaded, [import].into_iter(), &r).unwrap_err();
            let CapabilityError::Undeclared(v) = err else {
                panic!("expected undeclared for {import}: {err:?}")
            };
            assert_eq!(v.capability, cap);
            assert_eq!(v.wit_import, import);
        }
    }

    #[test]
    fn declared_gated_wasi_is_permitted() {
        let loaded = manifest_with_caps(&["wasi-sockets", "wasi-filesystem"]);
        let imports = [
            "wasi:sockets/tcp@0.2.6",
            "wasi:sockets/udp@0.2.6",
            "wasi:filesystem/types@0.2.6",
            "wasi:filesystem/preopens@0.2.6",
        ];
        let r = registry_with_ext();
        assert!(enforce_capabilities(&loaded, imports.into_iter(), &r).is_ok());
    }

    #[test]
    fn declaring_one_gated_cap_does_not_grant_another() {
        let loaded = manifest_with_caps(&["wasi-filesystem"]);
        let r = registry_with_ext();
        assert!(
            enforce_capabilities(&loaded, ["wasi:filesystem/types@0.2.6"].into_iter(), &r).is_ok()
        );
        assert!(enforce_capabilities(&loaded, ["wasi:sockets/tcp@0.2.6"].into_iter(), &r).is_err());
    }

    #[test]
    fn unknown_wasi_interface_is_refused_fail_closed() {
        // Even with an unrelated gated cap declared, an unrecognized wasi:
        // namespace is denied outright.
        let loaded = manifest_with_caps(&["wasi-sockets"]);
        let r = registry_with_ext();
        let err =
            enforce_capabilities(&loaded, ["wasi:nn/tensor@0.2.0"].into_iter(), &r).unwrap_err();
        assert!(matches!(err, CapabilityError::UnknownWasi { .. }));
    }

    #[test]
    fn wasi_gate_ignores_version_suffix() {
        let declared = manifest_with_caps(&["wasi-sockets"]);
        let none = manifest_with_caps(&["logging"]);
        let r = registry_with_ext();
        assert!(enforce_capabilities(&declared, ["wasi:sockets/tcp"].into_iter(), &r).is_ok());
        assert!(
            enforce_capabilities(&declared, ["wasi:sockets/tcp@0.2.6"].into_iter(), &r).is_ok()
        );
        assert!(enforce_capabilities(&none, ["wasi:filesystem/types"].into_iter(), &r).is_err());
    }

    #[test]
    fn wasi_capability_names_are_known() {
        let r = registry_with_ext();
        for cap in ["wasi-sockets", "wasi-filesystem"] {
            assert!(r.is_known(cap), "{cap} missing from known set");
            assert!(r.known_names().split(", ").any(|n| n == cap));
        }
    }
}
