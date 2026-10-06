# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions use
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.1] — 2026-10-06

**Fixes a write-blocking wall and several half-wired paths found by a full-tree
audit of v0.1.0** (report: `doc/全仓空壳审计_20261006.md`, Chinese).

### Fixed

- **Instances stopped accepting writes after ~180 statements.** Two causes,
  both "implemented but never called": segment growth did not extend the
  underlying file (`DataFile::extend` had no production caller), and the undo
  transaction table's 256 slots were never reclaimed (`write::reclaim` was only
  reachable from tests). Growth now extends the file by a fixed 8 MiB increment
  when an extent would fall beyond the current file length, and `Engine::begin`
  reclaims below the oldest snapshot when the slot table is full. Verified with
  2000 cross-process inserts.
- **Parameterized `INSERT` always failed on tables with a unique index** — the
  pre-write uniqueness check only accepted literals. It now accepts bound
  parameters, and parameters are pooled across a multi-statement batch
  (`BEGIN; INSERT … :p; COMMIT`).
- **`CREATE INDEX` failed on tables containing non-integer `NUMBER` values** —
  the DDL index build decoded rows into the catalog value model, which only
  holds integers. Keys are now built directly from the stored column bytes.
- **A member failure detected by the background writer never reached the
  control file** (the flag was set in shared state but only published when the
  *foreground* write failed, which could never happen again once the member was
  skipped), so `rebuild_member` silently reported "not damaged". A pending-
  publish flag now carries it to the next foreground publish.
- **The open path never ran the file/control-file consistency check**
  (`catalog::consistency` had no production caller, and `file_scn` was never
  advanced, so the check could never fire). Clean shutdown now advances
  `file_scn`, and `open` refuses to open when a file is *ahead* of the control
  file (mismatched copies) with a per-file finding.
- **`pctfree` and `itl_max` never took effect** (they were written to `tab$`
  and the segment header but the storage layer always used defaults). Verified
  black-box: with 800 rows, `pctfree = 50` uses 7 pages vs 4 for `pctfree = 0`.
- Index key columns silently degraded to *empty* on a row-cache miss (every row
  would compare equal — a unique index would then reject everything or nothing).
- `SegmentStore` swallowed page-read errors while enumerating index pages,
  silently shrinking fast full scans, `validate` and statistics.
- `Operator::rescan` silently defaulted to `Ok(())` (stateless assumption)
  while `RowCursor::rewind` reported a named error; the default is now a named
  `NoRescan`.
- Kernel tables `seq$` and `i_stat_pk` were assigned the same object number
  (`obj$` held two rows with the same `obj#`).
- REPL: a statement ending in a trailing line comment (`SELECT 1; -- note`)
  never executed; EOF silently dropped pending input. CLI: a failure to
  checkpoint at exit was swallowed when the session had already failed.
- Documentation that contradicted the code (WAL multi-member support marked
  "not implemented", the SQL crate's "binder/plan/session not landed" note, the
  executor README's reachability).

### Known limitations (recorded, with triggers)

- Wait/retry and deadlock detection are implemented but not wired into any
  production path yet (single-writer only today); `ExecEnv` carries no spill
  space, work-memory budget or cancellation source, so large sorts do not spill.

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
[0.1.1]: https://github.com/xuji755/bicdb/releases/tag/v0.1.1
