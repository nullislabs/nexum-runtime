//! Module and provider identity.

use std::borrow::Borrow;
use std::sync::Arc;

use derive_more::Display;
use thiserror::Error;

/// Why a `[component].name` cannot become a [`ModuleId`].
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum InvalidModuleName {
    #[error("[component].name is missing or blank; declare a non-empty name")]
    Blank,
    #[error("[component].name {0:?} must not contain '/', '\\', or '..'")]
    UnsafePathComponent(String),
}

/// The manifest namespace. `Arc`-backed so dispatch-path clones are
/// refcount bumps; `Display` is the bare namespace.
///
/// [`ModuleId::parse`] is the only constructor: the name becomes a
/// state-directory namespace, and an unchecked one could escape it.
#[derive(Clone, Debug, Display, Eq, Hash, PartialEq)]
pub struct ModuleId(Arc<str>);

impl ModuleId {
    pub fn parse(name: &str) -> Result<Self, InvalidModuleName> {
        if name.trim().is_empty() {
            return Err(InvalidModuleName::Blank);
        }
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err(InvalidModuleName::UnsafePathComponent(name.to_owned()));
        }
        Ok(Self(Arc::from(name)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Lets `ModuleId`-keyed maps answer plain `&str` queries; hash and
/// equality already delegate to the string.
impl Borrow<str> for ModuleId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// Hands a metric label the backing `Arc` instead of a copy, so a
/// per-dispatch label value is a refcount bump over the same bytes.
impl From<ModuleId> for metrics::SharedString {
    fn from(id: ModuleId) -> Self {
        Self::from_shared(id.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> ModuleId {
        ModuleId::parse(name).expect("valid name")
    }

    #[test]
    fn display_is_the_bare_namespace() {
        let id = id("twap-monitor");
        assert_eq!(id.to_string(), "twap-monitor");
        assert_eq!(id.as_str(), "twap-monitor");
    }

    #[test]
    fn keyed_maps_answer_str_queries() {
        let mut map = std::collections::HashMap::new();
        map.insert(id("keeper"), 1);
        assert_eq!(map.get("keeper"), Some(&1));
        assert_eq!(map.get("other"), None);
    }

    #[test]
    fn metric_label_value_is_the_bare_namespace() {
        let id = id("twap-monitor");
        let label = metrics::SharedString::from(id.clone());
        assert_eq!(&*label, "twap-monitor");
        assert_eq!(&*label, id.as_str());
    }

    #[test]
    fn parse_refuses_a_name_that_escapes_the_state_dir() {
        for bad in ["../evil", "a/b", "a\\b", "..", "/etc/passwd", "foo/../bar"] {
            assert_eq!(
                ModuleId::parse(bad),
                Err(InvalidModuleName::UnsafePathComponent(bad.to_owned())),
                "expected refusal for {bad:?}",
            );
        }
    }

    #[test]
    fn parse_refuses_a_blank_name() {
        for blank in ["", " ", "\t", "\n", " \t \n "] {
            assert_eq!(
                ModuleId::parse(blank),
                Err(InvalidModuleName::Blank),
                "expected refusal for {blank:?}",
            );
        }
    }
}
