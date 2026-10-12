use std::path::PathBuf;

use alloy_primitives::keccak256;
use nexum_runtime_api::ListQuery;
use redb::ReadableTable;

use super::*;
use crate::LocalStore;

type Rows = Vec<(Vec<u8>, Vec<u8>)>;

/// The marker row the test steps write, outside every module namespace.
const MARKER: &[u8] = b"test:migrated";

fn temp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ls.redb");
    (dir, path)
}

/// Every row of the data table, read through raw redb.
fn data_rows(path: &Path) -> Rows {
    let db = Database::open(path).expect("raw open");
    let txn = db.begin_read().expect("read txn");
    let table = txn.open_table(TABLE).expect("data table");
    table
        .iter()
        .expect("iter")
        .map(|entry| {
            let (key, value) = entry.expect("row");
            (key.value().to_vec(), value.value().to_vec())
        })
        .collect()
}

/// The raw stamp bytes; `None` when the metadata table is absent.
fn stamp(path: &Path) -> Option<Vec<u8>> {
    let db = Database::open(path).expect("raw open");
    let txn = db.begin_read().expect("read txn");
    let meta = match txn.open_table(META) {
        Ok(meta) => meta,
        Err(TableError::TableDoesNotExist(_)) => return None,
        Err(err) => panic!("metadata table: {err}"),
    };
    meta.get(FORMAT_VERSION_KEY)
        .expect("stamp")
        .map(|bytes| bytes.value().to_vec())
}

fn write_stamp(path: &Path, bytes: &[u8]) {
    let db = Database::create(path).expect("raw create");
    let txn = db.begin_write().expect("write txn");
    txn.open_table(META)
        .expect("metadata table")
        .insert(FORMAT_VERSION_KEY, bytes)
        .expect("insert");
    txn.commit().expect("commit");
}

/// A database as every engine before the stamp wrote it: the data table
/// and nothing else.
fn legacy(path: &Path, rows: &[(&str, &str, &[u8])]) {
    let db = Database::create(path).expect("raw create");
    let txn = db.begin_write().expect("write txn");
    {
        let mut table = txn.open_table(TABLE).expect("data table");
        for (namespace, key, value) in rows {
            let mut full = keccak256(namespace.as_bytes()).to_vec();
            full.extend_from_slice(key.as_bytes());
            table.insert(full.as_slice(), *value).expect("insert");
        }
    }
    txn.commit().expect("commit");
}

fn noop(_: &WriteTransaction) -> Result<(), BoxError> {
    Ok(())
}

fn fail(_: &WriteTransaction) -> Result<(), BoxError> {
    Err("step refused".into())
}

/// Refuses a second run, so a rerun of an applied step fails the open.
fn mark_two(txn: &WriteTransaction) -> Result<(), BoxError> {
    let mut table = txn.open_table(TABLE)?;
    if table.get(MARKER)?.is_some() {
        return Err("step 1 to 2 ran twice".into());
    }
    table.insert(MARKER, b"2".as_slice())?;
    Ok(())
}

/// Refuses to run before [`mark_two`], which pins the step order.
fn mark_three(txn: &WriteTransaction) -> Result<(), BoxError> {
    let mut table = txn.open_table(TABLE)?;
    let seen = table.get(MARKER)?.map(|value| value.value().to_vec());
    if seen.as_deref() != Some(b"2") {
        return Err("step 2 to 3 ran out of order".into());
    }
    table.insert(MARKER, b"3".as_slice())?;
    Ok(())
}

fn format(version: u32, steps: &[MigrationStep]) -> Format<'_> {
    Format { version, steps }
}

fn marker(path: &Path) -> Option<Vec<u8>> {
    data_rows(path)
        .into_iter()
        .find(|(key, _)| key == MARKER)
        .map(|(_, value)| value)
}

#[test]
fn the_engine_format_has_one_step_per_older_version() {
    assert_eq!(Format::ENGINE.version, FORMAT_VERSION);
    assert_eq!(Format::ENGINE.steps.len() as u32 + 1, FORMAT_VERSION);
}

#[test]
fn a_fresh_database_is_stamped_current() {
    let (_dir, path) = temp();
    let store = LocalStore::open(&path).expect("open");
    assert_eq!(store.format_found(), FormatFound::Fresh);
    assert_eq!(store.format_version(), FORMAT_VERSION);
    drop(store);
    assert_eq!(stamp(&path), Some(FORMAT_VERSION.to_le_bytes().to_vec()));
}

/// `Database::create` initializes an empty file, so the open must too.
#[test]
fn an_empty_file_is_a_fresh_database() {
    let (_dir, path) = temp();
    std::fs::write(&path, b"").expect("empty file");
    let store = LocalStore::open(&path).expect("open");
    assert_eq!(store.format_found(), FormatFound::Fresh);
}

#[test]
fn an_unstamped_database_is_adopted_as_version_one_with_its_data_intact() {
    let (_dir, path) = temp();
    legacy(
        &path,
        &[
            ("twap", "cursor", b"\x2a\x00"),
            ("twap", "posted:1", b""),
            ("host/twap", "chainlog_cursor:ab", &7u64.to_le_bytes()),
        ],
    );
    assert_eq!(stamp(&path), None);
    let before = data_rows(&path);
    assert_eq!(before.len(), 3);

    let store = LocalStore::open(&path).expect("adopt");
    assert_eq!(store.format_found(), FormatFound::Unstamped);
    assert_eq!(store.format_version(), 1);
    let twap = store.module("twap").unwrap();
    assert_eq!(
        twap.get("cursor").unwrap().as_deref(),
        Some(&b"\x2a\x00"[..])
    );
    assert_eq!(twap.list_keys("").unwrap(), ["cursor", "posted:1"]);
    drop(twap);
    drop(store);

    assert_eq!(data_rows(&path), before, "adoption rewrote module data");
    assert_eq!(stamp(&path), Some(1u32.to_le_bytes().to_vec()));
}

#[test]
fn the_current_version_reopens() {
    let (_dir, path) = temp();
    {
        let store = LocalStore::open(&path).expect("open");
        store.module("twap").unwrap().set("k", b"v").unwrap();
    }
    let store = LocalStore::open(&path).expect("reopen");
    assert_eq!(
        store.format_found(),
        FormatFound::Stamped(FORMAT_VERSION),
        "the first open stamped the file",
    );
    assert_eq!(store.format_version(), FORMAT_VERSION);
    let twap = store.module("twap").unwrap();
    assert_eq!(twap.get("k").unwrap().as_deref(), Some(&b"v"[..]));
}

#[test]
fn a_newer_stamp_is_refused_and_the_file_is_unmodified() {
    let (_dir, path) = temp();
    let newer = format(FORMAT_VERSION + 1, &[noop]);
    {
        let store = LocalStore::open_as(&path, &newer).expect("newer engine");
        store.module("twap").unwrap().set("k", b"v").unwrap();
    }
    let before = std::fs::read(&path).expect("read file");

    let err = LocalStore::open(&path).expect_err("a newer store must not open");
    assert!(
        matches!(
            err,
            StorageError::FormatTooNew { found, supported }
                if found == FORMAT_VERSION + 1 && supported == FORMAT_VERSION
        ),
        "{err:?}",
    );
    // Operator wording pin: both versions and both ways out.
    let message = err.to_string();
    for needle in [
        format!("format version {}", FORMAT_VERSION + 1),
        format!("than version {FORMAT_VERSION}"),
        "run the engine that wrote it".to_owned(),
        "restore a backup".to_owned(),
    ] {
        assert!(message.contains(&needle), "{needle:?} missing: {message}");
    }

    assert!(
        std::fs::read(&path).expect("read file") == before,
        "the refusal changed the file",
    );
    let store = LocalStore::open_as(&path, &newer).expect("the newer engine still opens it");
    assert_eq!(
        store.format_found(),
        FormatFound::Stamped(FORMAT_VERSION + 1)
    );
    let twap = store.module("twap").unwrap();
    assert_eq!(twap.get("k").unwrap().as_deref(), Some(&b"v"[..]));
}

/// A copy taken under a live writer is a crash image: redb repairs it
/// before any table reads, so the read-only refusal cannot run and the
/// writable path must refuse instead, without touching a table.
#[test]
fn a_newer_stamp_is_refused_after_an_unclean_shutdown() {
    let (dir, path) = temp();
    let crashed = dir.path().join("crashed.redb");
    let newer = format(FORMAT_VERSION + 1, &[noop]);
    {
        let store = LocalStore::open_as(&path, &newer).expect("newer engine");
        store.module("twap").unwrap().set("k", b"v").unwrap();
        std::fs::copy(&path, &crashed).expect("copy the live file");
    }
    assert!(
        matches!(
            ReadOnlyDatabase::open(&crashed),
            Err(DatabaseError::RepairAborted)
        ),
        "the fixture is not a crash image, so this test covers nothing",
    );

    let err = LocalStore::open(&crashed).expect_err("a newer store must not open");
    assert!(matches!(err, StorageError::FormatTooNew { .. }), "{err:?}");
    assert_eq!(
        stamp(&crashed),
        Some((FORMAT_VERSION + 1).to_le_bytes().to_vec())
    );
    assert_eq!(data_rows(&crashed), data_rows(&path));
}

#[test]
fn an_older_version_runs_its_steps_in_order_and_lands_the_stamp() {
    let (_dir, path) = temp();
    {
        let store = LocalStore::open(&path).expect("version 1");
        store.module("twap").unwrap().set("k", b"v").unwrap();
    }
    let v3 = format(3, &[mark_two, mark_three]);
    {
        let store = LocalStore::open_as(&path, &v3).expect("migrate");
        assert_eq!(store.format_found(), FormatFound::Stamped(1));
        assert_eq!(store.format_version(), 3);
        let twap = store.module("twap").unwrap();
        assert_eq!(twap.get("k").unwrap().as_deref(), Some(&b"v"[..]));
    }
    assert_eq!(stamp(&path), Some(3u32.to_le_bytes().to_vec()));
    assert_eq!(marker(&path).as_deref(), Some(&b"3"[..]));

    // `mark_two` refuses a second run, so a clean reopen proves that a
    // migrated store runs no step again.
    let store = LocalStore::open_as(&path, &v3).expect("reopen");
    assert_eq!(store.format_found(), FormatFound::Stamped(3));
}

#[test]
fn a_migration_starts_at_the_stamped_version() {
    let (_dir, path) = temp();
    drop(LocalStore::open(&path).expect("version 1"));
    drop(LocalStore::open_as(&path, &format(2, &[mark_two])).expect("to version 2"));
    assert_eq!(marker(&path).as_deref(), Some(&b"2"[..]));

    // From version 2 only the second step runs; `mark_two` would refuse.
    let store = LocalStore::open_as(&path, &format(3, &[mark_two, mark_three])).expect("to 3");
    assert_eq!(store.format_found(), FormatFound::Stamped(2));
    drop(store);
    assert_eq!(stamp(&path), Some(3u32.to_le_bytes().to_vec()));
    assert_eq!(marker(&path).as_deref(), Some(&b"3"[..]));
}

#[test]
fn an_unstamped_database_migrates_from_version_one() {
    let (_dir, path) = temp();
    legacy(&path, &[("twap", "k", b"v")]);
    let store = LocalStore::open_as(&path, &format(2, &[mark_two])).expect("adopt and migrate");
    assert_eq!(store.format_found(), FormatFound::Unstamped);
    drop(store);
    assert_eq!(stamp(&path), Some(2u32.to_le_bytes().to_vec()));
    assert_eq!(marker(&path).as_deref(), Some(&b"2"[..]));
}

/// The first step succeeds and writes; the second fails. Both the write
/// and the stamp share the abandoned transaction, so neither lands.
#[test]
fn a_failing_step_leaves_the_old_stamp_and_the_old_data() {
    let (_dir, path) = temp();
    {
        let store = LocalStore::open(&path).expect("version 1");
        store.module("twap").unwrap().set("k", b"v").unwrap();
    }
    let before = data_rows(&path);

    let err = LocalStore::open_as(&path, &format(3, &[mark_two, fail]))
        .expect_err("a failed step must fail the open");
    assert!(
        matches!(err, StorageError::Migration { from: 2, to: 3, .. }),
        "{err:?}",
    );
    assert!(err.to_string().contains("step refused"), "{err}");

    assert_eq!(stamp(&path), Some(1u32.to_le_bytes().to_vec()));
    assert_eq!(data_rows(&path), before, "a failed migration left a write");
    let store = LocalStore::open(&path).expect("the old engine still opens it");
    assert_eq!(store.format_found(), FormatFound::Stamped(1));
}

#[test]
fn a_stamp_no_engine_wrote_is_refused() {
    for bytes in [&b"\x01\x00\x00"[..], &0u32.to_le_bytes(), b""] {
        let (_dir, path) = temp();
        write_stamp(&path, bytes);
        let err = LocalStore::open(&path).expect_err("an unreadable stamp must not open");
        assert!(
            matches!(err, StorageError::FormatStampUnreadable),
            "{bytes:?}: {err:?}",
        );
    }
}

#[test]
fn the_metadata_table_is_invisible_to_module_reads_lists_and_quota() {
    let (_dir, path) = temp();
    let store = LocalStore::open(&path).expect("open");
    for namespace in ["m", "host/m", "nexum:store-meta"] {
        let ms = store.module(namespace).unwrap();
        assert_eq!(ms.get(FORMAT_VERSION_KEY).unwrap(), None);
        assert!(!ms.contains(FORMAT_VERSION_KEY).unwrap());
        assert_eq!(ms.len(FORMAT_VERSION_KEY).unwrap(), None);
        assert_eq!(ms.count("").unwrap(), 0);
        assert!(ms.list_keys("").unwrap().is_empty());
        let page = ms.list_entries(&ListQuery::default()).unwrap();
        assert!(page.entries.is_empty() && page.exhausted);
    }

    // Prefix, key, value and the fixed overhead of one entry: a quota of
    // exactly that admits the write only if the stamp is charged to nobody.
    let entry = 32 + "k".len() as u64 + "v".len() as u64 + 32;
    let ms = store.module("m").unwrap().with_quota(entry);
    ms.set("k", b"v")
        .expect("the stamp took quota from a module");

    // A module key spelt like the stamp lands in the data table.
    let squatter = store.module("squatter").unwrap();
    squatter.set(FORMAT_VERSION_KEY, &[0xff; 4]).unwrap();
    squatter.delete(FORMAT_VERSION_KEY).unwrap();
    drop((ms, squatter, store));

    assert_eq!(data_rows(&path).len(), 1, "the stamp is not a data row");
    assert_eq!(stamp(&path), Some(FORMAT_VERSION.to_le_bytes().to_vec()));
}
