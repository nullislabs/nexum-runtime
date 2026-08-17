//! Parse and validate `component.toml`.

use std::path::Path;

use tracing::info;

use super::capabilities::{CapabilityRegistry, ProvidedInterfaces};
use super::error::ParseError;
use super::types::{LoadedManifest, Manifest};

/// Parse and validate `component.toml`; no `[dependencies]` table refuses the
/// manifest (`required = []` is valid). The `[dependencies]` cross-check
/// runs later, in [`resolve_dependencies`].
pub fn load(path: &Path) -> Result<LoadedManifest, ParseError> {
    let raw = std::fs::read_to_string(path)?;
    let manifest: Manifest = toml::from_str(&raw)?;
    let loaded = LoadedManifest::try_from(manifest)?;
    if !loaded.dependencies.is_empty() {
        let names: Vec<&str> = loaded.dependencies.keys().map(String::as_str).collect();
        info!(target: "manifest", dependencies = %names.join(", "), "dependencies");
    }
    if !loaded.interfaces.is_empty() {
        let names: Vec<String> = loaded
            .interfaces
            .iter()
            .map(|(alias, track)| format!("{alias} = {track}"))
            .collect();
        info!(target: "manifest", interfaces = %names.join(", "), "interface dependencies");
    }
    if !loaded.http_allowlist.is_empty() {
        let hosts: Vec<String> = loaded
            .http_allowlist
            .iter()
            .map(ToString::to_string)
            .collect();
        info!(target: "manifest", hosts = %hosts.join(", "), "http hosts");
    }
    Ok(loaded)
}

/// Cross-check `[dependencies]` against the wired registry and the
/// provided interfaces.
///
/// This lives here rather than in the `TryFrom` conversion or in [`load`]
/// because it needs the wired registry and the full provided set: a
/// dependency may name an interface a later entry provides, so the boot
/// path calls it once every manifest is parsed and every `provides` claim
/// is known, still before any compile. A name resolves against the
/// capability table first (ADR-0016), so a provided interface can never
/// shadow a capability. Per entry the name check runs before the `hosts`
/// placement check, so an unknown name refuses as unknown rather than as
/// a misplaced attribute.
pub(crate) fn resolve_dependencies(
    loaded: &LoadedManifest,
    registry: &CapabilityRegistry,
    provided: &ProvidedInterfaces,
) -> Result<(), ParseError> {
    for (name, dep) in &loaded.dependencies {
        if !registry.is_known(name) {
            // A component's name is never a dependency; the refusal
            // spells the interface line the author meant.
            if let Some(track) = provided.track_of(name) {
                return Err(ParseError::DependencyNamesComponent {
                    dependency: name.clone(),
                    interface: track.clone(),
                });
            }
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
    for (alias, track) in &loaded.interfaces {
        if registry.is_known(alias) {
            return Err(ParseError::AliasShadowsCapability {
                dependency: alias.clone(),
                known: registry.known_names(),
            });
        }
        // The prepass refuses a second claimant of one track, so a claim
        // matching the requested track means the only provider is this
        // component itself; the provided set alone would resolve it.
        if let Some(claim) = &loaded.provides
            && claim.track() == *track
        {
            return Err(ParseError::SelfInterfaceDependency {
                dependency: alias.clone(),
                interface: track.clone(),
            });
        }
        if !provided.provides(track) {
            return Err(ParseError::InterfaceNotProvided {
                dependency: alias.clone(),
                interface: track.clone(),
                provided: provided.known_tracks(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::types::Subscription;

    /// Parse and validate an inline manifest, skipping the registry
    /// cross-check `load` adds.
    fn validate(toml: &str) -> Result<LoadedManifest, ParseError> {
        let manifest: Manifest = toml::from_str(toml)?;
        manifest.try_into()
    }

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
        let loaded = validate(toml).expect("parse");
        assert_eq!(loaded.name.as_str(), "twap-monitor");
        assert_eq!(loaded.subscriptions.len(), 2);
        assert!(matches!(
            &loaded.subscriptions[0],
            Subscription::Block { chain_id: 1 }
        ));
        if let Subscription::ChainLog {
            chain_id, address, ..
        } = &loaded.subscriptions[1]
        {
            assert_eq!(*chain_id, 1);
            assert!(address.is_some());
        } else {
            panic!("expected ChainLog subscription");
        }
    }

    /// Malformed chain-log hex refuses the manifest at load, not at first
    /// dispatch, with a typed variant carrying the value and the operator
    /// wording pinned verbatim.
    #[test]
    fn load_refuses_malformed_chain_log_hex_at_parse() {
        fn chain_log(field: &str) -> String {
            format!(
                "[component]\nname = \"bad\"\n\n[[subscription]]\nkind     = \"chain-log\"\n\
                 chain_id = 1\n{field}\n"
            )
        }
        let err = validate(&chain_log("address  = \"0xabc\"")).expect_err("malformed address");
        assert!(
            matches!(err, ParseError::InvalidChainLogAddress { ref value, .. } if value == "0xabc"),
            "{err:?}",
        );
        // Operator wording pin.
        assert!(
            err.to_string()
                .contains("invalid chain-log address \"0xabc\""),
            "{err}"
        );

        let err =
            validate(&chain_log("event_signature = \"not-a-topic\"")).expect_err("malformed topic");
        assert!(
            matches!(err, ParseError::InvalidChainLogTopic { ref value, .. } if value == "not-a-topic"),
            "{err:?}",
        );
        // Operator wording pin.
        assert!(
            err.to_string().contains("invalid topic \"not-a-topic\""),
            "{err}"
        );
    }

    /// A core-kind table whose shape does not match its kind carries the
    /// declared kind and the table's position in the refusal.
    #[test]
    fn load_refuses_a_core_subscription_missing_its_shape() {
        let toml = "[component]\nname = \"bad\"\n\n[[subscription]]\nkind = \"chain-log\"\n";
        let err = validate(toml).expect_err("chain-log without chain_id");
        assert!(
            matches!(
                err,
                ParseError::InvalidSubscription { index: 1, ref kind, .. } if kind == "chain-log"
            ),
            "{err:?}",
        );
    }

    /// A subscription table without a `kind` cannot dispatch; the refusal
    /// carries the table's 1-based position, the only locator left once
    /// validation runs after the TOML parse.
    #[test]
    fn load_refuses_a_subscription_without_a_kind() {
        let toml = "[component]\nname = \"bad\"\n\n[[subscription]]\nkind = \"block\"\n\
                    chain_id = 1\n\n[[subscription]]\nchain_id = 1\n";
        let err = validate(toml).expect_err("kindless subscription");
        assert!(
            matches!(err, ParseError::MissingSubscriptionKind { index: 2 }),
            "{err:?}"
        );
        // The position reaches the operator.
        assert!(err.to_string().contains("table 2"), "{err}");
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
                "[component]\nname = \"ok\"\n\n[dependencies]\n\n[[subscription]]\n\
                 kind     = \"chain-log\"\nchain_id = 1\naddress  = \"{spelling}\"\n"
            );
            let loaded = validate(&toml).expect(spelling);
            assert!(
                matches!(
                    &loaded.subscriptions[0],
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

[dependencies]

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
        let loaded_manifest = validate(toml).expect("parse");
        // Distinct, not `dedup`: the repeat is non-adjacent, as it is on chain.
        let mut loaded: Vec<alloy_primitives::B256> = Vec::new();
        for sub in &loaded_manifest.subscriptions {
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
        assert!(matches!(
            validate(bad),
            Err(ParseError::InvalidChainLogTopic { .. })
        ));
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

[dependencies]

[[subscription]]
kind     = "log"
chain_id = "1"
"#;
        let loaded = validate(toml).expect("parse");
        assert!(matches!(
            &loaded.subscriptions[0],
            Subscription::Extension { kind, .. } if kind == "log"
        ));
    }

    #[test]
    fn load_parses_extension_subscriptions_with_string_filters() {
        let toml = r#"
[component]
name = "watcher"

[dependencies]

[[subscription]]
kind = "acme-status"

[[subscription]]
kind  = "acme-status"
scope = "primary"
"#;
        let loaded = validate(toml).expect("parse");
        assert!(matches!(
            &loaded.subscriptions[0],
            Subscription::Extension { kind, filters } if kind == "acme-status" && filters.is_empty()
        ));
        assert!(matches!(
            &loaded.subscriptions[1],
            Subscription::Extension { kind, filters }
                if kind == "acme-status" && filters.get("scope").is_some_and(|v| v == "primary")
        ));
    }

    /// A non-string filter value on an extension kind is refused at load
    /// with a typed variant carrying the filter key.
    #[test]
    fn load_rejects_a_non_string_extension_filter() {
        let toml = r#"
[component]
name = "watcher"

[[subscription]]
kind  = "acme-status"
scope = 7
"#;
        let err = validate(toml).expect_err("non-string filter");
        assert!(
            matches!(err, ParseError::NonStringSubscriptionFilter { ref key } if key == "scope"),
            "{err:?}",
        );
        // Operator wording pin.
        assert!(err.to_string().contains("must be a string"), "{err}");
    }

    /// A non-core top-level section parses into the opaque extension map.
    #[test]
    fn load_parses_extension_sections_opaquely() {
        let toml = r#"
[component]
name = "keeper"

[dependencies]

[venue]
body_version = 2

[[subscription]]
kind     = "block"
chain_id = 1
"#;
        let loaded = validate(toml).expect("parse");
        assert_eq!(loaded.name.as_str(), "keeper");
        assert_eq!(loaded.subscriptions.len(), 1);
        assert_eq!(loaded.extensions.len(), 1);
        let venue = loaded.extensions.get("venue").expect("venue section");
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

[dependencies]
"#;
        let loaded = validate(toml).expect("parse");
        assert!(loaded.extensions.is_empty());
    }

    #[test]
    fn load_parses_cron_subscription() {
        let toml = r#"
[component]
name = "scheduler"

[dependencies]

[[subscription]]
kind     = "cron"
schedule = "*/5 * * * *"
"#;
        let loaded = validate(toml).expect("parse");
        assert!(matches!(
            &loaded.subscriptions[0],
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
        let err = load_inline(toml).unwrap_err();
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

    /// `hosts` on a known dependency other than `http` refuses with the
    /// misplaced-attribute variant and its pinned wording.
    #[test]
    fn load_refuses_hosts_on_a_dependency_that_does_not_take_it() {
        let toml = r#"
[component]
name = "bad"

[dependencies]
chain = { hosts = ["api.acme.example"] }
"#;
        let err = load_inline(toml).unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::MisplacedDependencyAttribute { ref dependency, attribute: "hosts" }
                    if dependency == "chain"
            ),
            "{err:?}",
        );
        // Operator wording pin.
        assert_eq!(
            err.to_string(),
            "manifest: [dependencies].chain does not take `hosts`",
        );
    }

    /// An unknown dependency refuses as unknown even when it also carries
    /// `hosts`; the name check precedes the placement check per entry.
    #[test]
    fn an_unknown_dependency_with_hosts_refuses_as_unknown() {
        let toml = r#"
[component]
name = "bad"

[dependencies]
not-a-real-cap = { hosts = ["api.acme.example"] }
"#;
        let err = load_inline(toml).unwrap_err();
        assert!(
            matches!(err, ParseError::UnknownCapability { ref name, .. } if name == "not-a-real-cap"),
            "{err:?}",
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
        let err = load_inline(toml).unwrap_err();
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
        let loaded = load_inline(toml).unwrap();
        let config: std::collections::HashMap<_, _> = loaded.config.into_iter().collect();
        assert_eq!(config.get("chain_id").map(String::as_str), Some("1"));
        assert_eq!(config.get("label").map(String::as_str), Some("mainnet"));
        assert_eq!(config.get("enabled").map(String::as_str), Some("true"));
    }

    /// ADR-0020 retired `[component].kind`; `deny_unknown_fields` on
    /// `ComponentSection` refuses it rather than parse-and-ignore.
    #[test]
    fn component_section_refuses_the_retired_kind_field() {
        let err = validate(
            r#"
[component]
name = "stale"
kind = "module"

[dependencies]
"#,
        )
        .expect_err("the retired kind field must refuse");
        assert!(matches!(err, ParseError::Toml(_)), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field") && msg.contains("`kind`"),
            "{msg}",
        );
    }

    #[test]
    fn resources_section_parses() {
        let toml = r#"
[component]
name = "twap"

[dependencies]

[component.resources]
max_memory_bytes      = 10485760
max_fuel_per_dispatch = 100000
max_state_bytes       = 52428800
"#;
        let loaded = validate(toml).expect("parse");
        assert_eq!(loaded.resources.max_memory_bytes, Some(10_485_760));
        assert_eq!(loaded.resources.max_fuel_per_dispatch, Some(100_000));
        assert_eq!(loaded.resources.max_state_bytes, Some(52_428_800));
    }

    #[test]
    fn resources_section_defaults_to_none() {
        let loaded = validate("[component]\nname = \"x\"\n\n[dependencies]\n").expect("parse");
        assert_eq!(loaded.resources.max_memory_bytes, None);
        assert_eq!(loaded.resources.max_fuel_per_dispatch, None);
        assert_eq!(loaded.resources.max_state_bytes, None);
    }

    #[test]
    fn resources_section_refuses_an_unknown_key() {
        let toml = r#"
[component]
name = "twap"

[dependencies]

[component.resources]
max_fuel = 100000
"#;
        let err = validate(toml).expect_err("an unknown resources key must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field") && msg.contains("`max_fuel`"),
            "{msg}",
        );
    }

    #[test]
    fn component_section_refuses_a_misspelled_resources_table() {
        let toml = r#"
[component]
name = "twap"

[dependencies]

[component.resource]
max_fuel_per_dispatch = 100000
"#;
        let err = validate(toml).expect_err("a misspelled resources table must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field") && msg.contains("`resource`"),
            "{msg}",
        );
    }

    #[test]
    fn load_rejects_module_name_that_escapes_the_state_dir() {
        // Name validation precedes the [dependencies] presence check.
        for bad in ["../evil", "a/b", "a\\b", "..", "/etc/passwd", "foo/../bar"] {
            // Single-quoted TOML literal string: no backslash-escape processing.
            let toml = format!("[component]\nname = '{bad}'\n");
            let err = load_inline(&toml).unwrap_err();
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
    fn load_rejects_an_untrimmed_module_name() {
        // Basic strings: `\t` and `\n` reach the parser as the whitespace.
        for (written, parsed) in [
            (r"cow ", "cow "),
            (r" cow", " cow"),
            (r" cow ", " cow "),
            (r"cow\t", "cow\t"),
            (r"\ncow", "\ncow"),
        ] {
            let manifest = format!("[component]\nname = \"{written}\"\n\n[dependencies]\n");
            let err = load_inline(&manifest).unwrap_err();
            assert!(
                matches!(err, ParseError::UntrimmedModuleName(ref n) if n == parsed),
                "expected untrimmed-name refusal for {written:?}, got {err:?}",
            );
        }
        let err = load_inline("[component]\nname = 'cow '\n\n[dependencies]\n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "manifest: [component].name \"cow \" must not have leading or trailing whitespace",
        );
    }

    #[test]
    fn load_accepts_plain_module_name() {
        let loaded =
            load_inline("[component]\nname = \"twap-monitor\"\n\n[dependencies]\n").unwrap();
        assert_eq!(loaded.name.as_str(), "twap-monitor");
    }

    #[test]
    fn load_rejects_a_missing_dependency_table() {
        let err = load_inline("[component]\nname = \"bare\"\n").unwrap_err();
        assert!(matches!(err, ParseError::MissingCapabilities), "{err:?}");
        let msg = err.to_string();
        // Operator wording pin.
        assert!(msg.contains("[dependencies]"), "{msg}");
        assert!(msg.contains("empty one grants nothing"), "{msg}");
    }

    #[test]
    fn load_refuses_a_manifest_still_carrying_optional() {
        // Silently ignoring the key would drop a declaration the author
        // believes is in effect; the retired key reads as an unknown
        // dependency and is refused by name.
        let err = load_inline(
            "[component]\nname = \"legacy\"\n\n[dependencies]\nlogging = {}\noptional = []\n",
        )
        .unwrap_err();
        assert!(
            matches!(err, ParseError::UnknownCapability { ref name, .. } if name == "optional"),
            "{err:?}",
        );
    }

    #[test]
    fn load_accepts_an_empty_dependency_table() {
        let loaded = load_inline("[component]\nname = \"minimal\"\n\n[dependencies]\n").unwrap();
        assert!(loaded.dependencies.is_empty());
    }

    /// The whole load path a boot takes: parse, then resolve against the
    /// core registry and the given provided set.
    fn load_resolved(
        toml: &str,
        provided: &ProvidedInterfaces,
    ) -> Result<LoadedManifest, ParseError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("component.toml");
        std::fs::write(&path, toml).unwrap();
        let loaded = load(&path)?;
        resolve_dependencies(&loaded, &CapabilityRegistry::core(), provided)?;
        Ok(loaded)
    }

    fn load_inline(toml: &str) -> Result<LoadedManifest, ParseError> {
        load_resolved(toml, &ProvidedInterfaces::default())
    }

    /// A provided set with one claim, keyed as the prepass ledger keys it.
    fn provided_by(component: &str, track: &str) -> ProvidedInterfaces {
        let mut provided = ProvidedInterfaces::default();
        provided.insert(
            crate::interface_id::InterfaceTrack::parse(track).expect("valid track"),
            component,
        );
        provided
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
    fn load_parses_a_valid_provides_claim_and_round_trips() {
        let loaded = load_inline(&digest_manifest("provides = \"nexum:wallet/signer@2.0.0\""))
            .expect("valid provides loads");
        let claim = loaded.provides.expect("provides parsed");
        assert_eq!(claim.to_string(), "nexum:wallet/signer@2.0.0");
    }

    #[test]
    fn load_defaults_an_absent_provides_to_none() {
        let loaded = load_inline(&digest_manifest("")).expect("absent provides loads");
        assert!(loaded.provides.is_none());
    }

    #[test]
    fn load_rejects_a_malformed_provides_claim() {
        for (label, bad) in [
            ("truncated version", "nexum:wallet/signer@2.0"),
            ("bare major", "nexum:wallet/signer@2"),
            ("no version", "nexum:wallet/signer"),
            ("no package", "signer@2.0.0"),
            ("no namespace", "wallet/signer@2.0.0"),
            ("no interface", "nexum:wallet@2.0.0"),
            ("empty", ""),
        ] {
            let err =
                load_inline(&digest_manifest(&format!("provides = \"{bad}\""))).expect_err(label);
            assert!(
                matches!(err, ParseError::InvalidInterfaceId { ref value, .. } if value == bad),
                "{label}: {err:?}",
            );
        }
    }

    /// The property ADR-0020 relied on for `kind`: a stale engine reading
    /// a `provides` manifest refuses at parse rather than silently
    /// ignoring the claim; here its neighbour, a misspelt `provides`.
    #[test]
    fn load_rejects_a_misspelt_provides_key() {
        let err = load_inline(&digest_manifest("provdes = \"nexum:wallet/signer@2.0.0\""))
            .expect_err("an unknown [component] key must refuse");
        assert!(
            matches!(&err, ParseError::Toml(e) if e.to_string().contains("unknown")),
            "{err:?}",
        );
    }

    fn consumer_manifest(dependency_line: &str) -> String {
        format!("[component]\nname = \"consumer\"\n\n[dependencies]\n{dependency_line}\n")
    }

    #[test]
    fn a_dependency_on_a_provided_track_resolves() {
        let loaded = load_resolved(
            &consumer_manifest("wallet = { interface = \"acme:pool/quoter@2\" }"),
            &provided_by("quoter-svc", "acme:pool/quoter@2"),
        )
        .expect("a provided track resolves");
        assert_eq!(
            loaded
                .interfaces
                .get("wallet")
                .expect("alias parsed")
                .as_str(),
            "acme:pool/quoter@2",
        );
        assert!(
            !loaded.dependencies.contains_key("wallet"),
            "an interface dependency is not a capability dependency",
        );
    }

    /// The value is a track, not a full version: a consumer asks for
    /// compatibility, and the provider's exact version is the provider's
    /// business.
    #[test]
    fn an_interface_value_that_is_not_a_track_refuses_at_parse() {
        for bad in [
            "acme:pool/quoter@2.0.0", // full version
            "acme:pool/quoter@2.0",
            "acme:pool/quoter",
            "quoter-svc", // a component name
            "",
        ] {
            let toml = consumer_manifest(&format!("wallet = {{ interface = \"{bad}\" }}"));
            let err = validate(&toml).expect_err(bad);
            assert!(
                matches!(&err, ParseError::InvalidInterfaceTrack { dependency, .. }
                    if dependency == "wallet"),
                "{bad}: {err:?}",
            );
        }
        // Operator wording pin.
        let err = validate(&consumer_manifest(
            "wallet = { interface = \"acme:pool/quoter@2.0.0\" }",
        ))
        .expect_err("a full version is not a track");
        assert_eq!(
            err.to_string(),
            "manifest: [dependencies].wallet: \"acme:pool/quoter@2.0.0\" is not an \
             interface track (name@major, name@0.minor below 1.0, or name@0.0.patch \
             below that)",
        );
    }

    #[test]
    fn hosts_on_an_interface_dependency_refuses_as_misplaced() {
        let toml = consumer_manifest(
            "wallet = { interface = \"acme:pool/quoter@2\", hosts = [\"api.acme.example\"] }",
        );
        let err = validate(&toml).expect_err("hosts on an interface dependency");
        assert!(
            matches!(&err, ParseError::MisplacedDependencyAttribute { dependency, attribute: "hosts" }
                if dependency == "wallet"),
            "{err:?}",
        );
    }

    /// The one alias the misplaced-hosts wording would lie about: `http`
    /// is the dependency that does take `hosts`, so the parse-time check
    /// skips it and the refusal names the real defect, the
    /// capability-named alias.
    #[test]
    fn an_http_alias_with_hosts_refuses_as_a_shadowing_alias() {
        let toml = consumer_manifest(
            "http = { interface = \"acme:pool/quoter@2\", hosts = [\"api.acme.example\"] }",
        );
        let err = load_resolved(&toml, &provided_by("quoter-svc", "acme:pool/quoter@2"))
            .expect_err("a capability-named alias must refuse as shadowing");
        assert!(
            matches!(&err, ParseError::AliasShadowsCapability { dependency, .. }
                if dependency == "http"),
            "{err:?}",
        );
    }

    /// A component's own claim never satisfies its own dependency: the
    /// prepass refuses a second claimant of the track, so resolving it
    /// against the provided set alone would accept a self-loop.
    #[test]
    fn a_dependency_on_the_component_own_interface_refuses() {
        let toml = "[component]\nname = \"quoter-svc\"\n\
                    provides = \"acme:pool/quoter@2.0.0\"\n\n[dependencies]\n\
                    self = { interface = \"acme:pool/quoter@2\" }\n";
        let err = load_resolved(toml, &provided_by("quoter-svc", "acme:pool/quoter@2"))
            .expect_err("a self-dependency must refuse");
        assert!(
            matches!(&err, ParseError::SelfInterfaceDependency { dependency, interface }
                if dependency == "self" && interface.as_str() == "acme:pool/quoter@2"),
            "{err:?}",
        );
        // Operator wording pin.
        assert_eq!(
            err.to_string(),
            "manifest: [dependencies].self depends on interface acme:pool/quoter@2, \
             which this component itself provides",
        );
    }

    /// Scope rule: the capability table resolves first, so an alias equal
    /// to any known capability name refuses rather than shadowing it.
    #[test]
    fn an_alias_that_is_a_capability_name_refuses() {
        for taken in ["chain", "http", "wasi-sockets"] {
            let toml = consumer_manifest(&format!(
                "{taken} = {{ interface = \"acme:pool/quoter@2\" }}"
            ));
            let err = load_resolved(&toml, &provided_by("quoter-svc", "acme:pool/quoter@2"))
                .expect_err(taken);
            assert!(
                matches!(&err, ParseError::AliasShadowsCapability { dependency, .. }
                    if dependency == taken),
                "{taken}: {err:?}",
            );
        }
        // Operator wording pin.
        let err = load_resolved(
            &consumer_manifest("chain = { interface = \"acme:pool/quoter@2\" }"),
            &provided_by("quoter-svc", "acme:pool/quoter@2"),
        )
        .expect_err("a shadowing alias must refuse");
        assert_eq!(
            err.to_string(),
            "manifest: [dependencies].chain declares an interface under a capability's \
             name; rename the alias (capabilities: chain, identity, local-store, \
             remote-store, logging, http, wasi-sockets, wasi-filesystem)",
        );
    }

    /// A bareword dependency naming a provider component refuses with the
    /// corrected interface line, not as a bare unknown name.
    #[test]
    fn a_dependency_naming_a_provider_component_spells_the_corrected_line() {
        let err = load_resolved(
            &consumer_manifest("quoter-svc = {}"),
            &provided_by("quoter-svc", "acme:pool/quoter@2"),
        )
        .expect_err("a component name is not a dependency");
        assert!(
            matches!(&err, ParseError::DependencyNamesComponent { dependency, interface }
                if dependency == "quoter-svc" && interface.as_str() == "acme:pool/quoter@2"),
            "{err:?}",
        );
        // Operator wording pin, corrected line included.
        assert_eq!(
            err.to_string(),
            "manifest: [dependencies].quoter-svc names a component, not an interface; \
             that component provides acme:pool/quoter@2, so write \
             quoter-svc = { interface = \"acme:pool/quoter@2\" }",
        );
    }

    /// A bareword naming a non-providing component stays an unknown
    /// dependency; there is no interface line to correct it to.
    #[test]
    fn a_dependency_naming_a_component_without_a_claim_refuses_as_unknown() {
        let err = load_resolved(
            &consumer_manifest("quoter-svc = {}"),
            &provided_by("other-svc", "acme:pool/quoter@2"),
        )
        .expect_err("an unknown name refuses");
        assert!(
            matches!(&err, ParseError::UnknownCapability { name, .. } if name == "quoter-svc"),
            "{err:?}",
        );
    }

    /// Resolution order is a decision: the core capability table first,
    /// then the provided interfaces. A component named like a capability
    /// never captures the capability's bareword.
    #[test]
    fn a_bareword_that_is_both_capability_and_component_resolves_as_the_capability() {
        load_resolved(
            &consumer_manifest("chain = {}"),
            &provided_by("chain", "acme:pool/quoter@2"),
        )
        .expect("the capability wins over the same-named component");
    }

    #[test]
    fn a_track_no_loaded_component_provides_refuses_at_resolution() {
        let err = load_resolved(
            &consumer_manifest("wallet = { interface = \"acme:pool/quoter@2\" }"),
            &ProvidedInterfaces::default(),
        )
        .expect_err("no provider, no boot");
        assert!(
            matches!(&err, ParseError::InterfaceNotProvided { dependency, interface, provided }
                if dependency == "wallet"
                    && interface.as_str() == "acme:pool/quoter@2"
                    && provided == "none"),
            "{err:?}",
        );
        // Operator wording pin, the empty set reading as `none`.
        assert_eq!(
            err.to_string(),
            "manifest: [dependencies].wallet depends on interface acme:pool/quoter@2 \
             but no loaded component provides it (provided: none)",
        );

        // A populated set is listed for the fix.
        let err = load_resolved(
            &consumer_manifest("wallet = { interface = \"acme:pool/quoter@2\" }"),
            &provided_by("signer-svc", "nexum:wallet/signer@1"),
        )
        .expect_err("the wrong track still refuses");
        assert!(
            matches!(&err, ParseError::InterfaceNotProvided { provided, .. }
                if provided == "nexum:wallet/signer@1"),
            "{err:?}",
        );
    }
}
