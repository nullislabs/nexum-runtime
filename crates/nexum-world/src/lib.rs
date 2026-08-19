//! Per-component world synthesis: turn a manifest's `[dependencies]`
//! into an inline WIT world whose imports are exactly the declared
//! capability interfaces.
//!
//! Invariant: the capability rows must agree with the runtime's
//! capability registry on both names and WIT interfaces, since the
//! runtime cross-checks a component's imports against the manifest at
//! load time. [`CORE`] carries only the core `nexum:host` rows;
//! per-namespace rows come from a composition root's `extensions.toml`
//! ([`manifest_extensions`]) and are passed to [`synthesize`].

#![forbid(unsafe_code)]

use alloy_primitives::B256;
use std::path::{Path, PathBuf};
use strum::{Display, EnumString, IntoStaticStr, VariantNames};

/// A core capability name; the single source [`CORE`] and the runtime's
/// capability registry emit from.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Hash, Display, EnumString, IntoStaticStr, VariantNames,
)]
#[strum(serialize_all = "kebab-case")]
#[non_exhaustive]
pub enum Cap {
    /// `nexum:host/chain`.
    Chain,
    /// `nexum:host/local-store`.
    LocalStore,
    /// `nexum:host/logging`.
    Logging,
    /// Gates `wasi:http/*`; no world import.
    Http,
}

impl Cap {
    /// The declared name; the discriminant indexes `VARIANTS`, so this is const.
    pub const fn as_str(self) -> &'static str {
        Self::VARIANTS[self as usize]
    }
}

/// Gates host linking only: these emit no world import, so [`synthesize`]
/// does not accept them.
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    Hash,
    Display,
    EnumString,
    IntoStaticStr,
    VariantNames,
    strum::VariantArray,
)]
#[non_exhaustive]
pub enum WasiCap {
    /// Gates `wasi:sockets/*`.
    #[strum(serialize = "wasi-sockets")]
    Sockets,
    /// Gates `wasi:filesystem/*`.
    #[strum(serialize = "wasi-filesystem")]
    Filesystem,
}

impl WasiCap {
    /// Every variant, in declaration order.
    pub const ALL: &'static [Self] = <Self as strum::VariantArray>::VARIANTS;

    /// The declared name.
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// The `wasi:` interface prefix this capability gates.
    pub const fn gated_prefix(self) -> &'static str {
        match self {
            Self::Sockets => "wasi:sockets/",
            Self::Filesystem => "wasi:filesystem/",
        }
    }
}

/// [`Cap::Http`] then the [`WasiCap`] names, in declaration order. A
/// registry recognizes these without registration.
pub const WASI_GATES: [&str; 1 + WasiCap::VARIANTS.len()] = {
    let mut out = [""; 1 + WasiCap::VARIANTS.len()];
    out[0] = Cap::Http.as_str();
    let mut i = 0;
    while i < WasiCap::VARIANTS.len() {
        out[i + 1] = WasiCap::VARIANTS[i];
        i += 1;
    }
    out
};

/// A core `[[trigger]] on` value. A kind with no variant here is
/// extension-owned, so the set is the runtime's core/extension split.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Hash, Display, EnumString, IntoStaticStr, VariantNames,
)]
#[strum(serialize_all = "kebab-case")]
#[non_exhaustive]
pub enum TriggerKind {
    /// A new block on a chain.
    Block,
    /// A contract event's log matching the address and topic-0 filters.
    Event,
    /// A cron expression's time arriving.
    Schedule,
}

/// A `nexum:host/types.fault` case as a stable snake_case label, in WIT
/// declaration order; the single source every label mirror emits from.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Hash, Display, EnumString, IntoStaticStr, VariantNames,
)]
#[strum(serialize_all = "snake_case")]
#[non_exhaustive]
pub enum FaultLabel {
    /// `fault.unsupported`.
    Unsupported,
    /// `fault.unavailable`.
    Unavailable,
    /// `fault.denied`.
    Denied,
    /// `fault.rate-limited`.
    RateLimited,
    /// `fault.timeout`.
    Timeout,
    /// `fault.invalid-input`.
    InvalidInput,
    /// `fault.internal`.
    Internal,
}

/// The permitted JSON-RPC read surface as a closed type; the single
/// source the guest allowlist and host dispatch table emit from.
/// Signing and state-mutating methods have no variant, so cannot cross
/// the WIT edge.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, EnumString, IntoStaticStr)]
#[non_exhaustive]
pub enum ChainMethod {
    /// `eth_blockNumber`.
    #[strum(serialize = "eth_blockNumber")]
    EthBlockNumber,
    /// `eth_call`.
    #[strum(serialize = "eth_call")]
    EthCall,
    /// `eth_chainId`.
    #[strum(serialize = "eth_chainId")]
    EthChainId,
    /// `eth_estimateGas`.
    #[strum(serialize = "eth_estimateGas")]
    EthEstimateGas,
    /// `eth_feeHistory`.
    #[strum(serialize = "eth_feeHistory")]
    EthFeeHistory,
    /// `eth_gasPrice`.
    #[strum(serialize = "eth_gasPrice")]
    EthGasPrice,
    /// `eth_maxPriorityFeePerGas`.
    #[strum(serialize = "eth_maxPriorityFeePerGas")]
    EthMaxPriorityFeePerGas,
    /// `eth_getBalance`.
    #[strum(serialize = "eth_getBalance")]
    EthGetBalance,
    /// `eth_getBlockByHash`.
    #[strum(serialize = "eth_getBlockByHash")]
    EthGetBlockByHash,
    /// `eth_getBlockByNumber`.
    #[strum(serialize = "eth_getBlockByNumber")]
    EthGetBlockByNumber,
    /// `eth_getBlockReceipts`.
    #[strum(serialize = "eth_getBlockReceipts")]
    EthGetBlockReceipts,
    /// `eth_getCode`.
    #[strum(serialize = "eth_getCode")]
    EthGetCode,
    /// `eth_getLogs`.
    #[strum(serialize = "eth_getLogs")]
    EthGetLogs,
    /// `eth_getProof`.
    #[strum(serialize = "eth_getProof")]
    EthGetProof,
    /// `eth_getStorageAt`.
    #[strum(serialize = "eth_getStorageAt")]
    EthGetStorageAt,
    /// `eth_getTransactionByHash`.
    #[strum(serialize = "eth_getTransactionByHash")]
    EthGetTransactionByHash,
    /// `eth_getTransactionCount`.
    #[strum(serialize = "eth_getTransactionCount")]
    EthGetTransactionCount,
    /// `eth_getTransactionReceipt`.
    #[strum(serialize = "eth_getTransactionReceipt")]
    EthGetTransactionReceipt,
    /// `net_version`.
    #[strum(serialize = "net_version")]
    NetVersion,
}

impl ChainMethod {
    /// The wire method name.
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// One manifest capability and its world wiring.
pub struct Capability {
    /// The name declared as a `[dependencies]` key.
    pub name: Cap,
    /// The WIT import the declaration turns into; `None` for a
    /// capability with no world import (`http`).
    pub import: Option<&'static str>,
    /// WIT package directories the import needs on the resolve path,
    /// beyond `nexum-host`.
    pub packages: &'static [&'static str],
    /// The `bind_host_via_wit_bindgen!` capability ident for this
    /// capability's host-adapter pieces, if it has a trait seam.
    pub adapter: Option<&'static str>,
}

/// The core capability rows, in emission order. Mirrors the runtime's
/// core registry and nothing else; extension rows are the caller's.
pub const CORE: &[Capability] = &[
    Capability {
        name: Cap::Chain,
        import: Some("nexum:host/chain@0.1.0"),
        packages: &[],
        adapter: Some("chain"),
    },
    Capability {
        name: Cap::LocalStore,
        import: Some("nexum:host/local-store@0.1.0"),
        packages: &[],
        adapter: Some("local_store"),
    },
    Capability {
        name: Cap::Logging,
        import: Some("nexum:host/logging@0.1.0"),
        packages: &[],
        adapter: Some("logging"),
    },
    Capability {
        name: Cap::Http,
        import: None,
        packages: &[],
        adapter: None,
    },
];

/// Number of import-bearing [`CORE`] rows.
const fn core_iface_count() -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < CORE.len() {
        if CORE[i].import.is_some() {
            n += 1;
        }
        i += 1;
    }
    n
}

/// Names of the import-bearing [`CORE`] rows, in emission order; the
/// `nexum:host` interface set the runtime enforces. `http` is absent.
pub const CORE_IFACES: [&str; core_iface_count()] = {
    let mut out = [""; core_iface_count()];
    let mut n = 0;
    let mut i = 0;
    while i < CORE.len() {
        if CORE[i].import.is_some() {
            out[n] = CORE[i].name.as_str();
            n += 1;
        }
        i += 1;
    }
    out
};

/// One registered extension row: a per-namespace capability declared in
/// a composition root's `extensions.toml`. Always has a WIT import,
/// never an adapter ident (adapter seams are core-only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionRow {
    /// The name a component declares as a `[dependencies]` key.
    pub name: String,
    /// The WIT import the declaration turns into.
    pub import: String,
    /// WIT package directories the import needs on the resolve path,
    /// beyond `nexum-host`, in dependency order.
    pub packages: Vec<String>,
}

/// The synthesized world plus what the `generate!` call and the host
/// adapter need to go with it.
#[derive(Debug)]
pub struct ModuleWorld {
    /// Inline WIT text defining `nexum:module-world/module`.
    pub wit: String,
    /// WIT package directories the resolve path must carry, in
    /// dependency order (a package precedes its dependants).
    pub packages: Vec<String>,
    /// Capability idents to pass to `bind_host_via_wit_bindgen!`.
    pub adapters: Vec<&'static str>,
}

/// A refusal from manifest reading or world synthesis. The `Display`
/// text is operator-facing wording: the proc macro surfaces it verbatim
/// as a compile error, so it is pinned by test.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WorldError {
    /// A manifest failed to parse as TOML.
    #[error("{file} is not valid TOML: {source}")]
    NotToml {
        /// The manifest's conventional file name.
        file: &'static str,
        /// The TOML parse failure.
        #[source]
        source: toml::de::Error,
    },
    /// The manifest still carries the replaced `[capabilities]` section.
    #[error(
        "[capabilities] is replaced by [dependencies]: each key names a host \
         capability or a service, and its table carries that dependency's \
         attributes (the http allowlist is now `http = {{{{ hosts = [...] }}}}`)"
    )]
    ReplacedCapabilities,
    /// The manifest declares no `[dependencies]` table.
    #[error(
        "component.toml has no [dependencies] table; the macro derives the component's \
         WIT world from it, so declare it (an empty table is valid)"
    )]
    MissingDependencies,
    /// `[dependencies]` is not a table.
    #[error("[dependencies] must be a table")]
    DependenciesNotATable,
    /// A `[dependencies]` value is not a table.
    #[error("[dependencies].{name} must be a table; an empty one is `{name} = {{}}`")]
    DependencyNotATable {
        /// The dependency key.
        name: String,
    },
    /// `[[trigger]]` is not an array of tables.
    #[error("[[trigger]] must be an array of tables")]
    TriggersNotAnArray,
    /// An event trigger's `event_signature` is not a string.
    #[error("[[trigger]].event_signature must be a string")]
    EventSignatureNotAString,
    /// An event trigger `event_signature` that is not 32-byte hex.
    // Pinned operator wording; mirrors the runtime's load-time refusal.
    #[error("invalid topic {topic:?}: {source}")]
    InvalidTopic {
        /// The topic as written.
        topic: String,
        /// The hex parse failure.
        #[source]
        source: alloy_primitives::hex::FromHexError,
    },
    /// `[extensions]` is not a table.
    #[error("[extensions] must be a table of `[extensions.<name>]` rows")]
    ExtensionsNotATable,
    /// An `[extensions.<name>]` row is not a table.
    #[error("[extensions.{name}] must be a table")]
    ExtensionNotATable {
        /// The extension name.
        name: String,
    },
    /// An `[extensions.<name>]` row has no string `import`.
    #[error("[extensions.{name}] must carry a string `import`")]
    ExtensionMissingImport {
        /// The extension name.
        name: String,
    },
    /// An `[extensions.<name>].packages` value is not an array.
    #[error("[extensions.{name}].packages must be an array of strings")]
    ExtensionPackagesNotAnArray {
        /// The extension name.
        name: String,
    },
    /// An `[extensions.<name>].packages` item is not a string.
    #[error("[extensions.{name}].packages must contain only strings")]
    ExtensionPackageNotAString {
        /// The extension name.
        name: String,
    },
    /// A registered extension name shadows a core capability or another
    /// registration.
    #[error(
        "extension capability `{name}` collides with an already-registered capability; \
         names must be unique across the core table and the registered extensions"
    )]
    ExtensionCollision {
        /// The colliding name.
        name: String,
    },
    /// A declared dependency named no core capability and no registered
    /// extension.
    #[error(
        "unknown dependency `{name}` in component.toml [dependencies]; expected one of: {}",
        .known.join(", ")
    )]
    UnknownDependency {
        /// The unrecognized name.
        name: String,
        /// The recognized names: core capabilities, then registered
        /// extensions.
        known: Vec<String>,
    },
    /// No `wit/` tree exists under the build root or any ancestor.
    #[error("no `wit/` tree exists under {} or any ancestor", .start.display())]
    NoWitTree {
        /// The directory the search started from.
        start: PathBuf,
    },
    /// A needed WIT package is absent from the nearest `wit/` tree.
    #[error(
        "declared capabilities need the `{package}` WIT package, but neither \
         `wit/deps/{package}` nor `wit/{package}` exists in {}",
        .wit.display()
    )]
    MissingWitPackage {
        /// The package directory name.
        package: String,
        /// The `wit/` tree that was searched.
        wit: PathBuf,
    },
    /// `CARGO_MANIFEST_DIR` is absent, so there is no crate root to
    /// resolve against.
    #[error("CARGO_MANIFEST_DIR is not set")]
    NoManifestDir,
}

/// The declared dependency names from `[dependencies]` in the manifest
/// text. A missing or malformed `[dependencies]` table is an
/// error.
pub fn manifest_capabilities(text: &str) -> Result<Vec<String>, WorldError> {
    let value: toml::Table = text.parse().map_err(|source| WorldError::NotToml {
        file: "component.toml",
        source,
    })?;
    if value.get("capabilities").is_some() {
        return Err(WorldError::ReplacedCapabilities);
    }
    let deps = value
        .get("dependencies")
        .ok_or(WorldError::MissingDependencies)?;
    let table = deps.as_table().ok_or(WorldError::DependenciesNotATable)?;
    for (name, spec) in table {
        if !spec.is_table() {
            return Err(WorldError::DependencyNotATable { name: name.clone() });
        }
    }
    Ok(table.keys().cloned().collect())
}

/// The distinct event trigger `event_signature` topics from the manifest
/// text, in declaration order. Same hex grammar as the runtime's load.
pub fn manifest_event_topics(text: &str) -> Result<Vec<B256>, WorldError> {
    let value: toml::Table = text.parse().map_err(|source| WorldError::NotToml {
        file: "component.toml",
        source,
    })?;
    let Some(triggers) = value.get("trigger") else {
        return Ok(Vec::new());
    };
    let triggers = triggers.as_array().ok_or(WorldError::TriggersNotAnArray)?;
    let mut topics = Vec::new();
    for trigger in triggers {
        let kind = trigger
            .get("on")
            .and_then(toml::Value::as_str)
            .map(str::parse::<TriggerKind>);
        if !matches!(kind, Some(Ok(TriggerKind::Event))) {
            continue;
        }
        let Some(raw) = trigger.get("event_signature") else {
            continue;
        };
        let raw = raw.as_str().ok_or(WorldError::EventSignatureNotAString)?;
        let topic: B256 = raw.parse().map_err(|source| WorldError::InvalidTopic {
            topic: raw.to_owned(),
            source,
        })?;
        if !topics.contains(&topic) {
            topics.push(topic);
        }
    }
    Ok(topics)
}

/// The registered extension rows from an `extensions.toml`. Each
/// `[extensions.<name>]` table carries a WIT `import` and the extra
/// `packages` its resolve path needs. No `[extensions]` section
/// registers nothing.
pub fn manifest_extensions(text: &str) -> Result<Vec<ExtensionRow>, WorldError> {
    let value: toml::Table = text.parse().map_err(|source| WorldError::NotToml {
        file: "extensions.toml",
        source,
    })?;
    let Some(extensions) = value.get("extensions") else {
        return Ok(Vec::new());
    };
    let extensions = extensions
        .as_table()
        .ok_or(WorldError::ExtensionsNotATable)?;
    extensions
        .iter()
        .map(|(name, row)| {
            let row = row
                .as_table()
                .ok_or_else(|| WorldError::ExtensionNotATable { name: name.clone() })?;
            let import = row
                .get("import")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| WorldError::ExtensionMissingImport { name: name.clone() })?
                .to_owned();
            let packages = match row.get("packages") {
                None => Vec::new(),
                Some(value) => value
                    .as_array()
                    .ok_or_else(|| WorldError::ExtensionPackagesNotAnArray { name: name.clone() })?
                    .iter()
                    .map(|item| {
                        item.as_str().map(str::to_owned).ok_or_else(|| {
                            WorldError::ExtensionPackageNotAString { name: name.clone() }
                        })
                    })
                    .collect::<Result<_, _>>()?,
            };
            Ok(ExtensionRow {
                name: name.clone(),
                import,
                packages,
            })
        })
        .collect()
}

/// The extension registry for a build rooted at `start`: the nearest
/// ancestor `extensions.toml`, or `None`.
pub fn find_extensions_manifest(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(cur) = dir {
        let candidate = cur.join("extensions.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = cur.parent();
    }
    None
}

/// The per-module world from the declared capability names. A request is
/// granted whole or the module refuses at boot, so every emitted import is
/// one the module holds. `extensions` rows emit after the core rows. An
/// unknown name is an error; an extension name that shadows a core row
/// or another registration is an error.
pub fn synthesize(
    declared: &[String],
    extensions: &[ExtensionRow],
) -> Result<ModuleWorld, WorldError> {
    for (idx, ext) in extensions.iter().enumerate() {
        if ext.name.parse::<Cap>().is_ok()
            || extensions[..idx].iter().any(|prior| prior.name == ext.name)
        {
            return Err(WorldError::ExtensionCollision {
                name: ext.name.clone(),
            });
        }
    }

    let mut caps = Vec::new();
    for name in declared {
        match name.parse::<Cap>() {
            Ok(cap) => caps.push(cap),
            Err(_) if extensions.iter().any(|e| &e.name == name) => {}
            Err(_) => {
                let known = Cap::VARIANTS
                    .iter()
                    .map(|cap| (*cap).to_owned())
                    .chain(extensions.iter().map(|e| e.name.clone()))
                    .collect();
                return Err(WorldError::UnknownDependency {
                    name: name.clone(),
                    known,
                });
            }
        }
    }

    let mut imports = String::new();
    // `nexum:host` is a leaf package (the `trigger` variant carries status
    // transitions as opaque bytes), so the base resolve set
    // is the host package alone; capability declarations append their
    // own packages. Dependency order: each directory is parsed against
    // the packages before it, so a package precedes its dependants.
    let mut packages = vec!["nexum-host".to_owned()];
    let mut adapters = Vec::new();
    for cap in CORE {
        if !caps.contains(&cap.name) {
            continue;
        }
        if let Some(import) = cap.import {
            imports.push_str(&format!("    import {import};\n"));
        }
        for package in cap.packages {
            if !packages.iter().any(|p| p == package) {
                packages.push((*package).to_owned());
            }
        }
        if let Some(adapter) = cap.adapter {
            adapters.push(adapter);
        }
    }
    for ext in extensions {
        if !declared.contains(&ext.name) {
            continue;
        }
        imports.push_str(&format!("    import {};\n", ext.import));
        for package in &ext.packages {
            if !packages.contains(package) {
                packages.push(package.clone());
            }
        }
    }

    let mut wit = String::from(
        "package nexum:module-world;\n\nworld module {\n    \
         use nexum:host/types@0.1.0.{config, trigger, fault};\n\n",
    );
    wit.push_str(&imports);
    wit.push_str(
        "\n    export init: func(config: config) -> result<_, fault>;\n    \
         export on-trigger: func(trigger: trigger) -> result<_, fault>;\n}\n",
    );

    Ok(ModuleWorld {
        wit,
        packages,
        adapters,
    })
}

/// Resolve each WIT package directory for a build rooted at `start`.
/// The nearest ancestor `wit/` tree is the sole authority: vendored
/// `wit/deps/<package>` before owned `wit/<package>`. A package missing
/// from that tree is an error; outer trees are never consulted, so a
/// group cannot leak WIT it has not vendored.
pub fn resolve_wit_packages<S: AsRef<str>>(
    start: &Path,
    packages: &[S],
) -> Result<Vec<PathBuf>, WorldError> {
    let wit = find_wit_tree(start).ok_or_else(|| WorldError::NoWitTree {
        start: start.to_owned(),
    })?;
    packages
        .iter()
        .map(|package| {
            let package = package.as_ref();
            resolve_wit_package(&wit, package).ok_or_else(|| WorldError::MissingWitPackage {
                package: package.to_owned(),
                wit: wit.clone(),
            })
        })
        .collect()
}

/// The consuming crate's manifest directory, the root every crate-local
/// lookup starts from.
pub fn manifest_dir() -> Result<PathBuf, WorldError> {
    std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .map_err(|_| WorldError::NoManifestDir)
}

/// [`resolve_wit_packages`] rooted at [`manifest_dir`], as the strings
/// `wit_bindgen::generate!` takes for its `path`.
pub fn manifest_wit_packages<S: AsRef<str>>(packages: &[S]) -> Result<Vec<String>, WorldError> {
    Ok(resolve_wit_packages(&manifest_dir()?, packages)?
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect())
}

/// Whether a type is a plain named path (`Foo`), the only shape a module
/// export type may take.
#[cfg(feature = "macros")]
pub fn is_plain_type(ty: &syn::Type) -> bool {
    matches!(ty, syn::Type::Path(tp) if tp.qself.is_none())
}

/// The nearest ancestor `wit/` directory of `start`: the crate-local or
/// group-local WIT tree the build resolves against.
fn find_wit_tree(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(cur) = dir {
        let wit = cur.join("wit");
        if wit.is_dir() {
            return Some(wit);
        }
        dir = cur.parent();
    }
    None
}

/// One package directory within a WIT tree: vendored `deps/<package>`
/// before owned `<package>`.
fn resolve_wit_package(wit: &Path, package: &str) -> Option<PathBuf> {
    [wit.join("deps").join(package), wit.join(package)]
        .into_iter()
        .find(|candidate| candidate.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The base package set every module world resolves against.
    const MODULE_PACKAGES: [&str; 1] = ["nexum-host"];

    /// A stand-in extension row, as a registered extension would pass.
    fn ext() -> Vec<ExtensionRow> {
        vec![ExtensionRow {
            name: "acme".to_owned(),
            import: "acme:ext/api@0.1.0".to_owned(),
            packages: vec!["acme-ext".to_owned()],
        }]
    }

    #[test]
    fn logging_only_world_imports_logging_alone() {
        let world = synthesize(&[Cap::Logging.to_string()], &[]).unwrap();
        assert!(world.wit.contains("import nexum:host/logging@0.1.0;"));
        assert!(!world.wit.contains("import nexum:host/chain"));
        assert_eq!(world.packages, MODULE_PACKAGES);
        assert_eq!(world.adapters, vec!["logging"]);
    }

    #[test]
    fn extension_row_emits_its_import_and_packages() {
        let world = synthesize(&[Cap::Logging.to_string(), "acme".to_string()], &ext()).unwrap();
        assert!(world.wit.contains("import acme:ext/api@0.1.0;"));
        assert_eq!(world.packages, vec!["nexum-host", "acme-ext"]);
    }

    #[test]
    fn undeclared_extension_row_stays_out_of_the_world() {
        let world = synthesize(&[Cap::Logging.to_string()], &ext()).unwrap();
        assert!(!world.wit.contains("acme"));
        assert_eq!(world.packages, MODULE_PACKAGES);
    }

    #[test]
    fn extension_shadowing_a_core_name_is_rejected() {
        let rows = vec![ExtensionRow {
            name: Cap::Chain.to_string(),
            import: "acme:ext/chain@0.1.0".to_owned(),
            packages: Vec::new(),
        }];
        let err = synthesize(&[Cap::Chain.to_string()], &rows).unwrap_err();
        assert!(matches!(
            &err,
            WorldError::ExtensionCollision { name } if name == "chain"
        ));
        // Operator-facing wording, pinned verbatim.
        assert_eq!(
            err.to_string(),
            "extension capability `chain` collides with an already-registered capability; \
             names must be unique across the core table and the registered extensions"
        );
    }

    #[test]
    fn duplicate_extension_registration_is_rejected() {
        let mut rows = ext();
        rows.extend(ext());
        let err = synthesize(&[], &rows).unwrap_err();
        assert!(matches!(
            &err,
            WorldError::ExtensionCollision { name } if name == "acme"
        ));
        assert!(
            err.to_string()
                .contains("extension capability `acme` collides")
        );
    }

    #[test]
    fn core_ifaces_are_the_import_bearing_rows() {
        assert_eq!(
            CORE_IFACES,
            [
                Cap::Chain.as_str(),
                Cap::LocalStore.as_str(),
                Cap::Logging.as_str(),
            ],
        );
        assert!(!CORE_IFACES.contains(&Cap::Http.as_str()));
    }

    #[test]
    fn cap_accessor_agrees_with_the_derived_vocabulary() {
        let names: Vec<&str> = CORE.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, Cap::VARIANTS);
        for name in Cap::VARIANTS {
            let cap = name.parse::<Cap>().unwrap();
            assert_eq!(cap.as_str(), *name);
            assert_eq!(<&'static str>::from(cap), *name);
            assert_eq!(cap.to_string(), *name);
        }
    }

    #[test]
    fn wasi_cap_accessor_agrees_with_the_derived_vocabulary() {
        let names: Vec<&str> = WasiCap::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(names, WasiCap::VARIANTS);
        for name in WasiCap::VARIANTS {
            let cap = name.parse::<WasiCap>().unwrap();
            assert_eq!(cap.as_str(), *name);
            assert_eq!(<&'static str>::from(cap), *name);
            assert_eq!(cap.to_string(), *name);
        }
    }

    /// Pinned name set and order; the runtime's `known_names` wording
    /// depends on it.
    #[test]
    fn wasi_gates_are_http_then_the_gated_wasi_names() {
        assert_eq!(WASI_GATES, ["http", "wasi-sockets", "wasi-filesystem"]);
    }

    #[test]
    fn each_gated_prefix_matches_its_capability_name() {
        for cap in WasiCap::ALL {
            let group = cap.as_str().strip_prefix("wasi-").unwrap();
            assert_eq!(cap.gated_prefix(), format!("wasi:{group}/"));
        }
    }

    #[test]
    fn fault_labels_are_snake_case_and_distinct() {
        for label in FaultLabel::VARIANTS {
            assert!(label.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        }
        let mut labels = FaultLabel::VARIANTS.to_vec();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), FaultLabel::VARIANTS.len());
    }

    #[test]
    fn fault_label_parses_back_from_its_label() {
        for label in FaultLabel::VARIANTS {
            let parsed: FaultLabel = label.parse().unwrap();
            assert_eq!(<&'static str>::from(parsed), *label);
            assert_eq!(parsed.to_string(), *label);
        }
        assert!("nonesuch".parse::<FaultLabel>().is_err());
    }

    #[test]
    fn core_table_carries_no_extension_row() {
        assert!(
            CORE.iter()
                .all(|c| c.import.is_none_or(|i| i.starts_with("nexum:host/")))
        );
        assert!(CORE.iter().all(|c| c.packages.is_empty()));
    }

    #[test]
    fn every_import_bearing_core_row_carries_an_adapter() {
        // `http` has no world import (SDK wasi:http client) and no
        // adapter; every other core row has both.
        for cap in CORE {
            assert_eq!(
                cap.import.is_some(),
                cap.adapter.is_some(),
                "{}",
                cap.name.as_str()
            );
        }
    }

    #[test]
    fn full_declaration_emits_every_adapter_in_core_order() {
        let declared: Vec<String> = CORE.iter().map(|c| c.name.as_str().to_owned()).collect();
        let world = synthesize(&declared, &[]).unwrap();
        assert_eq!(world.adapters, vec!["chain", "local_store", "logging"]);
    }

    #[test]
    fn http_declares_no_world_import() {
        let world = synthesize(&[Cap::Logging.to_string(), Cap::Http.to_string()], &[]).unwrap();
        assert!(!world.wit.contains("wasi:http"));
        assert_eq!(world.packages, MODULE_PACKAGES);
    }

    #[test]
    fn duplicate_declarations_emit_one_import() {
        let world = synthesize(&[Cap::Chain.to_string(), Cap::Chain.to_string()], &[]).unwrap();
        assert_eq!(world.wit.matches("import nexum:host/chain").count(), 1);
        assert_eq!(world.adapters, vec!["chain"]);
    }

    #[test]
    fn unknown_capability_is_rejected_with_the_known_list() {
        let err = synthesize(&["telepathy".to_string()], &ext()).unwrap_err();
        // The known set is a typed field: core capabilities, then extensions.
        assert!(matches!(
            &err,
            WorldError::UnknownDependency { name, known }
                if name == "telepathy" && known.last().is_some_and(|k| k == "acme")
        ));
        // Operator-facing wording and order, pinned verbatim.
        assert_eq!(
            err.to_string(),
            "unknown dependency `telepathy` in component.toml [dependencies]; expected one of: \
             chain, local-store, logging, http, acme"
        );
    }

    #[test]
    fn manifest_extensions_reads_rows() {
        let rows = manifest_extensions(
            r#"
[extensions.acme]
import = "acme:ext/api@0.1.0"
packages = ["acme-base", "acme-ext"]

[extensions.beta]
import = "beta:ext/api@0.1.0"
"#,
        )
        .unwrap();
        assert_eq!(rows, {
            let mut expected = ext();
            expected[0].packages = vec!["acme-base".to_owned(), "acme-ext".to_owned()];
            expected.push(ExtensionRow {
                name: "beta".to_owned(),
                import: "beta:ext/api@0.1.0".to_owned(),
                packages: Vec::new(),
            });
            expected
        });
    }

    #[test]
    fn manifest_without_extensions_section_registers_nothing() {
        assert_eq!(manifest_extensions("").unwrap(), Vec::new());
    }

    #[test]
    fn extension_row_without_an_import_is_an_error() {
        let err = manifest_extensions("[extensions.acme]\npackages = []\n").unwrap_err();
        assert!(matches!(
            &err,
            WorldError::ExtensionMissingImport { name } if name == "acme"
        ));
        // Operator-facing wording, pinned verbatim.
        assert_eq!(
            err.to_string(),
            "[extensions.acme] must carry a string `import`"
        );
    }

    #[test]
    fn extension_row_with_non_string_package_is_an_error() {
        let err =
            manifest_extensions("[extensions.acme]\nimport = \"a:b/c@0.1.0\"\npackages = [1]\n")
                .unwrap_err();
        assert!(matches!(
            &err,
            WorldError::ExtensionPackageNotAString { name } if name == "acme"
        ));
        // Operator-facing wording, pinned verbatim.
        assert_eq!(
            err.to_string(),
            "[extensions.acme].packages must contain only strings"
        );
    }

    #[test]
    fn manifest_capabilities_reads_the_dependency_keys() {
        let caps = manifest_capabilities(
            r#"
[component]
name = "probe"

[dependencies]
logging = {}
http = { hosts = ["api.acme.example"] }
"#,
        )
        .unwrap();
        // Keys come back sorted, since the table is a map and not a list.
        assert_eq!(caps, vec!["http", "logging"]);
    }

    #[test]
    fn manifest_capabilities_refuses_the_replaced_capabilities_section() {
        // The macro runs at build time, so an author on the old shape gets a
        // compile error naming the replacement rather than a boot refusal.
        let err = manifest_capabilities(
            r#"
[capabilities]
required = ["logging"]
"#,
        )
        .expect_err("the replaced section must refuse");
        assert!(matches!(err, WorldError::ReplacedCapabilities));
        // Operator-facing wording, pinned verbatim (the doubled braces are
        // the shipped text).
        assert_eq!(
            err.to_string(),
            "[capabilities] is replaced by [dependencies]: each key names a host \
             capability or a service, and its table carries that dependency's \
             attributes (the http allowlist is now `http = {{ hosts = [...] }}`)"
        );
    }

    #[test]
    fn manifest_capabilities_refuses_a_bare_dependency_name() {
        // A list would lose the attribute table, so a non-table value is an
        // error naming the empty-table spelling.
        let err = manifest_capabilities(
            r#"
[dependencies]
logging = "yes"
"#,
        )
        .expect_err("a non-table dependency must refuse");
        assert!(matches!(
            &err,
            WorldError::DependencyNotATable { name } if name == "logging"
        ));
        // Operator-facing wording, pinned verbatim.
        assert_eq!(
            err.to_string(),
            "[dependencies].logging must be a table; an empty one is `logging = {}`"
        );
    }

    /// Pin the operator-facing wording of the refusals no other test
    /// triggers; a module author sees this text as a compile error.
    #[test]
    fn remaining_refusals_pin_the_operator_wording() {
        let err = manifest_capabilities("=").unwrap_err();
        assert!(matches!(&err, WorldError::NotToml { file, .. } if *file == "component.toml"));
        assert!(
            err.to_string()
                .starts_with("component.toml is not valid TOML: "),
            "{err}"
        );
        let err = manifest_extensions("=").unwrap_err();
        assert!(matches!(&err, WorldError::NotToml { file, .. } if *file == "extensions.toml"));
        assert!(
            err.to_string()
                .starts_with("extensions.toml is not valid TOML: "),
            "{err}"
        );
        let err = manifest_capabilities("dependencies = 7\n").unwrap_err();
        assert!(matches!(err, WorldError::DependenciesNotATable));
        assert_eq!(err.to_string(), "[dependencies] must be a table");
        let err = manifest_event_topics("trigger = 7\n").unwrap_err();
        assert!(matches!(err, WorldError::TriggersNotAnArray));
        assert_eq!(err.to_string(), "[[trigger]] must be an array of tables");
        let err = manifest_extensions("extensions = 7\n").unwrap_err();
        assert!(matches!(err, WorldError::ExtensionsNotATable));
        assert_eq!(
            err.to_string(),
            "[extensions] must be a table of `[extensions.<name>]` rows"
        );
        let err = manifest_extensions("[extensions]\nacme = 7\n").unwrap_err();
        assert!(matches!(
            &err,
            WorldError::ExtensionNotATable { name } if name == "acme"
        ));
        assert_eq!(err.to_string(), "[extensions.acme] must be a table");
        let err =
            manifest_extensions("[extensions.acme]\nimport = \"a:b/c@0.1.0\"\npackages = 7\n")
                .unwrap_err();
        assert!(matches!(
            &err,
            WorldError::ExtensionPackagesNotAnArray { name } if name == "acme"
        ));
        assert_eq!(
            err.to_string(),
            "[extensions.acme].packages must be an array of strings"
        );
        assert_eq!(
            WorldError::NoManifestDir.to_string(),
            "CARGO_MANIFEST_DIR is not set"
        );
    }

    /// Pinned manifest grammar; the runtime's serde renames derive from it.
    #[test]
    fn trigger_kinds_spell_the_manifest_grammar() {
        assert_eq!(TriggerKind::VARIANTS, ["block", "event", "schedule"]);
        assert!("log".parse::<TriggerKind>().is_err());
        assert!("chain-log".parse::<TriggerKind>().is_err());
        assert!("cron".parse::<TriggerKind>().is_err());
    }

    #[test]
    fn event_topics_are_distinct_and_in_declaration_order() {
        let topics = manifest_event_topics(
            r#"
[[trigger]]
on       = "event"
chain_id = 1
event_signature = "0xcf5f9de2984132265203b5c335b25727702ca77262ff622e136baa7362bf1da9"

[[trigger]]
on       = "block"
chain_id = 1

[[trigger]]
on       = "event"
chain_id = 100
event_signature = "CF5F9DE2984132265203B5C335B25727702CA77262FF622E136BAA7362BF1DA9"

[[trigger]]
on       = "event"
chain_id = 1
event_signature = "0x0000000000000000000000000000000000000000000000000000000000000001"
"#,
        )
        .unwrap();
        assert_eq!(
            topics,
            vec![
                "0xcf5f9de2984132265203b5c335b25727702ca77262ff622e136baa7362bf1da9"
                    .parse::<B256>()
                    .unwrap(),
                B256::with_last_byte(1),
            ],
        );
    }

    #[test]
    fn event_topics_skip_wildcard_and_foreign_triggers() {
        let text = r#"
[[trigger]]
on       = "event"
chain_id = 1

[[trigger]]
on = "acme-status"
event_signature = "not-hex-but-not-ours"
"#;
        assert_eq!(manifest_event_topics(text).unwrap(), Vec::<B256>::new());
        assert_eq!(manifest_event_topics("").unwrap(), Vec::<B256>::new());
    }

    #[test]
    fn event_topic_refusal_pins_the_operator_wording() {
        let err = manifest_event_topics(
            "[[trigger]]\non = \"event\"\nchain_id = 1\n\
             event_signature = \"not-a-topic\"\n",
        )
        .unwrap_err();
        assert!(matches!(
            &err,
            WorldError::InvalidTopic { topic, .. } if topic == "not-a-topic"
        ));
        assert!(
            err.to_string()
                .starts_with("invalid topic \"not-a-topic\":"),
            "{err}"
        );
        let err = manifest_event_topics(
            "[[trigger]]\non = \"event\"\nchain_id = 1\nevent_signature = 7\n",
        )
        .unwrap_err();
        assert!(matches!(err, WorldError::EventSignatureNotAString));
        // Operator-facing wording, pinned verbatim.
        assert_eq!(
            err.to_string(),
            "[[trigger]].event_signature must be a string"
        );
    }

    #[test]
    fn manifest_without_a_dependency_table_is_an_error() {
        let err = manifest_capabilities("[component]\nname = \"x\"\n").unwrap_err();
        assert!(matches!(err, WorldError::MissingDependencies));
        // Operator-facing wording, pinned verbatim.
        assert_eq!(
            err.to_string(),
            "component.toml has no [dependencies] table; the macro derives the component's \
             WIT world from it, so declare it (an empty table is valid)"
        );
    }

    #[test]
    fn world_is_valid_wit_shape() {
        // Not a full WIT parse (that is the module build's job); pin the
        // structural pieces the runtime contract depends on.
        let world = synthesize(&[Cap::Logging.to_string()], &[]).unwrap();
        assert!(world.wit.starts_with("package nexum:module-world;"));
        assert!(world.wit.contains("world module {"));
        assert!(
            world
                .wit
                .contains("export init: func(config: config) -> result<_, fault>;")
        );
        assert!(
            world
                .wit
                .contains("export on-trigger: func(trigger: trigger) -> result<_, fault>;")
        );
    }

    #[test]
    fn resolution_prefers_vendored_deps_over_own_wit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("wit/deps/pkg")).unwrap();
        std::fs::create_dir_all(root.join("wit/pkg")).unwrap();
        let paths = resolve_wit_packages(root, &["pkg"]).unwrap();
        assert_eq!(paths, vec![root.join("wit/deps/pkg")]);
    }

    #[test]
    fn resolution_falls_back_to_the_nearest_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("wit/pkg")).unwrap();
        let leaf = root.join("crates/leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        let paths = resolve_wit_packages(&leaf, &["pkg"]).unwrap();
        assert_eq!(paths, vec![root.join("wit/pkg")]);
    }

    #[test]
    fn crate_local_package_shadows_the_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("wit/pkg")).unwrap();
        let leaf = root.join("crates/leaf");
        std::fs::create_dir_all(leaf.join("wit/deps/pkg")).unwrap();
        let paths = resolve_wit_packages(&leaf, &["pkg"]).unwrap();
        assert_eq!(paths, vec![leaf.join("wit/deps/pkg")]);
    }

    #[test]
    fn extension_registry_resolves_from_the_nearest_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("extensions.toml"), "").unwrap();
        let leaf = root.join("crates/leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        assert_eq!(
            find_extensions_manifest(&leaf),
            Some(root.join("extensions.toml"))
        );
    }

    #[test]
    fn absent_extension_registry_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(find_extensions_manifest(dir.path()), None);
    }

    #[test]
    fn missing_package_names_the_paths_tried() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("wit")).unwrap();
        let err = resolve_wit_packages(dir.path(), &["pkg"]).unwrap_err();
        assert!(matches!(
            &err,
            WorldError::MissingWitPackage { package, wit }
                if package == "pkg" && *wit == dir.path().join("wit")
        ));
        // Operator-facing wording, pinned verbatim.
        assert_eq!(
            err.to_string(),
            format!(
                "declared capabilities need the `pkg` WIT package, but neither \
                 `wit/deps/pkg` nor `wit/pkg` exists in {}",
                dir.path().join("wit").display()
            )
        );
    }

    #[test]
    fn absent_wit_tree_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_wit_packages(dir.path(), &["pkg"]).unwrap_err();
        assert!(matches!(
            &err,
            WorldError::NoWitTree { start } if *start == dir.path()
        ));
        // Operator-facing wording, pinned verbatim.
        assert_eq!(
            err.to_string(),
            format!(
                "no `wit/` tree exists under {} or any ancestor",
                dir.path().display()
            )
        );
    }

    #[test]
    fn nearest_tree_never_falls_through_to_an_outer_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("wit/pkg")).unwrap();
        let leaf = root.join("crates/leaf");
        std::fs::create_dir_all(leaf.join("wit/deps/other")).unwrap();
        let err = resolve_wit_packages(&leaf, &["pkg"]).unwrap_err();
        assert!(matches!(
            err,
            WorldError::MissingWitPackage { ref package, .. } if package == "pkg"
        ));
    }

    #[test]
    fn read_surface_methods_parse() {
        for m in [
            "eth_call",
            "eth_blockNumber",
            "eth_getBalance",
            "eth_getLogs",
            "eth_getTransactionReceipt",
            "net_version",
        ] {
            assert!(ChainMethod::try_from(m).is_ok(), "{m} should parse");
        }
    }

    #[test]
    fn signing_and_mutating_methods_have_no_variant() {
        for m in [
            "eth_sign",
            "eth_signTransaction",
            "eth_sendTransaction",
            "eth_sendRawTransaction",
            "eth_accounts",
            "personal_sign",
            "personal_unlockAccount",
            "admin_peers",
            "debug_traceCall",
            "miner_start",
            "eth_notAMethod",
            "",
        ] {
            assert!(ChainMethod::try_from(m).is_err(), "{m} must be rejected");
        }
    }

    #[test]
    fn as_str_round_trips_the_wire_name() {
        assert_eq!(ChainMethod::EthCall.as_str(), "eth_call");
        assert_eq!(
            ChainMethod::try_from(ChainMethod::EthGetBalance.as_str()),
            Ok(ChainMethod::EthGetBalance),
        );
    }
}
