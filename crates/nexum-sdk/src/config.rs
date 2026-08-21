//! Helpers over the `Vec<(String, String)>` `[config]` entries a
//! module's `init` receives: required and optional key lookup,
//! fixed-point decimal parsing, and a write-once [`Slot`] holding the
//! parsed result.

use std::sync::OnceLock;

use alloy_primitives::{I256, U256};
use thiserror::Error;

use crate::host::Fault;

/// Why a config lookup or parse failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The key was not present in the `entries` slice.
    #[error("missing key {key:?}")]
    MissingKey {
        /// Config-table key the lookup was for.
        key: String,
    },
    /// The value at `key` did not parse as the expected shape.
    #[error("parse {key:?}: {detail}")]
    Parse {
        /// Config-table key whose value failed to parse.
        key: String,
        /// Free-text parser detail.
        detail: String,
    },
    /// The value parsed but did not fit the target type's range.
    #[error("range {key:?}: {detail}")]
    Range {
        /// Config-table key whose value overflowed.
        key: String,
        /// Free-text range detail.
        detail: String,
    },
    /// A [`Slot`] was read before `init` stored anything in it.
    #[error("config not initialized")]
    NotInitialized,
    /// A [`Slot`] that already holds a value was stored to again.
    #[error("config already initialized")]
    AlreadyInitialized,
}

impl From<ConfigError> for Fault {
    fn from(e: ConfigError) -> Self {
        let message = e.to_string();
        match e {
            // Not a bad request: the config the module needs is not
            // ready yet.
            ConfigError::NotInitialized => Fault::Unavailable(message),
            ConfigError::MissingKey { .. }
            | ConfigError::Parse { .. }
            | ConfigError::Range { .. }
            | ConfigError::AlreadyInitialized => Fault::InvalidInput(message),
        }
    }
}

/// Write-once holder for the config a module parses in `init` and
/// reads in later dispatches.
///
/// The supervisor instantiates on a fresh store and calls `init` at
/// once, so the slot is instance memory that each (re)instantiation
/// seeds again, and nothing persists it across one.
pub struct Slot<T: Send + Sync>(OnceLock<T>);

impl<T: Send + Sync> Slot<T> {
    /// An empty slot, `const` so a `static` can hold one.
    #[must_use]
    pub const fn new() -> Self {
        Self(OnceLock::new())
    }

    /// Seed the slot; `Err(AlreadyInitialized)` if already seeded.
    pub fn store(&self, value: T) -> Result<(), ConfigError> {
        self.0
            .set(value)
            .map_err(|_| ConfigError::AlreadyInitialized)
    }

    /// Borrow the seeded value; `Err(NotInitialized)` before `store`.
    pub fn get(&self) -> Result<&T, ConfigError> {
        self.0.get().ok_or(ConfigError::NotInitialized)
    }
}

impl<T: Send + Sync> Default for Slot<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Look up a required entry; `Err(MissingKey)` if absent.
pub fn get_required<'a>(
    entries: &'a [(String, String)],
    key: &str,
) -> Result<&'a str, ConfigError> {
    entries
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .ok_or_else(|| ConfigError::MissingKey {
            key: key.to_owned(),
        })
}

/// Look up an optional entry; `None` when absent.
pub fn get_optional<'a>(entries: &'a [(String, String)], key: &str) -> Option<&'a str> {
    entries
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Parse a signed fixed-point decimal string into an `I256` scaled by
/// `10**decimals`. Short fractions are right-padded, long fractions
/// truncated, a leading `-` honoured; empty input and non-digit
/// characters (beyond the sign and one `.`) are rejected. `key` is
/// embedded in the error.
pub fn scale_decimal(value: &str, decimals: u32, key: &str) -> Result<I256, ConfigError> {
    let (sign, body) = if let Some(rest) = value.strip_prefix('-') {
        (-1i32, rest)
    } else {
        (1, value)
    };
    let (whole, frac) = match body.split_once('.') {
        Some((w, f)) => (w, f),
        None => (body, ""),
    };
    if whole.is_empty() && frac.is_empty() {
        return Err(ConfigError::Parse {
            key: key.to_owned(),
            detail: "empty".to_owned(),
        });
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return Err(ConfigError::Parse {
            key: key.to_owned(),
            detail: format!("non-digit character in {value:?}"),
        });
    }
    let frac_len = frac.len() as u32;
    let composed: String = if frac_len <= decimals {
        let mut s = String::with_capacity(whole.len() + decimals as usize);
        s.push_str(whole);
        s.push_str(frac);
        for _ in 0..(decimals - frac_len) {
            s.push('0');
        }
        s
    } else {
        let mut s = String::with_capacity(whole.len() + decimals as usize);
        s.push_str(whole);
        s.push_str(&frac[..decimals as usize]);
        s
    };
    let raw = if composed.is_empty() { "0" } else { &composed };
    let unsigned: U256 = raw.parse().map_err(|e| ConfigError::Parse {
        key: key.to_owned(),
        detail: format!("{e}"),
    })?;
    let signed = I256::try_from(unsigned).map_err(|e| ConfigError::Range {
        key: key.to_owned(),
        detail: format!("{e}"),
    })?;
    Ok(if sign < 0 { -signed } else { signed })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn get_required_finds_value() {
        let cfg = entries(&[("a", "1"), ("b", "2")]);
        assert_eq!(get_required(&cfg, "a").unwrap(), "1");
        assert_eq!(get_required(&cfg, "b").unwrap(), "2");
    }

    #[test]
    fn get_required_missing_is_typed_error() {
        let cfg = entries(&[("a", "1")]);
        let err = get_required(&cfg, "b").unwrap_err();
        assert!(matches!(err, ConfigError::MissingKey { ref key } if key == "b"));
    }

    #[test]
    fn lookup_error_folds_into_an_invalid_input_fault() {
        let fault = Fault::from(get_required(&entries(&[]), "threshold").unwrap_err());
        let Fault::InvalidInput(message) = fault else {
            panic!("expected invalid-input fault, got {fault:?}");
        };
        assert!(message.contains("threshold"));
    }

    #[test]
    fn double_store_folds_into_an_invalid_input_fault() {
        assert!(matches!(
            Fault::from(ConfigError::AlreadyInitialized),
            Fault::InvalidInput(_)
        ));
    }

    #[test]
    fn not_initialized_folds_into_an_unavailable_fault() {
        assert!(matches!(
            Fault::from(ConfigError::NotInitialized),
            Fault::Unavailable(_)
        ));
    }

    #[test]
    fn slot_get_borrows_the_stored_value() {
        let slot = Slot::new();
        slot.store("a".to_owned()).unwrap();
        assert_eq!(slot.get().unwrap(), "a");
        assert_eq!(slot.get().unwrap(), "a");
    }

    #[test]
    fn slot_read_before_store_is_a_typed_error() {
        let slot: Slot<u32> = Slot::new();
        assert!(matches!(
            slot.get().unwrap_err(),
            ConfigError::NotInitialized
        ));
    }

    #[test]
    fn slot_second_store_refuses_and_keeps_the_first_value() {
        let slot = Slot::new();
        slot.store(1u32).unwrap();
        assert!(matches!(
            slot.store(2).unwrap_err(),
            ConfigError::AlreadyInitialized
        ));
        assert_eq!(*slot.get().unwrap(), 1);
    }

    #[test]
    fn get_optional_returns_none_for_missing() {
        let cfg = entries(&[("a", "1")]);
        assert_eq!(get_optional(&cfg, "missing"), None);
        assert_eq!(get_optional(&cfg, "a"), Some("1"));
    }

    #[test]
    fn scale_decimal_pads_short_fractional() {
        // "2500.00" with 8 decimals -> 2500 * 1e8 = 250_000_000_000
        let v = scale_decimal("2500.00", 8, "threshold").unwrap();
        assert_eq!(v, I256::try_from(250_000_000_000_i128).unwrap());
    }

    #[test]
    fn scale_decimal_truncates_long_fractional() {
        // "1.123456789" with 4 decimals -> "11234"
        let v = scale_decimal("1.123456789", 4, "threshold").unwrap();
        assert_eq!(v, I256::try_from(11234_i128).unwrap());
    }

    #[test]
    fn scale_decimal_handles_no_decimal_point() {
        let v = scale_decimal("42", 4, "x").unwrap();
        assert_eq!(v, I256::try_from(420_000_i128).unwrap());
    }

    #[test]
    fn scale_decimal_handles_negative() {
        let v = scale_decimal("-2.5", 2, "x").unwrap();
        assert_eq!(v, I256::try_from(-250_i128).unwrap());
    }

    #[test]
    fn scale_decimal_rejects_empty() {
        let err = scale_decimal("", 2, "x").unwrap_err();
        assert!(
            matches!(err, ConfigError::Parse { ref key, .. } if key == "x"),
            "got {err:?}"
        );
    }

    #[test]
    fn scale_decimal_rejects_garbage() {
        let err = scale_decimal("not-a-number", 2, "x").unwrap_err();
        assert!(
            matches!(err, ConfigError::Parse { ref key, .. } if key == "x"),
            "got {err:?}"
        );
    }
}
