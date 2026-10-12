---
status: accepted
amends: 0003-local-store-namespacing.md
---

# The local store carries an engine-owned format version

## Context

The local store is one redb file that holds one shared table ([ADR-0003](0003-local-store-namespacing.md)).
The file carried no marker of its layout.
An engine could not tell a file it wrote from a file that a later release wrote with a different layout.
It opened either one as its own, and a write on a layout it misreads destroys state without an error.
A layout change also had no place to run, because no step could know which layout it started from.

## Decision

The engine records a store format version in the database.
The version is a `u32`.
Version 1 is the layout of ADR-0003 and [ADR-0024](0024-blocks-are-clocks-and-host-keys-leave-the-module-namespace.md): the shared table `nexum:local-store`, keyed by the 32-byte namespace prefix and the raw key.

### The stamp lives in a metadata table

The stamp is the key `format_version` in a second table, `nexum:store-meta`.
Its value is the version as four little-endian bytes.
A module handle opens only the shared data table.
A module therefore cannot read, list, or write the stamp, and the quota scan never counts it.
This is the structural exclusion that ADR-0024 chose for host keys, taken one step further: the stamp is in no namespace at all.

The table name, the key, and the encoding are permanent.
An older engine must read a newer stamp to refuse it, so no later version may move or re-encode it.

### Unstamped means version 1

Every file written before this record has the data table and no stamp.
The engine adopts such a file as version 1, writes the stamp, and does not touch the data.
A file with neither table is new and gets the current version.
A metadata table whose stamp is absent, zero, or not four bytes is refused, because no engine writes that and the layout behind it is unknown.

### A newer version is refused

A stamp above the version the engine supports refuses the open with `StorageError::FormatTooNew`.
The error names both versions and tells the operator to run the newer engine or to restore a backup.
It is a boot refusal, and it carries the `error_kind` label `store_format_too_new`.

The engine reads the stamp through a read-only handle before it opens a writable one, because a writable redb handle commits when it closes.
The refused file is therefore byte-identical afterwards.
One case cannot hold that: a file left by an unclean shutdown is unreadable until redb repairs it.
redb's own recovery then runs before the refusal, and no table changes.

### Migrations are transactional and forward-only

`MIGRATIONS` in `crates/nexum-runtime-store/src/format.rs` is an ordered list, and step `n` takes version `n` to `n + 1`.
A compile-time assertion holds the list at one step for each version below the current one.
At open, the engine runs every step from the stamped version to the current one inside one write transaction, and it writes the new stamp in that same transaction.
A failed step or a crash abandons the transaction, so the file keeps the old version and the old data, and the next boot starts again.
No step runs backwards.
The path back is the backup the operator took before the upgrade.

The list is empty, because version 1 is the current layout.

Opening stays cheap.
The version is one point read, and a file at the current version opens without a write.

### What the version does not cover

- A module's own data layout.
  That stays with the module, which puts its own marker in its payload and migrates on `init` (ADR-0003).
  A per-module schema version is out of scope.
- The redb file format.
  A file that this redb cannot read fails as an open error, not as a format refusal.
- The durability model.
  [ADR-0014](0014-local-store-durability-model.md) is unchanged: the open-time transaction is the only write this record adds, and it commits before any module runs.

## Rejected alternatives

- **A reserved key in the shared data table.**
  The convention-only exclusion that ADR-0024 rejected for host keys: the stamp would share the key space that module handles scan.
- **A reserved namespace such as `host/format`.**
  No module name can reach it, but the stamp would then sit behind the prefix derivation, which is part of the layout that the stamp describes.
- **A sidecar file beside the database.**
  Two files do not commit together, so a crash or a partial restore leaves a stamp that disagrees with the data.
  A backup of the one database file would also lose its version.
- **Opening a newer file read-only, or on a best-effort basis.**
  The engine cannot know what a newer layout means, so a partial read is a wrong read.
- **Downgrade migrations.**
  Each one needs an older engine to know a layout that did not exist when that engine shipped.

## Consequences

- A release that changes the layout adds one step and raises `FORMAT_VERSION`.
  The new-database path must then create the new layout directly.
- A downgrade across a format change refuses at boot, where before it ran on a layout it did not know.
- The first boot on this release writes one stamp into each existing file.
  An engine from before this record does not read the stamp, so it still opens a version 1 file.
  It also cannot refuse a later version, so the refusal protects from this release on.
- `LocalStoreBuilder` logs the version once at boot, at INFO.
- The operator procedure is in `docs/production.md`, section 3.
