//! Content digests for loaded component artifacts.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use sha2::{Digest, Sha256};
use thiserror::Error;

const SCHEME: &str = "sha256";

/// sha256 digest of an artifact's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentDigest([u8; 32]);

impl ContentDigest {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

impl FromStr for ContentDigest {
    type Err = DigestParseError;

    /// Strict `sha256:<64 hex chars>` grammar; anything else is a hard error.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((scheme, payload)) = s.split_once(':') else {
            return Err(DigestParseError::MissingScheme(s.to_owned()));
        };
        if scheme != SCHEME {
            return Err(DigestParseError::UnsupportedScheme {
                scheme: scheme.to_owned(),
            });
        }
        // const-hex would tolerate a `0x` prefix; the strict grammar must not.
        if payload.starts_with("0x") || payload.starts_with("0X") {
            return Err(DigestParseError::Hex {
                value: s.to_owned(),
                source: alloy_primitives::hex::FromHexError::InvalidHexCharacter {
                    c: 'x',
                    index: 1,
                },
            });
        }
        let digest =
            alloy_primitives::hex::decode_to_array::<_, 32>(payload).map_err(|source| {
                DigestParseError::Hex {
                    value: s.to_owned(),
                    source,
                }
            })?;
        if digest == [0u8; 32] {
            return Err(DigestParseError::Uncommitted);
        }
        Ok(Self(digest))
    }
}

impl fmt::Display for ContentDigest {
    /// Canonical lowercase `sha256:<hex>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{SCHEME}:{}", alloy_primitives::hex::encode(self.0))
    }
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DigestParseError {
    /// No `scheme:` prefix; the empty string lands here too.
    #[error("digest {0:?} has no scheme prefix; expected sha256:<64 hex chars>")]
    MissingScheme(String),
    #[error("unsupported digest scheme {scheme:?}; only sha256 is supported")]
    UnsupportedScheme { scheme: String },
    #[error("digest {value:?} has a malformed sha256 payload: {source}")]
    Hex {
        value: String,
        #[source]
        source: alloy_primitives::hex::FromHexError,
    },
    /// All-zero digest: the uncommitted sentinel, never a real pin.
    #[error("digest is the all-zero uncommitted sentinel; omit `component` instead")]
    Uncommitted,
}

/// A loaded artifact hashing differently from its manifest's pin.
#[derive(Debug, Error)]
#[error(
    "component digest mismatch for {}: manifest declares {declared}, \
     artifact hashes to {actual}",
    path.display()
)]
pub struct DigestMismatch {
    pub path: PathBuf,
    pub declared: ContentDigest,
    pub actual: ContentDigest,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The NIST sha256 test vector for "abc".
    const ABC_DIGEST: &str =
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn of_bytes_matches_the_known_test_vector() {
        assert_eq!(ContentDigest::of_bytes(b"abc").to_string(), ABC_DIGEST);
    }

    #[test]
    fn from_str_round_trips_via_display() {
        let digest: ContentDigest = ABC_DIGEST.parse().expect("parse");
        assert_eq!(digest.to_string(), ABC_DIGEST);
        assert_eq!(digest, ContentDigest::of_bytes(b"abc"));
    }

    #[test]
    fn mixed_case_hex_canonicalises_to_lowercase() {
        let upper = ABC_DIGEST.to_uppercase().replace("SHA256", "sha256");
        let digest: ContentDigest = upper.parse().expect("mixed-case hex parses");
        assert_eq!(digest.to_string(), ABC_DIGEST);
    }

    #[test]
    fn rejects_a_missing_scheme() {
        let err = "ba7816bf".parse::<ContentDigest>().unwrap_err();
        assert!(matches!(err, DigestParseError::MissingScheme(_)), "{err:?}");
    }

    #[test]
    fn rejects_an_empty_string() {
        let err = "".parse::<ContentDigest>().unwrap_err();
        assert!(matches!(err, DigestParseError::MissingScheme(_)), "{err:?}");
    }

    #[test]
    fn rejects_unknown_schemes() {
        for bad in ["sha1", "blake3", "SHA256", "md5"] {
            let value = format!("{bad}:{}", "a".repeat(64));
            let err = value.parse::<ContentDigest>().unwrap_err();
            assert!(
                matches!(err, DigestParseError::UnsupportedScheme { ref scheme } if scheme == bad),
                "expected scheme rejection for {bad:?}, got {err:?}",
            );
        }
    }

    #[test]
    fn rejects_wrong_length_payloads() {
        for len in [0, 63, 65] {
            let value = format!("sha256:{}", "a".repeat(len));
            let err = value.parse::<ContentDigest>().unwrap_err();
            assert!(
                matches!(err, DigestParseError::Hex { .. }),
                "expected hex rejection at length {len}, got {err:?}",
            );
        }
    }

    #[test]
    fn rejects_non_hex_characters() {
        let value = format!("sha256:{}", "z".repeat(64));
        let err = value.parse::<ContentDigest>().unwrap_err();
        assert!(matches!(err, DigestParseError::Hex { .. }), "{err:?}");
    }

    #[test]
    fn rejects_the_all_zero_uncommitted_sentinel() {
        let err = format!("sha256:{}", "0".repeat(64))
            .parse::<ContentDigest>()
            .unwrap_err();
        assert!(matches!(err, DigestParseError::Uncommitted));
    }

    #[test]
    fn rejects_a_0x_prefixed_payload() {
        let value = format!("sha256:0x{}", "a".repeat(64));
        let err = value.parse::<ContentDigest>().unwrap_err();
        assert!(matches!(err, DigestParseError::Hex { .. }), "{err:?}");
        let value = format!("sha256:0X{}", "a".repeat(64));
        let err = value.parse::<ContentDigest>().unwrap_err();
        assert!(matches!(err, DigestParseError::Hex { .. }), "{err:?}");
    }

    #[test]
    fn mismatch_message_names_both_digests_and_the_path() {
        let declared: ContentDigest = ABC_DIGEST.parse().expect("parse");
        let actual = ContentDigest::of_bytes(b"tampered");
        let err = DigestMismatch {
            path: PathBuf::from("modules/example.wasm"),
            declared,
            actual,
        };
        let msg = err.to_string();
        assert!(msg.contains("modules/example.wasm"), "{msg}");
        assert!(msg.contains(&declared.to_string()), "{msg}");
        assert!(msg.contains(&actual.to_string()), "{msg}");
    }
}
