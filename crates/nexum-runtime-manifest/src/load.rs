//! Parse and validate `component.toml`.

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
    let loaded = LoadedManifest::try_from(manifest)?;

    // The registry cross-check lives here, not in the `TryFrom`
    // conversion, because it needs the wired registry. The `hosts`
    // placement check follows it per entry, so an unknown name refuses
    // as unknown rather than as a misplaced attribute.
    for (name, dep) in &loaded.dependencies {
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
    if !loaded.dependencies.is_empty() {
        let names: Vec<&str> = loaded.dependencies.keys().map(String::as_str).collect();
        info!(target: "manifest", dependencies = %names.join(", "), "dependencies");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Trigger;

    /// Parse and validate an inline manifest, skipping the registry
    /// cross-check `load` adds.
    fn validate(toml: &str) -> Result<LoadedManifest, ParseError> {
        let manifest: Manifest = toml::from_str(toml)?;
        manifest.try_into()
    }

    #[test]
    fn load_parses_block_and_event_triggers() {
        let toml = r#"
[component]
name = "twap-monitor"

[dependencies]
chain = {}
local-store = {}

[[trigger]]
on       = "block"
chain_id = 1

[[trigger]]
on       = "event"
chain_id = 1
address  = "0xC92E8bdf79f0507f65a392b0ab4667716BFE0110"
event_signature = "0x00000000000000000000000000000000000000000000000000000000deadbeef"
"#;
        let loaded = validate(toml).expect("parse");
        assert_eq!(loaded.name.as_str(), "twap-monitor");
        assert_eq!(loaded.triggers.len(), 2);
        assert!(matches!(
            &loaded.triggers[0],
            Trigger::Block { chain_id: 1 }
        ));
        if let Trigger::Event {
            chain_id, address, ..
        } = &loaded.triggers[1]
        {
            assert_eq!(*chain_id, 1);
            assert!(address.is_some());
        } else {
            panic!("expected Event trigger");
        }
    }

    /// `start_block` reaches the trigger, and is refused without `resume`:
    /// with no durable cursor the seed would apply on every open, turning a
    /// one-time backfill into a rescan from that height on each restart.
    #[test]
    fn load_accepts_start_block_only_alongside_resume() {
        fn event_trigger(extra: &str) -> String {
            format!(
                "[component]\nname = \"seeded\"\n\n[dependencies]\nchain = {{}}\n\n\
                 [[trigger]]\non       = \"event\"\nchain_id = 1\n{extra}\n"
            )
        }

        let loaded = validate(&event_trigger("resume = true\nstart_block = 17883049"))
            .expect("a resuming trigger carries its seed");
        assert!(
            matches!(
                &loaded.triggers[0],
                Trigger::Event {
                    resume: true,
                    start_block: Some(17_883_049),
                    ..
                }
            ),
            "{:?}",
            loaded.triggers[0],
        );

        let err = validate(&event_trigger("start_block = 17883049"))
            .expect_err("a seed without a cursor is refused");
        assert!(
            matches!(
                err,
                ParseError::StartBlockWithoutResume {
                    start_block: 17_883_049
                }
            ),
            "{err:?}",
        );
        // Operator wording pin.
        assert!(
            err.to_string().contains("requires `resume = true`"),
            "{err}"
        );

        let loaded = validate(&event_trigger("resume = true"))
            .expect("the seed stays optional under resume");
        assert!(
            matches!(
                &loaded.triggers[0],
                Trigger::Event {
                    start_block: None,
                    ..
                }
            ),
            "{:?}",
            loaded.triggers[0],
        );
    }

    /// Malformed event trigger hex refuses the manifest at load, not at
    /// first dispatch, with a typed variant carrying the value and the
    /// operator wording pinned verbatim.
    #[test]
    fn load_refuses_malformed_event_hex_at_parse() {
        fn event_trigger(field: &str) -> String {
            format!(
                "[component]\nname = \"bad\"\n\n[[trigger]]\non       = \"event\"\n\
                 chain_id = 1\n{field}\n"
            )
        }
        let err = validate(&event_trigger("address  = \"0xabc\"")).expect_err("malformed address");
        assert!(
            matches!(err, ParseError::InvalidEventAddress { ref value, .. } if value == "0xabc"),
            "{err:?}",
        );
        // Operator wording pin.
        assert!(
            err.to_string().contains("invalid event address \"0xabc\""),
            "{err}"
        );

        let err = validate(&event_trigger("event_signature = \"not-a-topic\""))
            .expect_err("malformed topic");
        assert!(
            matches!(err, ParseError::InvalidEventTopic { ref value, .. } if value == "not-a-topic"),
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
    fn load_refuses_a_core_trigger_missing_its_shape() {
        let toml = "[component]\nname = \"bad\"\n\n[[trigger]]\non = \"event\"\n";
        let err = validate(toml).expect_err("event trigger without chain_id");
        assert!(
            matches!(
                err,
                ParseError::InvalidTrigger { index: 1, ref kind, .. } if kind == "event"
            ),
            "{err:?}",
        );
    }

    /// A trigger table without an `on` cannot dispatch; the refusal
    /// carries the table's 1-based position, the only locator left once
    /// validation runs after the TOML parse.
    #[test]
    fn load_refuses_a_trigger_without_an_on() {
        let toml = "[component]\nname = \"bad\"\n\n[[trigger]]\non = \"block\"\n\
                    chain_id = 1\n\n[[trigger]]\nchain_id = 1\n";
        let err = validate(toml).expect_err("kindless trigger");
        assert!(
            matches!(err, ParseError::MissingTriggerKind { index: 2 }),
            "{err:?}"
        );
        // The position reaches the operator.
        assert!(err.to_string().contains("table 2"), "{err}");
    }

    /// Typing the field must neither widen nor narrow the accepted spelling:
    /// `0x`-prefixed or bare, any case, no checksum requirement.
    #[test]
    fn load_accepts_every_hex_spelling_of_an_event_address() {
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
                "[component]\nname = \"ok\"\n\n[dependencies]\n\n[[trigger]]\n\
                 on       = \"event\"\nchain_id = 1\naddress  = \"{spelling}\"\n"
            );
            let loaded = validate(&toml).expect(spelling);
            assert!(
                matches!(
                    &loaded.triggers[0],
                    Trigger::Event { address: Some(a), .. } if *a == expected
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
name = "alerts"

[dependencies]

[[trigger]]
on       = "block"
chain_id = 1

[[trigger]]
on       = "event"
chain_id = 1
event_signature = "0xCF5F9DE2984132265203B5C335B25727702CA77262FF622E136BAA7362BF1DA9"

[[trigger]]
on       = "event"
chain_id = 1
event_signature = "0x0000000000000000000000000000000000000000000000000000000000000001"

[[trigger]]
on       = "event"
chain_id = 100
event_signature = "cf5f9de2984132265203b5c335b25727702ca77262ff622e136baa7362bf1da9"
"#;
        let loaded_manifest = validate(toml).expect("parse");
        // Distinct, not `dedup`: the repeat is non-adjacent, as it is on chain.
        let mut loaded: Vec<alloy_primitives::B256> = Vec::new();
        for trigger in &loaded_manifest.triggers {
            if let Trigger::Event {
                event_signature: Some(topic),
                ..
            } = trigger
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
            nexum_world::manifest_event_topics(toml).expect("extract"),
            loaded,
        );

        let bad = "[component]\nname = \"bad\"\n\n[[trigger]]\non = \"event\"\n\
                   chain_id = 1\nevent_signature = \"not-a-topic\"\n";
        assert!(matches!(
            validate(bad),
            Err(ParseError::InvalidEventTopic { .. })
        ));
        assert!(nexum_world::manifest_event_topics(bad).is_err());
    }

    #[test]
    fn load_refuses_the_retired_chain_log_kind() {
        // A not-yet-migrated manifest must refuse rather than silently
        // drop deliveries. `chain-log` parses as an extension kind, so
        // its integer `chain_id` fails the string-filter rule first.
        let toml = r#"
[component]
name = "stale"

[dependencies]

[[trigger]]
on       = "chain-log"
chain_id = 1
"#;
        assert!(matches!(
            validate(toml),
            Err(ParseError::NonStringTriggerFilter { key }) if key == "chain_id"
        ));
    }

    #[test]
    fn load_parses_extension_triggers_with_string_filters() {
        let toml = r#"
[component]
name = "alerts"

[dependencies]

[[trigger]]
on = "acme-status"

[[trigger]]
on    = "acme-status"
scope = "primary"
"#;
        let loaded = validate(toml).expect("parse");
        assert!(matches!(
            &loaded.triggers[0],
            Trigger::Extension { extension_kind, filters }
                if extension_kind == "acme-status" && filters.is_empty()
        ));
        assert!(matches!(
            &loaded.triggers[1],
            Trigger::Extension { extension_kind, filters }
                if extension_kind == "acme-status"
                    && filters.get("scope").is_some_and(|v| v == "primary")
        ));
    }

    /// A non-string filter value on an extension kind is refused at load
    /// with a typed variant carrying the filter key.
    #[test]
    fn load_rejects_a_non_string_extension_filter() {
        let toml = r#"
[component]
name = "alerts"

[[trigger]]
on    = "acme-status"
scope = 7
"#;
        let err = validate(toml).expect_err("non-string filter");
        assert!(
            matches!(err, ParseError::NonStringTriggerFilter { ref key } if key == "scope"),
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

[[trigger]]
on       = "block"
chain_id = 1
"#;
        let loaded = validate(toml).expect("parse");
        assert_eq!(loaded.name.as_str(), "keeper");
        assert_eq!(loaded.triggers.len(), 1);
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
    fn load_parses_schedule_trigger() {
        let toml = r#"
[component]
name = "scheduler"

[dependencies]

[[trigger]]
on   = "schedule"
cron = "*/5 * * * *"
"#;
        let loaded = validate(toml).expect("parse");
        assert!(matches!(&loaded.triggers[0], Trigger::Schedule { .. }));
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
             local-store, logging, http, wasi-sockets, wasi-filesystem)"
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("component.toml");
        std::fs::write(
            &path,
            "[component]\nname = \"twap-monitor\"\n\n[dependencies]\n",
        )
        .unwrap();
        let loaded = load(&path, &CapabilityRegistry::core()).unwrap();
        assert_eq!(loaded.name.as_str(), "twap-monitor");
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
        // believes is in effect; the retired key reads as an unknown
        // dependency and is refused by name.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("component.toml");
        std::fs::write(
            &path,
            "[component]\nname = \"legacy\"\n\n[dependencies]\nlogging = {}\noptional = []\n",
        )
        .unwrap();
        let err = load(&path, &CapabilityRegistry::core()).unwrap_err();
        assert!(
            matches!(err, ParseError::UnknownCapability { ref name, .. } if name == "optional"),
            "{err:?}",
        );
    }

    #[test]
    fn load_accepts_an_empty_dependency_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("component.toml");
        std::fs::write(&path, "[component]\nname = \"minimal\"\n\n[dependencies]\n").unwrap();
        let loaded = load(&path, &CapabilityRegistry::core()).unwrap();
        assert!(loaded.dependencies.is_empty());
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

    /// The property ADR-0020 relied on for `kind`, now holding for the
    /// `provides` field ADR-0022 retired: a stale manifest refuses at
    /// parse rather than silently ignoring the claim.
    #[test]
    fn load_rejects_a_retired_provides_key() {
        let err = load_inline(&digest_manifest("provides = \"nexum:wallet/signer@2.0.0\""))
            .expect_err("the retired [component].provides key must refuse");
        assert!(
            matches!(&err, ParseError::Toml(e) if e.to_string().contains("unknown")),
            "{err:?}",
        );
    }
}
