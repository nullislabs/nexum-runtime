//! Interface identity: the `[component].provides` claim and the
//! `[implements]` key.

use derive_more::Display;
use thiserror::Error;

/// Why a string cannot become an [`InterfaceId`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum InvalidInterfaceId {
    /// The WIT parser rejects a truncated version, so the grammar
    /// requires the full one.
    #[error("expected namespace:package/interface@major.minor.patch")]
    MalformedName,
    /// Not a full semver.
    #[error("the version is not a full semver: {0}")]
    Version(#[from] semver::Error),
}

/// A full interface id, `namespace:package/interface@major.minor.patch`.
///
/// [`InterfaceId::parse`] is the only constructor, as with `ModuleId`.
#[derive(Clone, Debug, Display, Eq, PartialEq)]
#[display("{name}@{version}")]
pub struct InterfaceId {
    /// Version-stripped interface name, `namespace:package/interface`.
    name: String,
    version: semver::Version,
}

impl InterfaceId {
    /// Validate a full interface id.
    pub fn parse(value: &str) -> Result<Self, InvalidInterfaceId> {
        let Some((name, version)) = value.split_once('@') else {
            return Err(InvalidInterfaceId::MalformedName);
        };
        validate_name(name)?;
        let version = semver::Version::parse(version)?;
        Ok(Self {
            name: name.to_owned(),
            version,
        })
    }

    /// True when `export` names the same interface at a version that
    /// satisfies the claim: the same [track](Self::track) and no older
    /// than the claimed version. An unversioned export never satisfies a
    /// claim, which always carries a full version.
    pub fn matches_export(&self, export: &str) -> bool {
        let Some((name, version)) = export.split_once('@') else {
            return false;
        };
        if name != self.name {
            return false;
        }
        let Ok(version) = semver::Version::parse(version) else {
            return false;
        };
        track_suffix(&version) == track_suffix(&self.version) && version >= self.version
    }

    /// The compatibility track this id belongs to. `[implements]` and the
    /// prepass duplicate-claim ledger both key on it, through this one
    /// derivation, so the two sites cannot drift.
    pub fn track(&self) -> InterfaceTrack {
        InterfaceTrack(format!("{}@{}", self.name, track_suffix(&self.version)))
    }
}

/// Semver's compatibility rule, which is leading-zero sensitive: the
/// major at or above 1.0, `0.minor` under it, and `0.0.patch` under that,
/// because every `0.0.z` release is its own breaking interface.
fn track_suffix(version: &semver::Version) -> String {
    match (version.major, version.minor) {
        (0, 0) => format!("0.0.{}", version.patch),
        (0, minor) => format!("0.{minor}"),
        (major, _) => major.to_string(),
    }
}

/// `name` must be `namespace:package/interface` in kebab-case words.
fn validate_name(name: &str) -> Result<(), InvalidInterfaceId> {
    let Some((package, interface)) = name.split_once('/') else {
        return Err(InvalidInterfaceId::MalformedName);
    };
    let package_ok = package
        .split_once(':')
        .is_some_and(|(namespace, pkg)| is_kebab_word(namespace) && is_kebab_word(pkg));
    if !package_ok || !is_kebab_word(interface) {
        return Err(InvalidInterfaceId::MalformedName);
    }
    Ok(())
}

fn is_kebab_word(word: &str) -> bool {
    !word.is_empty()
        && !word.starts_with('-')
        && !word.ends_with('-')
        && word
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The key is not a track: full semver, leading zeros, and a bare `@0`
/// are all refused, because a loosely spelt authorization row silently
/// binds nothing.
#[derive(Debug, Error)]
#[error(
    "{0:?} is not an interface track \
     (name@major, name@0.minor below 1.0, or name@0.0.patch below that)"
)]
pub struct InvalidInterfaceTrack(pub String);

/// An interface's compatibility track, e.g. `nexum:wallet/signer@2`.
/// A compatible provider release stays inside its track, so an
/// `[implements]` row keyed on one survives it; the digest pins the exact
/// artifact. Under 0.1 nothing is compatible, so the track is the whole
/// version and every release is an operator edit.
#[derive(Clone, Debug, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InterfaceTrack(String);

impl InterfaceTrack {
    /// Validate an `[implements]` key; the grammar is strict, as a pin's is.
    pub fn parse(key: &str) -> Result<Self, InvalidInterfaceTrack> {
        let refuse = || InvalidInterfaceTrack(key.to_owned());
        let Some((name, suffix)) = key.split_once('@') else {
            return Err(refuse());
        };
        if validate_name(name).is_err() {
            return Err(refuse());
        }
        let canonical = if let Some(patch) = suffix.strip_prefix("0.0.") {
            canonical_number(patch).map(|p| format!("0.0.{p}"))
        } else if let Some(minor) = suffix.strip_prefix("0.") {
            canonical_number(minor)
                .filter(|&minor| minor >= 1)
                .map(|m| format!("0.{m}"))
        } else {
            canonical_number(suffix)
                .filter(|&major| major >= 1)
                .map(|major| major.to_string())
        };
        match canonical {
            Some(rendered) if rendered == suffix => Ok(Self(key.to_owned())),
            _ => Err(refuse()),
        }
    }

    /// The track as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Digits only, no sign, no leading zero.
fn canonical_number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0'))
    {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> InterfaceId {
        InterfaceId::parse(value).expect("valid interface id")
    }

    #[test]
    fn parse_round_trips_via_display() {
        for value in [
            "nexum:wallet/signer@2.0.0",
            "acme:venues/registry@0.3.1",
            "a:b/c@1.0.0-rc.1",
        ] {
            assert_eq!(id(value).to_string(), value);
        }
    }

    #[test]
    fn parse_refuses_a_malformed_id() {
        for bad in [
            "",
            "nexum:wallet/signer",     // no version
            "nexum:wallet/signer@2",   // truncated version
            "nexum:wallet/signer@2.0", // truncated version
            "signer@2.0.0",            // no package
            "wallet/signer@2.0.0",     // no namespace
            "nexum:wallet@2.0.0",      // no interface
            "Nexum:wallet/signer@2.0.0",
            "nexum:wallet/signer @2.0.0",
        ] {
            assert!(
                InterfaceId::parse(bad).is_err(),
                "expected refusal for {bad:?}",
            );
        }
    }

    #[test]
    fn the_track_is_semvers_compatibility_range() {
        assert_eq!(
            id("nexum:wallet/signer@2.1.3").track().as_str(),
            "nexum:wallet/signer@2",
        );
        assert_eq!(id("a:b/c@0.3.9").track().as_str(), "a:b/c@0.3");
        // Under 0.1 every patch is its own breaking interface.
        assert_eq!(id("a:b/c@0.0.1").track().as_str(), "a:b/c@0.0.1");
    }

    #[test]
    fn two_versions_on_one_track_share_the_track() {
        assert_eq!(id("a:b/c@2.0.0").track(), id("a:b/c@2.9.9").track());
        assert_ne!(id("a:b/c@1.0.0").track(), id("a:b/c@2.0.0").track());
        assert_ne!(id("a:b/c@0.1.0").track(), id("a:b/c@0.2.0").track());
        assert_ne!(id("a:b/c@0.0.1").track(), id("a:b/c@0.0.9").track());
    }

    #[test]
    fn an_export_satisfies_the_claim_only_in_track_and_no_older() {
        let claim = id("nexum:wallet/signer@2.1.0");
        assert!(claim.matches_export("nexum:wallet/signer@2.1.0"));
        assert!(claim.matches_export("nexum:wallet/signer@2.2.0"));
        // An older in-track export does not honour the claimed surface.
        assert!(!claim.matches_export("nexum:wallet/signer@2.0.0"));
        // The wrong track is the fail-open the operator cannot see.
        assert!(!claim.matches_export("nexum:wallet/signer@1.0.0"));
        assert!(!claim.matches_export("nexum:wallet/signer@3.0.0"));
        assert!(!claim.matches_export("nexum:wallet/signer"));
        assert!(!claim.matches_export("nexum:wallet/other@2.1.0"));
        assert!(!claim.matches_export("init"));
    }

    /// Cargo reads `^0.0.1` as `=0.0.1`, so a newer patch under 0.1 is a
    /// different interface, not a compatible one.
    #[test]
    fn a_newer_patch_under_zero_one_does_not_satisfy_the_claim() {
        let claim = id("acme:pool/quoter@0.0.1");
        assert!(claim.matches_export("acme:pool/quoter@0.0.1"));
        assert!(!claim.matches_export("acme:pool/quoter@0.0.9"));
        assert!(!claim.matches_export("acme:pool/quoter@0.1.0"));
    }

    #[test]
    fn track_parse_accepts_only_the_canonical_spelling() {
        for good in ["nexum:wallet/signer@2", "a:b/c@0.3", "a:b/c@0.0.7"] {
            assert_eq!(
                InterfaceTrack::parse(good).expect("valid track").as_str(),
                good,
            );
        }
        for bad in [
            "nexum:wallet/signer@2.0.0",
            "nexum:wallet/signer@2.0",
            "nexum:wallet/signer@0",
            "nexum:wallet/signer@02",
            "nexum:wallet/signer@0.03",
            // The 0.0 band keys on the patch, so the band alone binds
            // nothing and a fourth field is past the grammar.
            "a:b/c@0.0",
            "a:b/c@0.0.07",
            "a:b/c@0.0.1.2",
            "nexum:wallet/signer",
            "signer@2",
            "",
        ] {
            assert!(
                InterfaceTrack::parse(bad).is_err(),
                "expected refusal for {bad:?}",
            );
        }
    }
}
