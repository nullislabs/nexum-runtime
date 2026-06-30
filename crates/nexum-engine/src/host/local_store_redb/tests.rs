use super::*;

fn fresh() -> (tempfile::TempDir, LocalStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = LocalStore::open(dir.path().join("ls.redb")).expect("open");
    (dir, store)
}

#[test]
fn set_get_roundtrip() {
    let (_dir, store) = fresh();
    let ms = store.module("twap").unwrap();
    ms.set("k", b"v").unwrap();
    assert_eq!(ms.get("k").unwrap().as_deref(), Some(&b"v"[..]));
}

#[test]
fn namespaces_isolate_modules() {
    let (_dir, store) = fresh();
    let a = store.module("a").unwrap();
    let b = store.module("b").unwrap();
    a.set("k", b"from-a").unwrap();
    b.set("k", b"from-b").unwrap();
    assert_eq!(a.get("k").unwrap().as_deref(), Some(&b"from-a"[..]));
    assert_eq!(b.get("k").unwrap().as_deref(), Some(&b"from-b"[..]));
}

#[test]
fn delete_then_get_is_none() {
    let (_dir, store) = fresh();
    let ms = store.module("twap").unwrap();
    ms.set("k", b"v").unwrap();
    ms.delete("k").unwrap();
    assert!(ms.get("k").unwrap().is_none());
}

#[test]
fn list_keys_strips_namespace_prefix() {
    let (_dir, store) = fresh();
    let ms = store.module("twap").unwrap();
    ms.set("posted:1", b"x").unwrap();
    ms.set("posted:2", b"y").unwrap();
    ms.set("other", b"z").unwrap();
    let keys = ms.list_keys("posted:").unwrap();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().all(|k| k.starts_with("posted:")));
}

#[test]
fn rejects_empty_namespace() {
    let (_dir, store) = fresh();
    let err = store.module("").unwrap_err();
    assert!(matches!(err, StorageError::InvalidNamespace(_)));
}

#[test]
fn prefix_is_fixed_32_bytes() {
    let short = store_prefix("a");
    let long = store_prefix(&"a".repeat(300));
    assert_eq!(short.len(), PREFIX_LEN);
    assert_eq!(long.len(), PREFIX_LEN);
    // Different inputs produce different prefixes.
    assert_ne!(short, long);
}

#[test]
fn prefix_is_deterministic() {
    let p1 = store_prefix("twap-monitor");
    let p2 = store_prefix("twap-monitor");
    assert_eq!(p1, p2);
}

#[test]
fn similar_names_differ() {
    // Verify that names that share a common prefix don't collide.
    let pa = store_prefix("module-a");
    let pb = store_prefix("module-b");
    assert_ne!(pa, pb);
}

#[test]
fn module_handles_share_underlying_data() {
    let (_dir, store) = fresh();
    let ms1 = store.module("twap").unwrap();
    let ms2 = ms1.clone();
    ms1.set("k", b"v").unwrap();
    assert_eq!(ms2.get("k").unwrap().as_deref(), Some(&b"v"[..]));
}

/// Helper: compute the prefix a ModuleStore would use for `name`.
fn store_prefix(name: &str) -> Vec<u8> {
    keccak256(name.as_bytes()).to_vec()
}
