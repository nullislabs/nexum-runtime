use super::*;

fn fresh() -> (tempfile::TempDir, LocalStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = LocalStore::open(dir.path().join("ls.redb")).expect("open");
    (dir, store)
}

#[test]
fn set_get_roundtrip() {
    let (_dir, store) = fresh();
    store.set("twap", "k", b"v").unwrap();
    assert_eq!(store.get("twap", "k").unwrap().as_deref(), Some(&b"v"[..]));
}

#[test]
fn namespaces_isolate_modules() {
    let (_dir, store) = fresh();
    store.set("a", "k", b"from-a").unwrap();
    store.set("b", "k", b"from-b").unwrap();
    assert_eq!(
        store.get("a", "k").unwrap().as_deref(),
        Some(&b"from-a"[..])
    );
    assert_eq!(
        store.get("b", "k").unwrap().as_deref(),
        Some(&b"from-b"[..])
    );
}

#[test]
fn delete_then_get_is_none() {
    let (_dir, store) = fresh();
    store.set("twap", "k", b"v").unwrap();
    store.delete("twap", "k").unwrap();
    assert!(store.get("twap", "k").unwrap().is_none());
}

#[test]
fn list_keys_strips_namespace_prefix() {
    let (_dir, store) = fresh();
    store.set("twap", "posted:1", b"x").unwrap();
    store.set("twap", "posted:2", b"y").unwrap();
    store.set("twap", "other", b"z").unwrap();
    let keys = store.list_keys("twap", "posted:").unwrap();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().all(|k| k.starts_with("posted:")));
}

#[test]
fn rejects_empty_namespace() {
    let (_dir, store) = fresh();
    let err = store.set("", "k", b"v").unwrap_err();
    assert!(matches!(err, StorageError::InvalidNamespace(_)));
}

#[test]
fn prefix_is_fixed_32_bytes() {
    let short = namespace_prefix("a").unwrap();
    let long = namespace_prefix(&"a".repeat(300)).unwrap();
    assert_eq!(short.len(), PREFIX_LEN);
    assert_eq!(long.len(), PREFIX_LEN);
    // Different inputs produce different prefixes.
    assert_ne!(short, long);
}

#[test]
fn prefix_is_deterministic() {
    let p1 = namespace_prefix("twap-monitor").unwrap();
    let p2 = namespace_prefix("twap-monitor").unwrap();
    assert_eq!(p1, p2);
}

#[test]
fn similar_names_differ() {
    // Verify that names that share a common prefix don't collide.
    let pa = namespace_prefix("module-a").unwrap();
    let pb = namespace_prefix("module-b").unwrap();
    assert_ne!(pa, pb);
}
