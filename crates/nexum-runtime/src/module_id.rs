//! Module and provider identity.

use std::borrow::Borrow;
use std::sync::Arc;

use derive_more::{AsRef, Display, From};

/// Identity of one loaded module or provider: its manifest namespace.
/// `Arc`-backed so dispatch-path clones are refcount bumps; `Display` is
/// the bare namespace, keeping log and metric values unchanged.
#[derive(AsRef, Clone, Debug, Display, Eq, From, Hash, Ord, PartialEq, PartialOrd)]
#[as_ref(str)]
#[from(forward)]
pub struct ModuleId(Arc<str>);

impl ModuleId {
    /// The namespace as a borrowed string.
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
}
