# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions use
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] — 2026-10-06

**First runnable version.** A single-workspace instance can be created on disk,
driven through SQL, killed at any moment, and reopened with recovery. Phases
P1–P4 are implemented; P5 (SQL / catalog) is in progress.

### Added

- **CLI (`bicdb`)**
  - `bicdb init <dir>` — creates a real on-disk instance: workspace dictionary
    file (`file0.dat`), undo segment (`undo.dat`), WAL group directory (`wal/`),
    and two copies of the control file (`cf_a`, `cf_b`).
  - `bicdb sql <dir> "<SQL>"` — executes one or more statements (`;`-separated,
    or `-` to read from stdin); `bicdb shell <dir>` — interactive shell.
  - Every command opens the instance through **three-phase crash recovery**
    (analysis → redo → loser rollback) and closes it with a **full checkpoint**
    (dirty pages written back, low-water mark published), so a killed process
    loses nothing that was committed.
- **Table access service (`bicdb-access`)** — page selection and growth, row
  write through the transaction engine, index-entry maintenance, tree-head
  persistence via page-diff redo; shared by the executor's DML path and the
  catalog's DDL path.
- **Catalog write side** — `CREATE TABLE`, `CREATE [UNIQUE] INDEX`,
  `DROP TABLE`, `DROP INDEX`, with the kernel tables `stat$`/`seq$` and the
  `object_id` sequence bootstrapped at instance creation; dictionary tables are
  ordinary tables (ITL / locks / undo / redo) with write-through row caching.
- **SQL front end (S1–S3, S5-lite)** — hand-written lexer and recursive-descent
  parser producing a PostgreSQL-shaped AST; binder with three-tier name
  resolution, type inference, parameter typing, write-target checks and version
  capture; direct physical plan mapping onto the `bicdb-exec` operator set; and
  a session that runs `SELECT` (projection, `WHERE`, `ORDER BY`, `LIMIT`),
  `INSERT … VALUES`, DDL, and `BEGIN` / `COMMIT` / `ROLLBACK`.
- **Uniqueness enforcement on `INSERT`** — checked before the row is written by
  fetching the candidate row back and recomputing its key byte-for-byte (the
  same shape PostgreSQL's `_bt_check_unique` uses); keys containing `NULL` are
  not treated as duplicates. Enforced inside explicit transactions too.
- **Index bulk load** (`bicdb-index::Tree::bulk_load`) — bottom-up index build
  used by `CREATE INDEX`.

### Fixed

- **Kernel table object numbers** — the object numbers of the keys of the
  kernel tables were derived as `reserved_obj + 1 + i`, which made `seq$` and
  `i_stat_pk` collide (two `obj$` rows with the same `obj#`). The reserved
  numbers are now passed explicitly.
- **Segment planner** — `plan_materialize_bitmap_page` read a stale header page
  inside its own extension loop, which could make large index builds loop
  forever; the in-flight overlay is now applied.
- **Explicit transactions** — statements used to commit themselves even inside
  `BEGIN`, so `ROLLBACK` was a no-op; the writer now respects transaction
  ownership from the session.
- **Sequence allocation** — `seq$` batches are flushed only on exhaustion
  (gaps are harmless), instead of one row update per allocation.

### Verified

- `cargo test --workspace` (all crates, including the new end-to-end suite),
  `cargo clippy --workspace --all-targets` with no warnings.
- End-to-end (`crates/cli/tests/e2e.rs`): create → DDL → DML → unique index →
  transactions → reopen; crash without a clean shutdown and recover 50 rows;
  unique-index `NULL` semantics.

### Known limitations

- `UPDATE` / `DELETE`, aggregates, joins, set operations, `DISTINCT`, table
  aliases, and the logical rewrite layer are not implemented yet (the binder
  rejects them by name — nothing is silently ignored).
- Index maintenance for `UPDATE`/`DELETE` lands together with those statements.
- The catalog's value model only carries integers; index keys are built
  directly from row bytes, so indexed columns are unaffected.
- Single workspace per instance; the daemon/protocol surface is not part of
  this release.

[0.1.0]: https://github.com/xuji755/bicdb/releases/tag/v0.1.0
