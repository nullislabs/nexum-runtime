use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use nexum_sdk::host::{EntryPage, Fault, ListQuery, LocalStoreHost, page_entries};

/// In-memory [`LocalStoreHost`]: namespaced views over one shared row
/// map, with store-wide entry and byte limits.
/// [`namespaced`](Self::namespaced) derives a sibling view over the same
/// rows; identical keys in different namespaces never collide, and limits
/// are shared across namespaces. Every `set` commits immediately, with no
/// transaction rollback on trap.
#[derive(Default)]
pub struct MockLocalStore {
    shared: Rc<SharedRows>,
    namespace: String,
    /// Key patterns that trigger injected faults on any operation.
    error_patterns: RefCell<Vec<(String, Fault)>>,
}

/// Backing rows and limits shared by every namespaced view.
#[derive(Default)]
struct SharedRows {
    /// Rows keyed by `(namespace, key)`.
    rows: RefCell<HashMap<(String, String), Vec<u8>>>,
    /// Total stored bytes (key + value) across all namespaces.
    bytes: Cell<usize>,
    /// When set, `set` on a new key fails once the store holds this
    /// many rows.
    max_entries: Cell<Option<usize>>,
    /// When set, `set` fails once stored bytes would exceed this.
    max_bytes: Cell<Option<usize>>,
}

impl MockLocalStore {
    /// A view over the same rows under `namespace`; same-namespace views
    /// alias, different namespaces isolate identical keys.
    ///
    /// # Panics
    ///
    /// On an empty namespace.
    pub fn namespaced(&self, namespace: impl Into<String>) -> MockLocalStore {
        let namespace = namespace.into();
        assert!(
            !namespace.is_empty(),
            "MockLocalStore: namespace must not be empty",
        );
        MockLocalStore {
            shared: Rc::clone(&self.shared),
            namespace,
            error_patterns: RefCell::new(Vec::new()),
        }
    }

    /// Number of rows in this view's namespace.
    pub fn len(&self) -> usize {
        self.shared
            .rows
            .borrow()
            .keys()
            .filter(|(ns, _)| *ns == self.namespace)
            .count()
    }

    /// Whether this view's namespace holds no rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Direct read of this view's namespace, for assertions.
    pub fn snapshot(&self) -> HashMap<String, Vec<u8>> {
        self.shared
            .rows
            .borrow()
            .iter()
            .filter(|((ns, _), _)| *ns == self.namespace)
            .map(|((_, key), value)| (key.clone(), value.clone()))
            .collect()
    }

    /// Cap row count across all namespaces; `set` on a new key then
    /// fails, overwrites still succeed.
    pub fn set_max_entries(&self, limit: usize) {
        self.shared.max_entries.set(Some(limit));
    }

    /// Cap total stored bytes (key + value, all namespaces); an over-cap
    /// `set` fails, deletes and overwrites release displaced bytes.
    pub fn set_max_bytes(&self, limit: usize) {
        self.shared.max_bytes.set(Some(limit));
    }

    /// Inject a fault for any operation whose key starts with `prefix`;
    /// first registered match fires.
    pub fn fail_on(&self, prefix: impl Into<String>, fault: Fault) {
        self.error_patterns
            .borrow_mut()
            .push((prefix.into(), fault));
    }

    fn check_injected_error(&self, key: &str) -> Result<(), Fault> {
        for (pattern, fault) in self.error_patterns.borrow().iter() {
            if key.starts_with(pattern) {
                return Err(fault.clone());
            }
        }
        Ok(())
    }
}

impl LocalStoreHost for MockLocalStore {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Fault> {
        self.check_injected_error(key)?;
        Ok(self
            .shared
            .rows
            .borrow()
            .get(&(self.namespace.clone(), key.to_string()))
            .cloned())
    }
    fn set(&self, key: &str, value: &[u8]) -> Result<(), Fault> {
        self.check_injected_error(key)?;
        let mut rows = self.shared.rows.borrow_mut();
        let compound = (self.namespace.clone(), key.to_string());
        let existing = rows.get(&compound).map(Vec::len);
        if existing.is_none()
            && let Some(limit) = self.shared.max_entries.get()
            && rows.len() >= limit
        {
            return Err(Fault::Internal(format!(
                "MockLocalStore: max entries ({limit}) reached"
            )));
        }
        // Same-key overwrites release the displaced bytes before the
        // new row is charged.
        let displaced = existing.map_or(0, |len| key.len() + len);
        let total = self.shared.bytes.get() - displaced + key.len() + value.len();
        if let Some(budget) = self.shared.max_bytes.get()
            && total > budget
        {
            return Err(Fault::Internal(format!(
                "MockLocalStore: max bytes ({budget}) reached"
            )));
        }
        rows.insert(compound, value.to_vec());
        self.shared.bytes.set(total);
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), Fault> {
        self.check_injected_error(key)?;
        if let Some(value) = self
            .shared
            .rows
            .borrow_mut()
            .remove(&(self.namespace.clone(), key.to_string()))
        {
            self.shared
                .bytes
                .set(self.shared.bytes.get() - key.len() - value.len());
        }
        Ok(())
    }
    fn list_keys(&self, prefix: &str) -> Result<Vec<String>, Fault> {
        self.check_injected_error(prefix)?;
        let mut keys: Vec<String> = self
            .shared
            .rows
            .borrow()
            .keys()
            .filter(|(ns, key)| *ns == self.namespace && key.starts_with(prefix))
            .map(|(_, key)| key.clone())
            .collect();
        keys.sort();
        Ok(keys)
    }
    /// One call, not one per key: a module's crossing count under the
    /// mock matches what the real host charges.
    fn list_entries(&self, query: &ListQuery<'_>) -> Result<EntryPage, Fault> {
        self.check_injected_error(query.prefix)?;
        let mut rows: Vec<(String, Vec<u8>)> = self
            .shared
            .rows
            .borrow()
            .iter()
            .filter(|((ns, _), _)| *ns == self.namespace)
            .map(|((_, key), value)| (key.clone(), value.clone()))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(page_entries(query, rows))
    }
    fn contains(&self, key: &str) -> Result<bool, Fault> {
        self.check_injected_error(key)?;
        Ok(self
            .shared
            .rows
            .borrow()
            .contains_key(&(self.namespace.clone(), key.to_string())))
    }
    fn len(&self, key: &str) -> Result<Option<u64>, Fault> {
        self.check_injected_error(key)?;
        Ok(self
            .shared
            .rows
            .borrow()
            .get(&(self.namespace.clone(), key.to_string()))
            .map(|v| v.len() as u64))
    }
    fn count(&self, prefix: &str) -> Result<u64, Fault> {
        self.check_injected_error(prefix)?;
        Ok(self
            .shared
            .rows
            .borrow()
            .keys()
            .filter(|(ns, key)| *ns == self.namespace && key.starts_with(prefix))
            .count() as u64)
    }
}

/// Trap-injection wrapper over a [`LocalStoreHost`]: counts `set` and
/// `delete` calls and, once armed, simulates a guest trap mid-flow.
/// [`arm_after`](Self::arm_after)`(n)` lets the next `n` writes land;
/// the write after that trips the trap, and from then on every
/// operation - reads included - faults until
/// [`disarm`](Self::disarm), because nothing past a trap executes.
/// Sweeping `n` over a flow's write count drives the store through
/// every torn prefix a trap can strand, so a recovery pass can be
/// held to convergence from each one.
///
/// The rule this harness enforces: no in-store invariant
/// may span two `set` calls unless the intermediate state is
/// self-healing or the writes ride the atomic `apply` batch verb.
pub struct TrapStore<H> {
    inner: H,
    /// Write calls the trap let through.
    writes: Cell<u64>,
    /// Every call the trap let through, so a test can hold a flow to a
    /// boundary-crossing count.
    calls: Cell<u64>,
    /// Writes still allowed before the trap trips; `None` when unarmed.
    remaining: Cell<Option<u64>>,
    tripped: Cell<bool>,
}

impl<H> TrapStore<H> {
    /// Wrap `inner`, unarmed: every operation delegates, writes are
    /// counted.
    pub fn new(inner: H) -> Self {
        Self {
            inner,
            writes: Cell::new(0),
            calls: Cell::new(0),
            remaining: Cell::new(None),
            tripped: Cell::new(false),
        }
    }

    /// Arm the trap: the next `n` writes land, the one after trips it.
    pub fn arm_after(&self, n: u64) {
        self.remaining.set(Some(n));
        self.tripped.set(false);
    }

    /// Clear the trap and the tripped state; operations resume. The
    /// write count keeps accumulating.
    pub fn disarm(&self) {
        self.remaining.set(None);
        self.tripped.set(false);
    }

    /// `set`/`delete` calls the trap let through since construction.
    pub fn writes(&self) -> u64 {
        self.writes.get()
    }

    /// Every call the trap let through since construction; a write
    /// counts once, since it is gated as a read first.
    pub fn calls(&self) -> u64 {
        self.calls.get()
    }

    /// Whether the trap has fired.
    pub fn tripped(&self) -> bool {
        self.tripped.get()
    }

    /// The wrapped store.
    pub fn inner(&self) -> &H {
        &self.inner
    }

    /// Fault unless still executing: past the trap nothing runs.
    fn read_gate(&self) -> Result<(), Fault> {
        if self.tripped.get() {
            return Err(Fault::Internal("TrapStore: trapped".into()));
        }
        self.calls.set(self.calls.get() + 1);
        Ok(())
    }

    /// Spend one write from the armed budget, tripping at zero.
    fn write_gate(&self) -> Result<(), Fault> {
        self.read_gate()?;
        if let Some(remaining) = self.remaining.get() {
            if remaining == 0 {
                self.tripped.set(true);
                return Err(Fault::Internal("TrapStore: trapped".into()));
            }
            self.remaining.set(Some(remaining - 1));
        }
        self.writes.set(self.writes.get() + 1);
        Ok(())
    }
}

impl<H: LocalStoreHost> LocalStoreHost for TrapStore<H> {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Fault> {
        self.read_gate()?;
        self.inner.get(key)
    }
    fn set(&self, key: &str, value: &[u8]) -> Result<(), Fault> {
        self.write_gate()?;
        self.inner.set(key, value)
    }
    fn delete(&self, key: &str) -> Result<(), Fault> {
        self.write_gate()?;
        self.inner.delete(key)
    }
    fn list_keys(&self, prefix: &str) -> Result<Vec<String>, Fault> {
        self.read_gate()?;
        self.inner.list_keys(prefix)
    }
    fn list_entries(&self, query: &ListQuery<'_>) -> Result<EntryPage, Fault> {
        self.read_gate()?;
        self.inner.list_entries(query)
    }
    fn contains(&self, key: &str) -> Result<bool, Fault> {
        self.read_gate()?;
        self.inner.contains(key)
    }
    fn len(&self, key: &str) -> Result<Option<u64>, Fault> {
        self.read_gate()?;
        self.inner.len(key)
    }
    fn count(&self, prefix: &str) -> Result<u64, Fault> {
        self.read_gate()?;
        self.inner.count(prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_store_round_trips() {
        let store = MockLocalStore::default();
        store.set("k", b"v").unwrap();
        assert_eq!(store.get("k").unwrap().as_deref(), Some(&b"v"[..]));
        store.delete("k").unwrap();
        assert!(store.get("k").unwrap().is_none());
    }

    #[test]
    fn local_store_list_keys_prefix_scan() {
        let store = MockLocalStore::default();
        store.set("commitment:a:1", b"").unwrap();
        store.set("commitment:a:2", b"").unwrap();
        store.set("submitted:1", b"").unwrap();
        let keys = store.list_keys("commitment:").unwrap();
        assert_eq!(keys, vec!["commitment:a:1", "commitment:a:2"]);
    }

    #[test]
    fn local_store_list_entries_filters_pages_and_isolates_namespaces() {
        use nexum_sdk::host::ValueFilter;

        let store = MockLocalStore::default();
        let other = store.namespaced("other");
        store.set("p:1", b"\x01one").unwrap();
        store.set("p:2", b"\x02two").unwrap();
        store.set("p:3", b"\x01three").unwrap();
        other.set("p:9", b"\x01nine").unwrap();
        let page = |view: &MockLocalStore, start_after, limit| {
            view.list_entries(&ListQuery {
                prefix: "p:",
                start_after,
                limit,
                scan_limit: 0,
                filter: Some(ValueFilter::LacksPrefix(&[0x02])),
            })
            .unwrap()
        };

        let first = page(&store, "", 1);
        assert_eq!(first.entries, vec![("p:1".to_owned(), b"\x01one".to_vec())]);
        assert!(!first.exhausted);

        let rest = page(&store, first.last_examined.as_deref().unwrap(), 0);
        assert_eq!(
            rest.entries,
            vec![("p:3".to_owned(), b"\x01three".to_vec())],
        );
        assert!(rest.exhausted);

        assert_eq!(page(&other, "", 0).entries.len(), 1);
    }

    #[test]
    fn local_store_metadata_queries() {
        let store = MockLocalStore::default();
        store.set("watch:a", b"abc").unwrap();
        store.set("watch:b", b"").unwrap();
        store.set("posted:1", b"x").unwrap();

        assert!(store.contains("watch:a").unwrap());
        assert!(!store.contains("missing").unwrap());
        assert_eq!(LocalStoreHost::len(&store, "watch:a").unwrap(), Some(3));
        assert_eq!(LocalStoreHost::len(&store, "watch:b").unwrap(), Some(0));
        assert_eq!(LocalStoreHost::len(&store, "missing").unwrap(), None);
        assert_eq!(store.count("watch:").unwrap(), 2);
        assert_eq!(store.count("").unwrap(), 3);

        // Metadata queries stay namespace-scoped.
        let other = store.namespaced("other");
        assert_eq!(other.count("").unwrap(), 0);
        assert!(!other.contains("watch:a").unwrap());

        // And respect fault injection.
        store.fail_on("bad:", Fault::Internal("injected".into()));
        assert!(store.contains("bad:k").is_err());
        assert!(LocalStoreHost::len(&store, "bad:k").is_err());
        assert!(store.count("bad:").is_err());
    }

    #[test]
    fn local_store_error_injection() {
        let store = MockLocalStore::default();
        store.fail_on("bad:", Fault::Internal("injected".into()));
        // Non-matching keys work fine.
        store.set("good:k", b"v").unwrap();
        assert_eq!(store.get("good:k").unwrap().as_deref(), Some(&b"v"[..]));
        // Matching keys trigger the error.
        assert!(store.set("bad:k", b"v").is_err());
        assert!(store.get("bad:k").is_err());
        assert!(store.delete("bad:k").is_err());
        assert!(store.list_keys("bad:").is_err());
    }

    #[test]
    fn trap_store_counts_writes_unarmed() {
        let store = TrapStore::new(MockLocalStore::default());
        store.set("a", b"1").unwrap();
        store.set("b", b"2").unwrap();
        store.delete("a").unwrap();
        assert_eq!(store.writes(), 3);
        assert!(!store.tripped());
        assert_eq!(store.get("b").unwrap().as_deref(), Some(&b"2"[..]));
    }

    #[test]
    fn trap_store_trips_after_the_armed_budget() {
        let store = TrapStore::new(MockLocalStore::default());
        store.arm_after(2);
        store.set("a", b"1").unwrap();
        store.delete("a").unwrap();
        // The third write trips; the row never lands.
        assert!(store.set("b", b"2").is_err());
        assert!(store.tripped());
        assert_eq!(store.writes(), 2);
        assert!(store.inner().get("b").unwrap().is_none());
    }

    #[test]
    fn trap_store_faults_every_operation_once_tripped() {
        let store = TrapStore::new(MockLocalStore::default());
        store.set("a", b"1").unwrap();
        store.arm_after(0);
        assert!(store.set("b", b"2").is_err());
        // Nothing past a trap executes, reads included.
        assert!(store.get("a").is_err());
        assert!(store.list_keys("").is_err());
        assert!(store.contains("a").is_err());
        assert!(store.delete("a").is_err());
    }

    #[test]
    fn trap_store_disarm_resumes_over_the_surviving_rows() {
        let store = TrapStore::new(MockLocalStore::default());
        store.set("a", b"1").unwrap();
        store.arm_after(0);
        assert!(store.set("b", b"2").is_err());

        store.disarm();
        assert!(!store.tripped());
        // The torn prefix survives: `a` landed, `b` never did.
        assert_eq!(store.get("a").unwrap().as_deref(), Some(&b"1"[..]));
        assert!(store.get("b").unwrap().is_none());
        store.set("b", b"2").unwrap();
        assert_eq!(store.writes(), 2);
    }

    #[test]
    fn local_store_max_entries_enforced() {
        let store = MockLocalStore::default();
        store.set_max_entries(2);
        store.set("a", b"1").unwrap();
        store.set("b", b"2").unwrap();
        // Updating an existing key is OK even at the limit.
        store.set("b", b"3").unwrap();
        // Adding a new key exceeds the limit.
        let err = store.set("c", b"4").unwrap_err();
        assert!(matches!(err, Fault::Internal(ref m) if m.contains("max entries")));
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn local_store_namespaces_isolate_identical_keys() {
        let store = MockLocalStore::default();
        let other = store.namespaced("other-module");
        store.set("watch:a", b"mine").unwrap();
        other.set("watch:a", b"theirs").unwrap();

        assert_eq!(store.get("watch:a").unwrap().as_deref(), Some(&b"mine"[..]));
        assert_eq!(
            other.get("watch:a").unwrap().as_deref(),
            Some(&b"theirs"[..]),
        );

        // Scans, counts, and snapshots stay view-scoped.
        assert_eq!(store.len(), 1);
        assert_eq!(other.len(), 1);
        assert_eq!(store.list_keys("").unwrap(), vec!["watch:a"]);
        assert_eq!(store.snapshot().get("watch:a").unwrap(), b"mine");

        // Deletes never reach across the namespace boundary.
        other.delete("watch:a").unwrap();
        assert!(other.is_empty());
        assert_eq!(store.get("watch:a").unwrap().as_deref(), Some(&b"mine"[..]));
    }

    #[test]
    fn local_store_same_namespace_views_alias_the_same_rows() {
        let store = MockLocalStore::default();
        let one = store.namespaced("mod");
        let two = store.namespaced("mod");
        one.set("k", b"v").unwrap();
        assert_eq!(two.get("k").unwrap().as_deref(), Some(&b"v"[..]));
    }

    #[test]
    #[should_panic(expected = "namespace must not be empty")]
    fn local_store_empty_namespace_panics() {
        let _ = MockLocalStore::default().namespaced("");
    }

    #[test]
    fn local_store_entry_limit_spans_namespaces() {
        let store = MockLocalStore::default();
        store.set_max_entries(2);
        let other = store.namespaced("other-module");
        store.set("a", b"1").unwrap();
        other.set("b", b"2").unwrap();
        // The store is one shared file: a sibling namespace's rows
        // consume the same headroom.
        let err = store.set("c", b"3").unwrap_err();
        assert!(matches!(err, Fault::Internal(ref m) if m.contains("max entries")));
    }

    #[test]
    fn local_store_byte_budget_enforced_and_released() {
        let store = MockLocalStore::default();
        store.set_max_bytes(8);
        store.set("abcd", b"1234").unwrap(); // 4 + 4 = 8, exactly at budget
        let err = store.set("x", b"y").unwrap_err();
        assert!(matches!(err, Fault::Internal(ref m) if m.contains("max bytes")));

        // A same-key overwrite releases the displaced value first.
        store.set("abcd", b"12").unwrap();
        store.set("x", b"y").unwrap();

        // Deleting releases the whole row's bytes.
        store.delete("abcd").unwrap();
        store.set("ab", b"12").unwrap();
        assert_eq!(store.len(), 2);
    }
}
