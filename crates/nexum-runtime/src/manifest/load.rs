//! Parse and validate `component.toml`, plus the host-matching helper the
//! wasi:http gate uses to enforce the http dependency's `hosts`.

use std::path::Path;

use tracing::info;

use super::capabilities::CapabilityRegistry;
use super::error::ParseError;
use super::types::{LoadedManifest, Manifest};

/// Parse and validate `component.toml`; no `[dependencies]` table refuses the
/// manifest (`required = []` is valid).
pub fn load(path: &Path, registry: &CapabilityRegistry) -> Result<LoadedManifest, ParseError> {
    let raw = std::fs::read_to_string(path)?;
    let manifest: Manifest = toml::from_str(&raw)?;

    validate_module_name(&manifest.component.name)?;

    let component_digest = manifest
        .component
        .digest
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(|source| ParseError::InvalidComponentDigest {
            value: manifest.component.digest.clone().unwrap_or_default(),
            source,
        })?;

    let deps = manifest
        .dependencies
        .as_ref()
        .ok_or(ParseError::MissingCapabilities)?;

    for (name, dep) in deps {
        if !registry.is_known(name) {
            return Err(ParseError::UnknownCapability {
                name: name.clone(),
                known: registry.known_names(),
            });
        }
        // `hosts` qualifies the http dependency and nothing else; accepting
        // it elsewhere would silently drop a grant the author believes in.
        if !dep.hosts.is_empty() && name != nexum_world::Cap::Http.as_str() {
            return Err(ParseError::MisplacedDependencyAttribute {
                dependency: name.clone(),
                attribute: "hosts",
            });
        }
    }
    if !deps.is_empty() {
        let names: Vec<&str> = deps.keys().map(String::as_str).collect();
        info!(target: "manifest", dependencies = %names.join(", "), "dependencies");
    }

    let http_allowlist = deps
        .get(nexum_world::Cap::Http.as_str())
        .map(|dep| dep.hosts.clone())
        .unwrap_or_default();
    if !http_allowlist.is_empty() {
        info!(target: "manifest", hosts = %http_allowlist.join(", "), "http hosts");
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
        component_digest,
    })
}

/// Reject a `[component].name` that is blank or not a single safe path
/// component, so it cannot escape the state directory.
fn validate_module_name(name: &str) -> Result<(), ParseError> {
    if name.trim().is_empty() {
        return Err(ParseError::BlankModuleName);
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(ParseError::InvalidModuleName(name.to_owned()));
    }
    Ok(())
}

/// Whether `host` matches any allowlist pattern: exact, or a `*.suffix`
/// wildcard matching any subdomain of `suffix` but not `suffix` itself.
/// Case-insensitive and host-only (no scheme or port; IPv6 literals keep
/// their brackets).
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
    use crate::manifest::types::Subscription;

    #[test]
    fn load_parses_block_and_chain_log_subscriptions() {
        let toml = r#"
[component]
name = "twap-monitor"

[dependencies]
chain = {}
local-store = {}

[[subscription]]
kind     = "block"
chain_id = 1

[[subscription]]
kind     = "chain-log"
chain_id = 1
address  = "0xC92E8bdf79f0507f65a392b0ab4667716BFE0110"
event_signature = "0x00000000000000000000000000000000000000000000000000000000deadbeef"
"#;
        let manifest: Manifest = toml::from_str(toml).expect("parse");
        assert_eq!(manifest.component.name, "twap-monitor");
        assert_eq!(manifest.subscriptions.len(), 2);
        assert!(matches!(
            &manifest.subscriptions[0],
            Subscription::Block { chain_id: 1 }
        ));
        if let Subscription::ChainLog {
            chain_id, address, ..
        } = &manifest.subscriptions[1]
        {
            assert_eq!(*chain_id, 1);
            assert!(address.is_some());
        } else {
            panic!("expected ChainLog subscription");
        }
    }

    /// Malformed chain-log hex refuses the manifest at parse, not at first
    /// dispatch, with the operator wording pinned verbatim.
    #[test]
    fn load_refuses_malformed_chain_log_hex_at_parse() {
        for (field, detail) in [
            (
                "address  = \"0xabc\"",
                "invalid chain-log address \"0xabc\"",
            ),
            (
                "event_signature = \"not-a-topic\"",
                "invalid topic \"not-a-topic\"",
            ),
        ] {
            let toml = format!(
                "[component]\nname = \"bad\"\n\n[[subscription]]\nkind     = \"chain-log\"\n\
                 chain_id = 1\n{field}\n"
            );
            let err = toml::from_str::<Manifest>(&toml).expect_err("malformed hex");
            // Foreign toml::de::Error; pins our hex message threaded through it.
            assert!(err.to_string().contains(detail), "{err}");
        }
    }

    /// Typing the field must neither widen nor narrow the accepted spelling:
    /// `0x`-prefixed or bare, any case, no checksum requirement.
    #[test]
    fn load_accepts_every_hex_spelling_of_a_chain_log_address() {
        let expected: alloy_primitives::Address = "0xc92e8bdf79f0507f65a392b0ab4667716bfe0110"
            .parse()
            .expect("canonical address");
        for spelling in [
            "0xC92E8bdf79f0507f65a392b0ab4667716BFE0110",
            "0xc92e8bdf79f0507f65a392b0ab4667716bfe0110",
            "0xC92E8BDF79F0507F65A392B0AB4667716BFE0110",
            "c92e8bdf79f0507f65a392b0ab4667716bfe0110",
        ] {
            let toml = format!(
                "[component]\nname = \"ok\"\n\n[[subscription]]\nkind     = \"chain-log\"\n\
                 chain_id = 1\naddress  = \"{spelling}\"\n"
            );
            let manifest: Manifest = toml::from_str(&toml).expect(spelling);
            assert!(
                matches!(
                    &manifest.subscriptions[0],
                    Subscription::ChainLog { address: Some(a), .. } if *a == expected
                ),
                "{spelling} must parse to the canonical address",
            );
        }
    }

    /// The macro-side topic extraction and the load-time parse read one
    /// grammar: a drift lets a build-checked manifest fail at load, or vice
    /// versa.
    #[test]
    fn world_topic_extraction_agrees_with_load() {
        let toml = r#"
[component]
name = "watcher"

[[subscription]]
kind     = "block"
chain_id = 1

[[subscription]]
kind     = "chain-log"
chain_id = 1
event_signature = "0xCF5F9DE2984132265203B5C335B25727702CA77262FF622E136BAA7362BF1DA9"

[[subscription]]
kind     = "chain-log"
chain_id = 1
event_signature = "0x0000000000000000000000000000000000000000000000000000000000000001"

[[subscription]]
kind     = "chain-log"
chain_id = 100
event_signature = "cf5f9de2984132265203b5c335b25727702ca77262ff622e136baa7362bf1da9"
"#;
        let manifest: Manifest = toml::from_str(toml).expect("parse");
        // Distinct, not `dedup`: the repeat is non-adjacent, as it is on chain.
        let mut loaded: Vec<alloy_primitives::B256> = Vec::new();
        for sub in &manifest.subscriptions {
            if let Subscription::ChainLog {
                event_signature: Some(topic),
                ..
            } = sub
                && !loaded.contains(topic)
            {
                loaded.push(*topic);
            }
        }
        assert_eq!(
            loaded.len(),
            2,
            "the fixture repeats a topic non-adjacently"
        );
        assert_eq!(
            nexum_world::manifest_chain_log_topics(toml).expect("extract"),
            loaded,
        );

        let bad = "[component]\nname = \"bad\"\n\n[[subscription]]\nkind = \"chain-log\"\n\
                   chain_id = 1\nevent_signature = \"not-a-topic\"\n";
        assert!(toml::from_str::<Manifest>(bad).is_err());
        assert!(nexum_world::manifest_chain_log_topics(bad).is_err());
    }

    #[test]
    fn load_parses_the_retired_log_kind_as_an_extension_kind() {
        // The chain-event kind is `chain-log`; a stale `kind = "log"`
        // parses as an extension kind and boot refuses it against the
        // extension vocabulary, so a not-yet-migrated manifest still
        // surfaces clearly rather than silently dropping events.
        let toml = r#"
[component]
name = "stale"

[[subscription]]
kind     = "log"
chain_id = "1"
"#;
        let manifest: Manifest = toml::from_str(toml).expect("parse");
        assert!(matches!(
            &manifest.subscriptions[0],
            Subscription::Extension { kind, .. } if kind == "log"
        ));
    }

    #[test]
    fn load_parses_extension_subscriptions_with_string_filters() {
        let toml = r#"
[component]
name = "watcher"

[[subscription]]
kind = "acme-status"

[[subscription]]
kind  = "acme-status"
scope = "primary"
"#;
        let manifest: Manifest = toml::from_str(toml).expect("parse");
        assert!(matches!(
            &manifest.subscriptions[0],
            Subscription::Extension { kind, filters } if kind == "acme-status" && filters.is_empty()
        ));
        assert!(matches!(
            &manifest.subscriptions[1],
            Subscription::Extension { kind, filters }
                if kind == "acme-status" && filters.get("scope").is_some_and(|v| v == "primary")
        ));
    }

    /// A non-string filter value on an extension kind is refused at parse.
    #[test]
    fn load_rejects_a_non_string_extension_filter() {
        let toml = r#"
[component]
name = "watcher"

[[subscription]]
kind  = "acme-status"
scope = 7
"#;
        let err = toml::from_str::<Manifest>(toml).expect_err("non-string filter");
        // Foreign toml::de::Error; pins our filter message threaded through it.
        assert!(err.to_string().contains("must be a string"), "{err}");
    }

    /// A non-core top-level section parses into the opaque extension map.
    #[test]
    fn load_parses_extension_sections_opaquely() {
        let toml = r#"
[component]
name = "keeper"

[venue]
body_version = 2

[[subscription]]
kind     = "block"
chain_id = 1
"#;
        let manifest: Manifest = toml::from_str(toml).expect("parse");
        assert_eq!(manifest.component.name, "keeper");
        assert_eq!(manifest.subscriptions.len(), 1);
        assert_eq!(manifest.extensions.len(), 1);
        let venue = manifest.extensions.get("venue").expect("venue section");
        assert_eq!(
            venue.get("body_version").and_then(toml::Value::as_integer),
            Some(2),
        );
    }

    /// A manifest without extension sections carries an empty map.
    #[test]
    fn load_defaults_to_no_extension_sections() {
        let toml = r#"
[component]
name = "plain"
"#;
        let manifest: Manifest = toml::from_str(toml).expect("parse");
        assert!(manifest.extensions.is_empty());
    }

    #[test]
    fn load_parses_cron_subscription() {
        let toml = r#"
[component]
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
[component]
name = "bad"

[dependencies]
chain = {}
not-a-real-cap = {}
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("component.toml");
        std::fs::write(&path, toml).unwrap();
        let err = load(&path, &CapabilityRegistry::core()).unwrap_err();
        assert!(
            matches!(err, ParseError::UnknownCapability { ref name, .. } if name == "not-a-real-cap")
        );
        // Operator-facing wording and order, pinned verbatim.
        assert_eq!(
            err.to_string(),
            "manifest: unknown dependency \"not-a-real-cap\" in [dependencies] (known: chain, \
             identity, local-store, remote-store, logging, http, wasi-sockets, \
             wasi-filesystem)"
        );
    }

    #[test]
    fn load_rejects_the_retired_clock_capability() {
        // `clock` is no longer a host capability (WASI clocks are ambient);
        // a manifest declaring it fails like any other unknown name.
        let toml = r#"
[component]
name = "stale"

[dependencies]
clock = {}
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("component.toml");
        std::fs::write(&path, toml).unwrap();
        let err = load(&path, &CapabilityRegistry::core()).unwrap_err();
        assert!(matches!(err, ParseError::UnknownCapability { ref name, .. } if name == "clock"));
    }

    #[test]
    fn load_parses_config_table() {
        let toml = r#"
[component]
name = "example"

[dependencies]

[config]
chain_id = 1
label    = "mainnet"
enabled  = true
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("component.toml");
        std::fs::write(&path, toml).unwrap();
        let loaded = load(&path, &CapabilityRegistry::core()).unwrap();
        let config: std::collections::HashMap<_, _> = loaded.config.into_iter().collect();
        assert_eq!(config.get("chain_id").map(String::as_str), Some("1"));
        assert_eq!(config.get("label").map(String::as_str), Some("mainnet"));
        assert_eq!(config.get("enabled").map(String::as_str), Some("true"));
    }

    #[test]
    fn component_kind_defaults_to_a_module() {
        use crate::manifest::types::ComponentKind;
        let manifest: Manifest = toml::from_str(
            r#"
[component]
name = "plain"
"#,
        )
        .expect("parse");
        assert_eq!(manifest.component.kind, ComponentKind::Module);
    }

    #[test]
    fn component_kind_reads_service() {
        use crate::manifest::types::ComponentKind;
        let manifest: Manifest = toml::from_str(
            r#"
[component]
name = "acme-provider"
kind = "service"
"#,
        )
        .expect("parse");
        assert_eq!(manifest.component.kind, ComponentKind::Service);
        // A service's name is the service type, so the name selects the row.
        assert_eq!(manifest.component.name, "acme-provider");
    }

    /// `Display` is the manifest spelling for both kinds.
    #[test]
    fn component_kind_displays_its_manifest_spelling() {
        use crate::manifest::types::ComponentKind;
        assert_eq!(ComponentKind::Module.to_string(), "module");
        assert_eq!(ComponentKind::Service.to_string(), "service");
    }

    /// The kind is a closed role now, so an invented spelling refuses at
    /// parse instead of surviving to boot as a provider name.
    #[test]
    fn component_kind_refuses_an_unknown_spelling() {
        let err = toml::from_str::<Manifest>(
            r#"
[component]
name = "bad"
kind = "gadget"
"#,
        )
        .expect_err("an unknown kind must refuse");
        assert!(err.to_string().contains("gadget"), "{err}");
    }

    #[test]
    fn resources_section_parses() {
        let toml = r#"
[component]
name = "twap"

[component.resources]
max_memory_bytes   = 10485760
max_fuel_per_event = 100000
max_state_bytes    = 52428800
"#;
        let m: Manifest = toml::from_str(toml).expect("parse");
        assert_eq!(m.component.resources.max_memory_bytes, Some(10_485_760));
        assert_eq!(m.component.resources.max_fuel_per_event, Some(100_000));
        assert_eq!(m.component.resources.max_state_bytes, Some(52_428_800));
    }

    #[test]
    fn resources_section_defaults_to_none() {
        let m: Manifest = toml::from_str("[component]\nname = \"x\"\n").expect("parse");
        assert_eq!(m.component.resources.max_memory_bytes, None);
        assert_eq!(m.component.resources.max_fuel_per_event, None);
        assert_eq!(m.component.resources.max_state_bytes, None);
    }

    #[test]
    fn load_rejects_module_name_that_escapes_the_state_dir() {
        // Name validation precedes the [dependencies] presence check.
        for bad in ["../evil", "a/b", "a\\b", "..", "/etc/passwd", "foo/../bar"] {
            // Single-quoted TOML literal string: no backslash-escape processing.
            let toml = format!("[component]\nname = '{bad}'\n");
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("component.toml");
            std::fs::write(&path, toml).unwrap();
            let err = load(&path, &CapabilityRegistry::core()).unwrap_err();
            assert!(
                matches!(err, ParseError::InvalidModuleName(ref n) if n == bad),
                "expected rejection for {bad:?}, got {err:?}",
            );
        }
    }

    #[test]
    fn load_rejects_a_blank_module_name() {
        // A missing name deserializes to the empty string, so absence,
        // emptiness, and whitespace all hit the same refusal.
        let mut manifests = vec![
            "[dependencies]\n".to_owned(),
            "[component]\n\n[dependencies]\n".to_owned(),
        ];
        // Basic strings: `\t` and `\n` reach the parser as the whitespace.
        manifests.extend(
            ["", "  ", r"\t", r"\n", r" \t \n "]
                .map(|blank| format!("[component]\nname = \"{blank}\"\n\n[dependencies]\n")),
        );
        for manifest in manifests {
            let err = load_inline(&manifest).unwrap_err();
            assert!(
                matches!(err, ParseError::BlankModuleName),
                "expected blank-name refusal for {manifest:?}, got {err:?}",
            );
        }
    }

    #[test]
    fn load_accepts_plain_module_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("component.toml");
        std::fs::write(
            &path,
            "[component]\nname = \"twap-monitor\"\n\n[dependencies]\n",
        )
        .unwrap();
        let loaded = load(&path, &CapabilityRegistry::core()).unwrap();
        assert_eq!(loaded.manifest.component.name, "twap-monitor");
    }

    #[test]
    fn load_rejects_a_missing_dependency_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("component.toml");
        std::fs::write(&path, "[component]\nname = \"bare\"\n").unwrap();
        let err = load(&path, &CapabilityRegistry::core()).unwrap_err();
        assert!(matches!(err, ParseError::MissingCapabilities), "{err:?}");
        let msg = err.to_string();
        // Operator wording pin.
        assert!(msg.contains("[dependencies]"), "{msg}");
        assert!(msg.contains("empty one grants nothing"), "{msg}");
    }

    #[test]
    fn load_refuses_a_manifest_still_carrying_optional() {
        // Silently ignoring the key would drop a declaration the author
        // believes is in effect, so the section denies unknown fields.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("component.toml");
        std::fs::write(
            &path,
            "[component]\nname = \"legacy\"\n\n[dependencies]\nlogging = {}\noptional = []\n",
        )
        .unwrap();
        let err = load(&path, &CapabilityRegistry::core()).unwrap_err();
        assert!(err.to_string().contains("optional"), "{err}");
    }

    #[test]
    fn load_accepts_an_empty_dependency_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("component.toml");
        std::fs::write(&path, "[component]\nname = \"minimal\"\n\n[dependencies]\n").unwrap();
        let loaded = load(&path, &CapabilityRegistry::core()).unwrap();
        let deps = loaded
            .manifest
            .dependencies
            .as_ref()
            .expect("dependency table parsed");
        assert!(deps.is_empty());
    }

    fn load_inline(toml: &str) -> Result<LoadedManifest, ParseError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("component.toml");
        std::fs::write(&path, toml).unwrap();
        load(&path, &CapabilityRegistry::core())
    }

    fn digest_manifest(component_line: &str) -> String {
        format!("[component]\nname = \"pinned\"\n{component_line}\n\n[dependencies]\n")
    }

    #[test]
    fn load_rejects_a_schemeless_component_digest() {
        let err = load_inline(&digest_manifest("digest = \"notahash\"")).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidComponentDigest { ref value, .. } if value == "notahash"),
            "{err:?}",
        );
    }

    #[test]
    fn load_rejects_an_explicitly_empty_component_digest() {
        let err = load_inline(&digest_manifest("digest = \"\"")).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidComponentDigest { ref value, .. } if value.is_empty()),
            "{err:?}",
        );
    }

    #[test]
    fn load_defaults_an_absent_component_digest_to_none() {
        let loaded = load_inline(&digest_manifest("")).expect("absent digest loads");
        assert!(loaded.manifest.component.digest.is_none());
        assert!(loaded.component_digest.is_none());
    }

    #[test]
    fn load_parses_a_valid_component_digest_and_round_trips() {
        let pin = format!("sha256:{}", "ab".repeat(32));
        let loaded = load_inline(&digest_manifest(&format!("digest = \"{pin}\"")))
            .expect("valid digest loads");
        let digest = loaded.component_digest.expect("digest parsed");
        assert_eq!(digest.to_string(), pin);
    }

    #[test]
    fn host_allowed_exact_and_wildcard() {
        let allow = vec!["api.acme.example".to_string(), "*.discord.com".to_string()];
        assert!(host_allowed("api.acme.example", &allow));
        assert!(!host_allowed("evil.api.acme.example", &allow));
        assert!(host_allowed("foo.discord.com", &allow));
        assert!(host_allowed("a.b.discord.com", &allow));
        assert!(!host_allowed("discord.com", &allow));
        assert!(!host_allowed("nope.example", &allow));
    }

    #[test]
    fn host_allowed_is_case_insensitive_both_ways() {
        let upper = vec!["API.ACME.EXAMPLE".to_string()];
        let lower = vec!["api.acme.example".to_string()];
        assert!(host_allowed("api.acme.example", &upper));
        assert!(host_allowed("Api.Acme.Example", &lower));
    }

    #[test]
    fn host_allowed_matches_hosts_not_authorities() {
        // Entries are bare hosts; a port or userinfo in a pattern can
        // never match a host string.
        let allow = vec![
            "api.acme.example:8443".to_string(),
            "u@api.acme.example".to_string(),
        ];
        assert!(!host_allowed("api.acme.example", &allow));
    }
}
