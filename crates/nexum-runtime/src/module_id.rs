//! Module and provider identity.

use std::borrow::Borrow;
use std::sync::Arc;

use derive_more::{Display, From};

/// The manifest namespace. `Arc`-backed so dispatch-path clones are
/// refcount bumps; `Display` is the bare namespace.
#[derive(Clone, Debug, Display, Eq, From, Hash, PartialEq)]
#[from(forward)]
pub struct ModuleId(Arc<str>);

impl ModuleId {
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

    #[test]
    fn display_is_the_bare_namespace() {
        let id = ModuleId::from("twap-monitor");
        assert_eq!(id.to_string(), "twap-monitor");
        assert_eq!(id.as_str(), "twap-monitor");
    }

    #[test]
    fn keyed_maps_answer_str_queries() {
        let mut map = std::collections::HashMap::new();
        map.insert(ModuleId::from(String::from("keeper")), 1);
        assert_eq!(map.get("keeper"), Some(&1));
        assert_eq!(map.get("other"), None);
    }

    #[test]
    fn metric_label_value_is_the_bare_namespace() {
        let id = ModuleId::from("twap-monitor");
        let label = metrics::SharedString::from(id.clone());
        assert_eq!(&*label, "twap-monitor");
        assert_eq!(&*label, id.as_str());
    }
}
