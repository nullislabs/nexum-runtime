//! The store format version: the stamp in the metadata table, and the
//! forward migrations that [`LocalStore::open`](crate::LocalStore::open)
//! runs to reach the current one.
//!
//! The version names the engine's own layout inside the redb file. It is
//! not a schema version for a module's data: a module versions its own
//! payload (ADR-0003, ADR-0026).

#![allow(clippy::result_large_err)]

use std::path::Path;

use nexum_runtime_api::BoxError;
use redb::{
    Database, DatabaseError, ReadOnlyDatabase, ReadableDatabase, TableDefinition, TableError,
    WriteTransaction,
};

use crate::local_store_redb::{StorageError, TABLE};

/// The on-disk layout this engine reads and writes.
pub const FORMAT_VERSION: u32 = 1;

/// Engine metadata. No module handle opens this table, so a module cannot
/// read it, list it, write it, or be charged quota for it.
const META: TableDefinition<'static, &str, &[u8]> = TableDefinition::new("nexum:store-meta");

/// The stamp: the format version as a little-endian `u32`. The table, the
/// key and the encoding never change, because an older engine has to read
/// a newer stamp in order to refuse it.
const FORMAT_VERSION_KEY: &str = "format_version";

/// One forward step, from format `n` to `n + 1`. It runs inside the
/// open-time write transaction that also lands the new stamp, so it must
/// not commit.
pub(crate) type MigrationStep = fn(&WriteTransaction) -> Result<(), BoxError>;

/// `MIGRATIONS[n - 1]` takes format `n` to `n + 1`. Empty while version 1
/// is current.
const MIGRATIONS: &[MigrationStep] = &[];

// One step per version below the current one, so every stamp an engine
// release wrote has a path forward.
const _: () = assert!(MIGRATIONS.len() + 1 == FORMAT_VERSION as usize);

/// What [`LocalStore::open`](crate::LocalStore::open) found on disk, before
/// it stamped or migrated anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatFound {
    /// No database, or one holding no store table: created at the current
    /// version.
    Fresh,
    /// A data table and no stamp: written before the stamp existed, and
    /// adopted as version 1.
    Unstamped,
    /// The stamped version.
    Stamped(u32),
}

/// A target version and the steps that reach it. Tests build their own to
/// exercise steps the engine does not ship.
pub(crate) struct Format<'a> {
    pub(crate) version: u32,
    pub(crate) steps: &'a [MigrationStep],
}

impl Format<'static> {
    /// The format this engine ships.
    pub(crate) const ENGINE: Self = Self {
        version: FORMAT_VERSION,
        steps: MIGRATIONS,
    };
}

impl Format<'_> {
    /// Open or create the database at `path` and bring it to this format.
    pub(crate) fn open(&self, path: &Path) -> Result<(Database, FormatFound), StorageError> {
        // A writable redb handle commits when it drops. A newer stamp is
        // therefore refused through a read-only handle, before a writable
        // one exists, so the refusal leaves the file byte-identical.
        if let Some(db) = open_read_only(path)? {
            self.refuse_newer(read_found(&db)?)?;
        }
        let db = Database::create(path).map_err(StorageError::Open)?;
        let found = read_found(&db)?;
        // Reached with a newer stamp only when redb had to repair the file
        // before anything could read it.
        self.refuse_newer(found)?;
        let from = match found {
            FormatFound::Stamped(version) if version == self.version => return Ok((db, found)),
            FormatFound::Stamped(version) => Some(version),
            FormatFound::Unstamped => Some(1),
            FormatFound::Fresh => None,
        };
        // One transaction carries the layout change and the stamp, so a
        // crash or a failed step leaves the previous version whole.
        let txn = db.begin_write().map_err(StorageError::Txn)?;
        match from {
            Some(from) => self.migrate(&txn, from)?,
            // A new database takes the current layout directly.
            None => {
                txn.open_table(TABLE).map_err(StorageError::Table)?;
            }
        }
        {
            let mut meta = txn.open_table(META).map_err(StorageError::Table)?;
            meta.insert(FORMAT_VERSION_KEY, self.version.to_le_bytes().as_slice())
                .map_err(StorageError::Storage)?;
        }
        txn.commit().map_err(StorageError::Commit)?;
        Ok((db, found))
    }

    fn refuse_newer(&self, found: FormatFound) -> Result<(), StorageError> {
        match found {
            FormatFound::Stamped(version) if version > self.version => {
                Err(StorageError::FormatTooNew {
                    found: version,
                    supported: self.version,
                })
            }
            _ => Ok(()),
        }
    }

    /// Run every step from `from` up to this version, in order.
    fn migrate(&self, txn: &WriteTransaction, from: u32) -> Result<(), StorageError> {
        let first = from.saturating_sub(1) as usize;
        for (step, from) in self.steps.iter().skip(first).zip(from..) {
            step(txn).map_err(|source| StorageError::Migration {
                from,
                to: from + 1,
                source,
            })?;
        }
        Ok(())
    }
}

/// A read-only handle on an existing database. `None` when there is no
/// database yet, or when redb must repair the file before it can be read.
fn open_read_only(path: &Path) -> Result<Option<ReadOnlyDatabase>, StorageError> {
    // A read-only open refuses the missing or empty file that
    // `Database::create` initializes.
    if !std::fs::metadata(path).is_ok_and(|file| file.len() > 0) {
        return Ok(None);
    }
    match ReadOnlyDatabase::open(path) {
        Ok(db) => Ok(Some(db)),
        // An unclean shutdown. Only a writable open repairs it, and no
        // table is readable until it does.
        Err(DatabaseError::RepairAborted) => Ok(None),
        Err(err) => Err(StorageError::Open(err)),
    }
}

/// One point read of the stamp, and a table lookup when it is absent.
fn read_found(db: &impl ReadableDatabase) -> Result<FormatFound, StorageError> {
    let txn = db.begin_read().map_err(StorageError::Txn)?;
    match txn.open_table(META) {
        Ok(meta) => meta
            .get(FORMAT_VERSION_KEY)
            .map_err(StorageError::Storage)?
            .and_then(|stamp| <[u8; 4]>::try_from(stamp.value()).ok())
            .map(u32::from_le_bytes)
            .filter(|version| *version != 0)
            .map(FormatFound::Stamped)
            .ok_or(StorageError::FormatStampUnreadable),
        Err(TableError::TableDoesNotExist(_)) => match txn.open_table(TABLE) {
            Ok(_) => Ok(FormatFound::Unstamped),
            Err(TableError::TableDoesNotExist(_)) => Ok(FormatFound::Fresh),
            Err(err) => Err(StorageError::Table(err)),
        },
        Err(err) => Err(StorageError::Table(err)),
    }
}

#[cfg(test)]
mod tests;
