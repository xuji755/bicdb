# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions use
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Documentation — 2026-10-10

- Updated the English/Chinese project status to the v0.2.0 development line,
  including authenticated private workspaces, structure templates, native graph
  storage, the Cypher subset, property indexes, `GRAPH_TABLE`, graph full-text
  search and deferred index maintenance.
- Added a graph and full-text usage guide with runnable SQL examples and explicit
  limits. The README, manual, documentation index and Wiki now distinguish
  implemented behavior from later Neo4j/Cypher, TCP, parallel-execution and
  production-scale qualification work.

**DCL v0.2: dependency order FS → WORKSPACE → USER** — the management-statement
surface was redesigned (`doc/DCL语句设计_v0.1.md` v0.2) and its **syntax layer (D1)
landed** in `crates/sql`. Earlier in this cycle: full-repo audit (report
`doc/全仓审计_20261007.md`), drivers and a frozen local protocol
(`docs/客户端协议_v0.1.md`).

### Added

- **`IN (value list)` uses the index** — each value becomes a point lookup; the
  candidate list is carried by a single `IndexScan` (`points: [...]`) rather than
  by chaining one scan per value. That is not just tidiness: row de-duplication is
  per-operator, so chaining let a row reachable through two index entries — the
  stale entry left behind by a key-column update plus the new one — come out once
  per point. The e2e differential test caught exactly that (`UPDATE k=1→3` then
  `k IN (1, 3)` returned the row twice); with a single operator it returns once.
  Literal duplicates are collapsed at plan time; a `NULL` element never matches
  (three-valued logic) so it is dropped, and parameters are allowed (evaluated at
  run time, with the per-row de-duplication as the backstop). `NOT IN` is a
  negation, not a membership predicate, and stays a full scan.
- **`ORDER BY` takes expressions, including unprojected columns** — the spec's
  grammar has always been `ORDER BY <表达式> [ASC|DESC]`; the implementation only
  accepted output names/ordinals and refused everything else (a named refusal, so
  no silent wrongness, but `SELECT name FROM emp ORDER BY sal DESC` is the most
  natural thing to write). Keys that are output columns still sort *above* the
  projection; a key that is an input expression moves the sort **below** the
  projection, where the input row still exists — output-column keys are rewritten
  to their projection expression so a mixed list works
  (`ORDER BY g DESC, x DESC` with only `x` projected).
  - Under aggregation the sort scope is the aggregate output row, so a *group
    column* can be ordered even when it is not projected (`SELECT COUNT(*) FROM t
    GROUP BY g ORDER BY g`); a non-grouped column is still refused by name.
    `DISTINCT` plus an expression key is refused too (the keys must be in the
    select list — PostgreSQL's rule).
- **Fixed table `file$` is queryable** — the first introspection surface: the data
  file list of the current workspace, whose rows come from the **control file**
  (the single authority — the design deliberately does not keep a second copy in
  the dictionary). It works like any other row source (predicates, ordering,
  aggregates, aliases, joins), and it is read-only *by construction*: the write
  target namespace has no fixed-table tier, so `INSERT INTO file$` answers
  "no such object", exactly like a table that never existed. On the `public`
  workspace it is **management metadata**: a named subject sees "no such object"
  too (no "permission denied" leak), while the management identity sees the rows.
  - This needed a piece that was specified but never produced: **nothing ever
    wrote the control file's data-file records** (only tests constructed them),
    so `file$` would have been empty on every real instance. The workspace's
    files are now registered at creation, and **idempotently back-filled at open**
    (so instances created before this change get their rows too) — written during
    assembly, before `GroupWriter` takes over, which keeps the "control file has a
    single writer" discipline intact.
  - The SQL side is a port (`FixedTableSource`) implemented by the CLI reading
    `<workspace>/control/`; the catalog layer stays free of control-file handles.
- **Bounded index range scans** — `BETWEEN a AND b`, `>= a AND <= b`, `> a AND < b`
  and friends now use the index when the column is the sole key of a single-column
  index; the operator carries open/closed ends (the tree's `range` is closed-only,
  so an excluded endpoint has its entries dropped during collection — byte
  equality, no successor arithmetic). **One-sided bounds stay full scans**: index
  entries are collected into memory per scan, so `k > 5` would pull the whole tree
  in; that waits for the streaming-cursor work.
  - This is the last of Oracle's RBO-ranked index paths for a single table
    (rank 10, bounded range, right after unique/plain equality).
  - **The range case exposed a real defect in the entry model**: because index
    entries are insert-only, changing a key column appends a second entry while
    the old one stays — so *one live row can have two entries*. Equality lookups
    never notice (only one key matches), but a range covering both old and new
    keys returned that row **twice** (caught by the differential e2e test:
    `UPDATE k=1→3` then `BETWEEN 1 AND 3` returned the row twice). Index scans now
    de-duplicate by the **resolved (post-forwarding) physical ROWID**, which is
    also what makes the forwarding case correct. Pinned by an executor unit test
    that fails with `["1","2","2","1"]` if the de-duplication is removed.
- **Index nested-loop joins (IndexNL)** — when the inner table of a two-table
  join has a single-column index on its join column, the inner side is now an
  `IndexScan` parameterised by the outer row (`NestedLoop`'s `inner_params` were
  built for exactly this) instead of a full rescan per outer row. The predicate
  is taken from the `ON` clause for inner and left joins, and from the `WHERE`
  clause for inner joins (for left joins the `WHERE` form is left alone — it is
  sound but needs its own argument, recorded).
  - Same rule-based ranking as the single-table case (Oracle's RBO puts
    "single-row access via unique/primary key" at rank 2/3 and a per-outer-row
    full scan at 15) and the same correctness stance: the index only narrows
    candidates, the join condition is still re-evaluated on the combined row, so
    stale entries are rejected by the row itself.
  - Pinned by a differential test: the same join with the index and after
    `DROP INDEX` (full rescan) must agree row for row, including duplicate keys,
    a stale entry produced by updating an indexed (non-unique) key column, and
    left-join null padding. Measured on the demo (1,000 × 1,000 self-join):
    33 ms vs 462 ms, identical results.
- **On-demand checkpoints under log pressure** — redo groups rotate, and a group
  can only be reused once it has been *demoted* (`ACTIVE → INACTIVE`), which
  happens when a checkpoint advances past its end. Nothing in the running
  service ever advanced one, so any workload that produced more than about two
  groups of redo simply stopped: the 40th insert into a 32 KiB-group instance
  failed with "log switch waiting for a checkpoint (no reusable group)" — with a
  4 MiB-group instance that is a bulk load dying around 6,500 rows.
  - The fix is the design's own CKPT trigger ② ("group full, forced"), which is
    Oracle's *log switch triggers a checkpoint* and PostgreSQL's `max_wal_size`
    trigger: before each statement the session asks (read-only) whether the next
    switch would block, and if so publishes a full checkpoint right there.
    Trigger ① (the ~3 s periodic publish) still belongs to the background-role
    work — `crates/daemon` is not wired in yet (audit R1).
  - Only at a statement boundary with no explicit transaction open (the same
    discipline `Instance::shutdown` follows); inside a long explicit transaction
    the log can still block, and the next statement after `COMMIT` clears it.
  - Pinned by `crates/cli/tests/e2e.rs::log_pressure_triggers_a_checkpoint_so_bulk_loads_keep_going`,
    which fails with `Txn(Log(Blocked(AwaitingCheckpoint)))` when the hook is
    removed. Measured: 32 KiB-group vs 4 MiB-group instances load at the same
    rate (232 vs 239 rows/s, within process-startup noise), so the forced full
    checkpoints are not the bottleneck at this pool size.
- **Index access paths (rule-based, equality only)** — a single-table `SELECT`
  whose `WHERE` contains `column = constant` (or `= :param`) where that column is
  the sole key of a **single-column index** now runs as an `IndexScan` instead of
  a full table scan; a unique index is preferred over a non-unique one (Oracle's
  RBO ranks unique-key single-row access above single-column index access, which
  in turn ranks above a full scan). Everything else — ranges, `OR`, `IS NULL`,
  multi-column keys, column-to-column comparisons — stays a full scan for now.
  - No cost model, deliberately: with no statistics (`ANALYZE`) any "cost" would
    be a made-up constant, which is exactly the bind-variable misestimation the
    KB documents; rule-based selection is the mature answer until statistics
    land.
  - **The index only narrows candidates, it never decides correctness**: index
    entries are insert-only (a key change appends, it does not move) and carry
    no visibility information (PostgreSQL's reason for re-checking the heap even
    on index-only scans), so the scan always fetches the row and the `Filter`
    re-evaluates the predicate. Two consequences are pinned by tests: the scan
    takes **no `limit`** (the first entry for a key may be a stale one, with the
    live row behind it) and a bound expression that evaluates to `NULL` yields
    **no rows** rather than degenerating into an unbounded index scan.
  - Fixes audit item **R4** while landing: `IndexScan` compared bounds built with
    the *bare* column encoding against tree keys built with the composite
    `key::encode` framing — different prefixes, so a wired-up scan would have
    silently returned the empty set. The operator now uses the composite form
    (single-column shape) and reads the tree root from the index segment's header
    page at scan time instead of trusting a plan-time root.
  - Measured on the demo instance (6,639-row table, same connection): indexed
    equality 0.6 ms median vs 2.7 ms for the equivalent full scan.
- **Authentication (D6, local part)** — a new `AUTH` verb on the local protocol
  (spec `docs/客户端协议_v0.1.md` §4.3, byte-frozen in both implementations):
  a subject name plus a password verified **server-side** against
  `public.user$`'s PBKDF2-SHA512 hash. **No credentials = the management
  identity** (the local/OS identity, i.e. whatever the socket's file
  permissions let in), so every existing client keeps working; a named subject
  is **read-only on `public`** and may only change **its own** password
  (`ALTER USER … IDENTIFIED BY … REPLACE …`, the Oracle shape for self-service).
  - **Failure semantics follow measured practice, not intuition** (evidence pack
    `doc/evidence/auth-20261007/`): the password check comes **before** the
    status check, "no such subject" and "wrong password" produce **one**
    message, and the unknown-subject path still runs **one PBKDF2 of the same
    cost** (discarded) so response time does not reveal existence — PostgreSQL's
    *mock authentication*. `PAUSE` refuses new sessions (open ones are not
    killed); `EXPIRE` admits the session but **restricts it** to its own
    password change, which clears the restriction in place (MySQL's
    "password expired" restricted-login mode). Passwords never enter the log,
    and no challenge/response is built on the stored hash — that would make it a
    password-equivalent (the PG MD5 failure mode the KB documents).
  - `AUTH` must be the **first business request** on a connection (running SQL
    first and then "becoming" someone would be privilege laundering), and the
    payload carries **no user id** — identity only ever comes from
    authentication (`REQ-ISO-002`).
  - Clients: `bicdb sql -U <subject>`, `bicdbcli -U <subject>` (password from
    `BICDB_PASSWORD` or a `/dev/tty` prompt — never a command-line argument),
    Rust driver `Connection::connect_as(…)`/`login(…)`/`user()`, Python driver
    `bicdb.connect(target, user=…, password=…)`.
  - Two defects the new tests caught while landing this: the Python driver's
    `connect(user=…)` leaked the socket when authentication failed (the service
    serves one connection at a time, so every later connection got "instance
    busy"), and the Python exception classifier matched `唯一` before checking
    for "not implemented", so `UPDATE` of a unique-key column — refused by name,
    not a uniqueness violation — came back as `IntegrityError` instead of
    `NotSupportedError`.
  - Not done (recorded): network transport + channel binding, routing an
    authenticated session to the subject's **own workspace** (the service is
    still one workspace per service), an admin subject inside `user$`, and audit
    trails for password resets.
- **Set operations, `SELECT` without `FROM`, and `INSERT … SELECT`** —
  `UNION [ALL]` / `INTERSECT [ALL]` / `EXCEPT [ALL]` (left-deep, both sides
  must be `SELECT`s of equal width; the outer `ORDER BY`/`LIMIT` applies to the
  merged result), `SELECT 1` (a new one-row, zero-column `SingleRow` source in
  the executor), and `INSERT … SELECT` where the source is **materialised**
  first and then inserted as literal rows — so the uniqueness pre-check and
  index maintenance are reused unchanged. The parser now accepts `SELECT` as an
  insert source (`DEFAULT VALUES` still refused: no column defaults yet).
  - Two more silent-wrong-answer defects were caught by the new tests while
    landing this: the uniqueness pre-check read `plan.node` instead of the
    materialised node (so `INSERT … SELECT` skipped it and wrote duplicates),
    and `shift_sources` (the source-id offsetting used when a plan embeds
    another plan) did not recurse into aggregates/joins/unique/set-op nodes, so
    `INSERT INTO t SELECT COUNT(*) FROM big` counted **the target table**.
- **Two-table joins** (`JOIN … ON`, `LEFT [OUTER] JOIN`, comma joins, table
  aliases, qualified `t.col` references) — bound against a combined row scope
  that rejects **ambiguous** unqualified names instead of guessing, and planned
  as a `NestedLoop` over two `SeqScan`s. Each source now gets its own scan
  cursor (the session previously opened the *first* source for every id, which
  nothing noticed while only single-table scans existed). Three or more tables
  and nested joins are refused by name for now.
- **`UPDATE` and `DELETE`** (single table, with `WHERE`) — the exec operators
  existed but the SQL path did not; now the binder, planner and session run them
  (`PlanKind::{Update,Delete}`, `WithRowId` source, index maintenance).
  - **The DML source is materialised first** (rows matching the statement
    snapshot are collected, then written): scanning while writing would let the
    same cursor see its own changes (a row migrated to a page not yet visited
    could be updated twice). Filtering happens during materialisation, so the
    predicate is evaluated on one consistent row set.
  - **Index maintenance for `UPDATE`/`DELETE`** is now implemented
    (`IndexMaintenance::after_update/after_delete` + `TableWriter` receiving the
    old row bytes). Updating a **unique key column** is refused with a pointed
    message (the write-ahead uniqueness check covers `INSERT` only).
  - **Two real defects found and fixed while landing this**, both of them
    silent-wrong-answer class:
    1. **Rollback lost index entries.** `arch/09` §9.1.2 says index entries are
       removed with the deleting transaction, but index page writes carry redo
       only (no undo) — so `BEGIN; DELETE; ROLLBACK` restored the row and left
       the index **missing an entry for a live row** (uniqueness then let a
       duplicate in). Index maintenance is now **insert-only** — the PG model:
       stale entries are filtered by the row itself ("fetch the row at that
       RID, recompute the key, compare bytes"), which is how the read paths
       already worked. Entries are reclaimed by `DROP INDEX`/rebuild; removing
       them at delete time needs undo for index writes (recorded).
    2. **Batch fetch did not follow forwarding pointers.** After a growing
       update migrated a row, the index entry (stable entry RID) no longer
       pointed at a physical row — `scan::fetch_rows` returned `None`, so the
       uniqueness check concluded "no live row with this key" and **allowed a
       duplicate insert** (caught by the new e2e test). Batch fetch now resolves
       the forwarding chain first.
- **DCL execution: user lifecycle (U group) runs end to end** —
  `CREATE USER <name> IDENTIFIED BY '<pw>' USING WORKSPACE <ref>` writes the
  principal into `public.user$` with a **PBKDF2-HMAC-SHA512** hash and binds the
  workspace in the same action (the workspace must be owner-less: *someone
  else's workspace has no entry point*). `ALTER USER` covers admin password
  reset (`IDENTIFIED BY '<new>' [EXPIRE]` — the old password dies immediately),
  self-service change (`REPLACE '<old>'`, verified against the stored hash),
  `PAUSE`/`RESUME` (≈ Oracle `ACCOUNT LOCK`, refusing new sessions only),
  `USING WORKSPACE` (bind another) and `DROP WORKSPACE` (unbind; the **last**
  workspace cannot be unbound). `DROP USER [CASCADE]` refuses with the workspace
  list unless `CASCADE`, which runs the reverse three-step drop for each owned
  workspace.
  - **PBKDF2-HMAC-SHA512 implemented in-crate** (`crates/common/src/sha512.rs`,
    `pbkdf2.rs`): SHA-512 with FIPS vectors, HMAC-SHA512, PBKDF2 with RFC-style
    vectors cross-checked against an independent implementation. The stored
    form is self-describing — `pbkdf2-sha512$<iters>$<salt-b64>$<hash-b64>` —
    so raising the iteration count never breaks old rows. Comparison is
    constant-time; there is **no "read back the password" path**, and the hash
    has exactly one reader (the authentication path).
  - The iteration count is a real instance parameter now:
    `[auth] pbkdf2_iterations` (default 210000, OWASP 2023 scale for
    PBKDF2-HMAC-SHA512).
- **DCL execution: workspace lifecycle (W group) runs end to end** —
  `CREATE WORKSPACE` now builds a **complete, independent workspace file set**
  (`<BICDB_HOME>/<name>/`: parameter file · `control/` · `wal/` · `data/`),
  registers it in `public.ws$` (owner-less) and, **last**, in the instance
  registry — the three-step protocol of `arch/02` §2.11 with visibility as the
  single switch. The result is a real library of its own: `bicdb start -p <name>`
  serves it with its own dictionary, WAL and control file. `ALTER WORKSPACE`
  covers `ADD FILESYSTEM`/quota (into `wq$`), `SET DEFAULT FILESYSTEM` (a new
  `ws$.default_fs` column), `SET NAME` (delete+insert: the name is a unique key)
  and `SET QUOTA`. `DROP WORKSPACE` runs the reverse order (visibility first,
  then the `ws$` tombstone, then the directory) and **pre-checks** first, so a
  foreseeable refusal (directory held by a live instance) leaves no half-dead
  state. `public` is reserved and cannot be dropped; the pool must be non-empty
  before a workspace can be created. The step that needs instance-level
  assembly is reached through a **port** (`WorkspaceProvisioner`, implemented by
  the CLI over `boot`), keeping `bicdb-sql` free of CLI dependencies.
- **DCL execution: filesystem pool (F group) runs end to end** —
  `CREATE FILESYSTEM <name> USING '<path>'`, `ALTER FILESYSTEM … SET ALLOCATE =
  ON|OFF` and `DROP FILESYSTEM …` now really execute (`crates/sql/src/dcl_exec.rs`),
  writing **two places in a fixed order**: the registry (`<BICDB_HOME>/control/`
  global control file, two copies) and the `public.fs$` row — attributes first,
  visibility last, mirroring the workspace protocol. Management statements run
  **only on the PUBLIC workspace** (eligibility is checked before object lookup);
  the path check requires an existing, writable, **non-symlink directory**
  (writability is probed with a real file — permission bits lie under ACLs);
  `DROP FILESYSTEM` refuses while any registered workspace lives under that path.
  Capacity caching (`total_bytes`/`free_bytes`) deliberately stays 0/None: the
  authority is the filesystem itself, so we do not invent a number.
- **Management-plane dictionary, v0.2 shape** (`crates/catalog/src/dict.rs`):
  `ws$` now has a **nullable `user_id`** (a freshly created workspace is an
  owner-less container until `CREATE USER … USING WORKSPACE` binds it) and a
  **NOT NULL, instance-unique `name`** (at creation time there is no owner to
  qualify it by); `fs$` gains a **`name`** column (the identifier — `mount_path`
  is renamed `path`, since the path is only a creation argument) plus
  **`allocate`** for the drain valve, and now has unique indexes on *both* name
  and path; `wq$` (workspace × filesystem × quota) is new. Public workspaces
  therefore bootstrap **27** entries (was 23); ordinary workspaces stay at 15.
- **DCL dictionary write layer** (`crates/catalog/src/dcl.rs`): insert/find/
  update for `fs$`/`ws$`/`wq$`/`user$`, each action being one DDL transaction.
  `DictWriter` gains `update_dict_row` (key columns must not change — checked),
  `delete_dict_row` and, on insert, a **unique-key pre-check**: the index layer
  deliberately does not enforce uniqueness (slot reuse would produce false
  conflicts), so the check fetches the candidate row and compares recomputed key
  bytes — the same rule the SQL DDL path uses.
  Password hashes have exactly one reader (`password_hash`, for authentication)
  and never appear in the metadata view.
- **Workspace identity is now `workspace_ref = SHA-256(workspace_id)` (first 8
  bytes)** — `crates/common/src/sha256.rs` (in-crate, FIPS 180-4 vectors, no
  third-party dependency; the "SHA-256 or BLAKE3" P0 open item is closed with
  SHA-256). Data-file names are `<ref-hex>_<role>` and the ref is stamped in
  each file header, so **file name and file header share one source**: opening
  reads the workspace id from the control file and derives the names from it.
  The old fixed `bicdb001` naming is not migrated (documented: rebuild) and the
  error message says so.
- **The PUBLIC workspace is now the management plane** — `<BICDB_HOME>/public`
  is created with `is_public = true`, i.e. it carries the `user$`/`ws$`/`fs$`
  bootstrap tables (23 bootstrap entries); ordinary workspaces do not.
- **`scripts/demo.sh` now deploys a real install shape**: `~/bicdb/demo` is a
  whole `BICDB_HOME` (`app` · `control` · `public` · `log` · `backup`), so the
  registry and the management plane are actually exercised there.
  `BICDB_HOME=<new dir> bicdb init` also creates the root now
  (`Home::locate_for_init`), instead of requiring a prior `mkdir`.
- **Global control file** — the instance-level registry (`<BICDB_HOME>/control/`,
  two copies), implemented in `crates/storage/src/globalctl.rs` and wired into
  `bicdb init` / `bicdb list` / `bicdb home`. Design: `doc/全局控制文件设计_v0.1.md`
  (KB evidence: `doc/evidence/global-cf-20261007/`).
  - **Same physical format and update protocol as the workspace control file**
    (20 pages × 16 KiB, two copies, single-interval undo publish, crash
    self-heal): the shared core was extracted as `CfCore` inside
    `controlfile.rs` — all 189 existing workspace-control-file tests pass
    unchanged, public API and byte layout untouched.
  - The page-header `flags` byte now carries a **kind**; opening a workspace
    control file as a global one (or vice versa) fails immediately instead of
    reading plausible-looking bytes.
  - Layout: page 1 holds a 64 B library entry (random 16 B `library_id` — an
    identifier, **not a key**) plus 48 B counters; pages 2–11 hold 580 × 280 B
    workspace records (`workspace_id`, status, root path); pages 12–19 hold
    384 × 336 B pool records (slot, status, `ALLOCATE` flag, name, path).
  - **"A workspace exists" has exactly one witness: an active record here**
    (`arch/02` §2.12). Slots are allocated monotonically and never reused, so a
    tombstone keeps pointing at the same disk; allocation always scans for an
    empty slot (the counters are informational — never a second source of
    truth). `CREATE` order is files → `public.ws$` → **this file last**.
- **DCL syntax, v0.2 (D1 landed; execution still pending D2/D2.5/D3/D5/D6)** —
  `crates/sql/src/{lexer,ast,parser}.rs`:
  - `CREATE FILESYSTEM <name> USING '<path>'` / `ALTER FILESYSTEM <name>
    SET ALLOCATE = ON|OFF` / `DROP FILESYSTEM <name>` — a three-piece set
    isomorphic with WORKSPACE and USER. **The name is the identifier; the path
    is only a creation argument**, and a "filesystem" may be a plain directory
    (validation: exists, writable, a directory, not a symlink — *not* required
    to be a mount point).
  - `CREATE WORKSPACE <name> [DEFAULT FILESYSTEM <fs>] [FROM TEMPLATE '<t>']
    [QUOTA <n>|UNLIMITED ON FILESYSTEM <fs>]…` — builds an **owner-less
    container**; the owner is bound only by
    `CREATE USER <name> IDENTIFIED BY '<pw>' USING WORKSPACE <ref>` (binding is
    mandatory: a user without a workspace has nowhere to put data).
  - `ALTER WORKSPACE … ADD FILESYSTEM / SET DEFAULT FILESYSTEM / SET NAME =
    '<n>' / SET QUOTA (…) / TO TEMPLATE '<t>'`; `DROP WORKSPACE <ref>`;
    `ALTER USER … IDENTIFIED BY '<new>' [EXPIRE] | … REPLACE '<old>' | PAUSE |
    RESUME | USING WORKSPACE | DROP WORKSPACE`; `DROP USER <name> [CASCADE]`;
    `ALTER DATABASE ADD|DROP TEMPLATE` (clone and in-place templating each have
    exactly one spelling).
  - **Removed**: `CREATE WORKSPACE FOR USER …` (two places setting the owner is
    an inconsistency; MySQL 8.0 dropping `GRANT … IDENTIFIED BY` is the same
    lesson) and `ALTER SYSTEM ADD|ALTER|DROP FILESYSTEM`. Old spellings now
    produce **pointing error messages** naming the replacement.
  - The name space had to be tightened as a consequence: a workspace has no
    owner at creation time, so **`ws$.name` must be unique instance-wide** —
    the whole `FOR USER` qualifier / "name defaults to the owner's name" family
    is gone. `QUOTA` hangs off the *workspace* (`ws$` role columns + `wq$` per
    filesystem), so the planned user-level `uq$` and `user$.default_fs` were
    dropped (one concept, one place).
  - Binder now rejects F/W/U/T with the owning slice and restates the ordering
    (`FS → WORKSPACE → USER`). Docs synced: `doc/spec/SQL.md` (REQ-SQL-005,
    frozen item 48), `doc/SQL前端设计_v0.1.md` §3.1, `doc/arch/02-工作区存储布局.md`,
    `doc/用户与配额管理设计_v0.1.md` (v0.2), `docs/使用手册.md` §4.2/§5/§8.6.

- **Install tooling: one root, four directories** (`scripts/install.sh`,
  `scripts/uninstall.sh`, `doc/安装布局_v0.1.md`). `<BICDB_HOME>` (default:
  `/u01/app/bicdb` → `/opt/bicdb` → `$HOME/bicdb-home`, always a visible
  directory — two earlier cuts used a hidden `$HOME/.local/...` and an
  over-layered OFA `BASE/product/<version>` tree; both were too complex) holds
  `app/` (programs + share, read-only — upgrading replaces just this),
  `public/` (the PUBLIC workspace: `bicdb.ini` · `control/` · `wal/` · `data/`),
  `log/` (all database logs, named per workspace — **one place to look**) and
  `backup/` (default backup destination). Reinstalling never touches `public/`;
  uninstall keeps data unless `--purge` (and refuses while a workspace runs).
  There is deliberately **no `template/`**: the parameter-file template lives in
  the binary (`bicdb init` renders it), so a loose copy would be a second
  source of truth.
- **Workspace directory structure settled** (`doc/安装布局_v0.1.md` §2,
  `crates/workspace/src/layout.rs`): three subdirectories —
  `control/` (the recovery anchor: you must know which files to open before you
  can open any file), `wal/` (circularly reused, different lifecycle from data)
  and `data/` (grows only; **the file role is in the file name**:
  `<ws>_meta` / `_undo` / `_temp` / `_data_NN`). The earlier ten-directory list
  (catalog/undo/index/tmp/…) conflated *file roles* with directories; it is now
  reconciled in code and docs, and `assets/`/`staging/` are documented as living
  in the filesystem pool, not under a workspace root.
- **Name-based addressing and discovery** (`crates/cli/src/home.rs`):
  `BICDB_HOME` resolves from the environment, else from the binary's path shape
  (`<home>/app/bin/bicdb` — a dev tree is not mistaken for an install).
  `bicdb init` (no argument) creates `<BICDB_HOME>/public`; `bicdb init <name>`
  creates an extra workspace root for testing; `bicdb start -p public` resolves
  the name when it is not a real path; `bicdb list` shows every workspace with
  its state, and the service log defaults to `<BICDB_HOME>/log/<name>.log`.
  Commands: `bicdb home` (the four paths + version), `bicdb list`.
- **`scripts/demo.sh` — a local test environment** at `~/bicdb/demo`: create /
  start / stop / status / reset / cli / sql / py, with seeded data (`t` 3 rows,
  `emp` 4 rows, `big` 1000 rows, unique indexes, one table with `pctfree`/`itl_max`).
  The instance directory is git-ignored (`/demo/`); the script is tracked so the
  environment is reproducible on any machine.

### Added

- **User / quota / filesystem-allocation design** (`doc/用户与配额管理设计_v0.1.md`,
  evidence pack `user-quota-design-20261007`) — the gap the manual surfaced is now
  specified: `CREATE USER … IDENTIFIED BY … [DEFAULT FILESYSTEM] [QUOTA … ON FILESYSTEM]`,
  `ALTER USER … PAUSE|RESUME` (Oracle `ACCOUNT LOCK|UNLOCK`: new sessions refused,
  open ones not cut), quota and default-filesystem changes, `DROP USER [CASCADE]`
  (refused while the user still owns a workspace), and **password handling**: admin
  initialises and **resets** passwords (`IDENTIFIED BY … [EXPIRE]`, audited) while the
  stored `passwd` is a PBKDF2-SHA512 hash — **nobody, admin included, can read a
  password back**; "recovery" means resetting it. Model: `user$` gains `default_fs`,
  new `uq$` (user × filesystem × bytes) sits beside `ws$`'s four quotas as the second
  quota level. Principle fixed by the user: **admin administers users but never sees
  user data** — no entry to other workspaces (not even for admin), metadata-only
  introspection, hashes never readable, diagnostics owner-authorised.

### Changed

- **User manual rewritten as a task-ordered reference** (`docs/使用手册.md`),
  starting with a **capability table** (what works ✅ / what does not ⛔ and which
  slice owns it) so nothing is documented as usable before it is. New chapters:
  install, create-database, storage & the filesystem pool (designed, not
  implemented), users & workspaces (ditto — plus a recorded gap: there is no
  `CREATE USER` statement anywhere in the DCL design), connecting, tables (all
  five designed table types with only type 1 implemented), a **verified SQL
  syntax reference** (every claim probed against a live instance), parameters,
  commands, operations, drivers. Writing it surfaced two doc-vs-code drifts that
  are now fixed: the manual used to claim `DATE`/`TIMESTAMP`/`UUID`/`RAW` column
  types and `CASE WHEN`/`ORDER BY <any column>` were supported.
- **Unwired datetime types are refused instead of mistyped**: `TIMESTAMP` was
  accepted by DDL but stored as a plain number (`crates/types/src/datetime.rs`
  has the 7-byte DATE / 11-byte TIMESTAMP encodings and **no consumer**), so
  `'2026-01-02 03:04:05'` failed while `20260102` succeeded. `DATE`,
  `TIMESTAMP`, `TIMESTAMPTZ`, `UUID`, `RAW`, `JSON` and `VECTOR` are now named
  refusals pointing at the TYP slice.
- **Parameter file is declarative now** — one table (`SPECS` in `crates/cli/src/config.rs`)
  declares every key (section, effect, default, doc); the closed set, the generated
  `bicdb.ini`, `bicdb params` and key lookup all derive from it. Adding a key used to
  mean editing five places; it is now two (the table plus one setter arm).
- **39 keys across 12 sections** (up from 12 across 6). New:
  `[buffer]` hash_buckets, bucket_latches, hot_fraction, touch_interval_ms, cool_count,
  stay_count, hot_criteria, max_scan_fraction, make_free_batch_divisor;
  `[storage]` multiblock_read_pages (one value for heap scan *and* index FFS — the two
  8-page constants are now shared), cr_max_rounds;
  `[wal]` log_buffer_pages, flush_trigger_permille;
  `[catalog]` row_cache_rows, row_cache_bytes, rid_forward_max_hops;
  `[index]` bulk_fill_percent;
  `[service]` start_wait_s, stop_wait_s, ready_poll_ms, stop_poll_ms, probe_timeout_ms,
  status_timeout_ms, log_tail_lines;
  `[client]` handshake_timeout_ms, request_timeout_ms;
  `[lock]` wait_max_rounds.
- Range checks happen at parse time and refuse by name (no silent clamping), and the
  setters at instance open refuse too — a key that cannot take effect must not be accepted.

### Fixed

- **A failed statement rolled back the whole transaction.** In an explicit transaction
  `BEGIN; INSERT ok; INSERT fails; COMMIT;` destroyed the earlier insert and left
  `COMMIT` answering "no active transaction" — the handle was dropped. Errors in an
  explicit transaction now use the engine's **statement rollback** (`statement_mark` +
  `rollback_statement`: no lock release, earlier statements stay); autocommit keeps the
  full rollback. (Regression: `cli/tests/e2e.rs`.)
- **DML error paths swallowed rollback failures** (the 2026-10-06 fix covered DDL only):
  a failed rollback would leak row locks/ITL/undo silently. `exec/dml.rs` (all three
  operators) and `sql/session.rs` now report both errors (`ExecError::RollbackFailed`).
- **`stop` parsed `-w` and threw it away**; `start`/`stop` wait limits were the only
  run-time values with no parameter-file key. Now `service.start_wait_s` /
  `service.stop_wait_s` with `-w` as the override.
- **Silently ignored table options**: `WITH (table_type = append_only)`,
  `logging = redo_only|none` and `retention = n` were accepted, stored in `tab$`, and
  never read — you got a plain table with no warning. They are now **named refusals**
  at bind time; `transactional|heap`, `pctfree` and `itl_max` keep working.
- **`pctfree`/`itl_max` were silently truncated** (`as u8`: 300 → 44, 256 → 0). Now
  range-checked with named errors (0–99 / 1–32).
- **ITL entries marked committed with an empty sequence** were decoded as "committed at
  seq 0" ⇒ visible to *every* snapshot. They now decode as "unknown" and fall back to
  the transaction table (the authority), like `Active` entries do.
- **`db_check`/`page_dump` read the whole image into memory** and printed unbounded
  output — the diagnostics tool OOM-ing on the database it is diagnosing. Both stream
  page by page now; `db_check --max-findings N` (default 1000) caps the *report* while
  verdict and totals stay exact.
- **Nested scripts had no depth limit** (`@self.sql` → stack overflow). Now a named
  refusal at depth 32.
- **`bicdbcli -s <socket>` was documented but not implemented** — implemented (and
  mutually exclusive with `--direct`, reported by name).
- **Default-value contradictions**: `ArchiveRecord::default()` said archive-on while both
  production paths write no-archive (now aligned to no-archive; the three archive-gated
  WAL tests opt in explicitly); `lock.deadlock_threshold_ms` parameter defaulted to 1000
  while the engine constant is 3000 (now sourced from `WaitPolicy::default()`);
  `InsertPolicy::default()` reserved 10% while every production path used 0 (aligned).
- Small bounds: `Page::set_free_end` clamped into the page-tail area (now clamps to the
  row-area floor); `Page::set_slot` lacked the directory-overflow guard `slot()` has;
  the DDL index-build ordinal could underflow on a corrupt `col#`; `Delete` did not mark
  itself done on error.
- Stale docs: the previous audit called `crates/net` a 10-line skeleton (it is the real
  protocol layer now); `bind/mod.rs` claimed type inference was not implemented (it is);
  `latch.rs` said spin default 40 while the code uses 8.

### Added

- **`bicdb-net` (`crates/net`)** — the local client protocol, implemented:
  length-prefixed binary-safe frames (`VERB\n<len>\n<payload>`, `OK|ERR` replies),
  a wire value model (`NULL` / decimal-text number / bool / **hex byte string**),
  the verb set `HELLO | STATUS | SQL | DESCRIBE | SHUTDOWN`, and a client
  (`Client`) with version negotiation and `BICDB_TRACE` protocol tracing
  (PG `PQTRACE` style). The previous ad-hoc `crates/cli/src/wire.rs` is gone.
- **Protocol v0.1 spec** ([docs/客户端协议_v0.1.md](docs/客户端协议_v0.1.md),
  Chinese) — the single document the Rust driver, the Python driver and
  `bicdbcli` all implement, with **byte-frozen examples** checked by tests on
  both sides (`crates/net/tests/wire_v01_frozen.rs`,
  `drivers/python/tests/test_wire.py` — same literals).
- **Rust driver `bicdb-client` (`crates/client`)** — `Connection::connect(<ini|dir>)`,
  `query`/`execute`/`describe`/`status`, typed `ResultSet`/`Row` accessors
  (`row.i64(0)`, `row.str(1)`, `row.bytes(1)`), value conversions, and a
  `Error` split that keeps "can't connect" apart from "statement failed"
  (server text passed through verbatim) and from `Busy`.
- **Python driver `drivers/python/bicdb`** — PEP 249 (DB-API 2.0) subset,
  stdlib only: `connect`, `Connection`, `Cursor` (`execute`/`executemany`/
  `fetchone`/`fetchmany`/`fetchall`/`description`/`rowcount`/iteration),
  the full exception hierarchy, `paramstyle = "named"`, DB-API type objects,
  and DB-API transaction semantics (implicit `BEGIN` before writes, `commit`/
  `rollback` to end it, DDL commits first, `autocommit=True` opt-out).

### Fixed

- **Read-your-own-writes** — a session could not see its *own* uncommitted
  changes: consistent read (CR) undid the ITL entry of every transaction
  invisible to the snapshot, including the reader's own. `BEGIN; INSERT;
  SELECT` returned nothing for the just-inserted row. The read side now takes
  a `ReadView { snapshot, own }` (`own` = the session's active transaction, by
  `txn_id`): the reader's own entries are never undone. Others still can't see
  them. (Regression: `crates/cli/tests/e2e.rs`,
  `crates/storage/src/cr.rs::own_txn_sees_its_uncommitted_row_others_do_not`.)
- **`bicdbcli --direct` silently lost explicit transactions** — the direct path
  built a fresh `Session` per statement, so `BEGIN` was rolled back by the
  session's `Drop` and `COMMIT` answered "没有活动事务". The transaction handle
  now lives on the connection (`Instance::txn`) and is lent to each statement's
  session — direct and over-the-service semantics now match.
- **A second connection used to hang** — the service serves one connection at
  a time (single writer). It now best-effort rejects a pending connection with
  a named `ERR` ("实例正忙"), and clients carry a **handshake timeout**
  (default 5 s) surfacing `ClientError::Busy` / `Error::Busy` /
  `OperationalError`, instead of blocking forever.
- The SQL\*Plus transaction test asserted `out.contains('2')`, which matched
  the `ROLLBACK（撤销 2 条）` text — a **false pass** that hid the missing
  read-your-own-writes. It now checks the result table body.

## [0.2.0] — 2026-10-06

**Background service and a standalone SQL\*Plus-style client.** Design note:
`doc/服务与客户端_v0.1.md` (Chinese).

### Added

- **Instance lock** (`<dir>/bicdb.pid`) — one writer per instance, enforced
  rather than assumed: `bicdb init`/`sql`/`shell`/`bicdbcli --direct` take a
  *direct* lock, the service takes a *service* lock, and a second opener is
  refused. Liveness is decided by pid + the process start time read from
  `/proc/<pid>/stat`, so a stale lock (after `kill -9`) is taken over
  automatically — PG's `postmaster.pid` dance, with no `unsafe` and no signals.
- **Service lifecycle** (`pg_ctl`-shaped): `bicdb start <dir> [-s socket]
  [-l log] [-w seconds]`, `stop [-m fast|immediate]`, `status`, `restart`.
  `start` spawns a detached daemon (`process_group(0)`, stdio to the log) and
  **waits until it is ready** (polls the control socket). `stop` asks the
  service to finish over the socket — `fast` runs a full checkpoint first,
  `immediate` exits and lets crash recovery do the work on the next open.
- **`bicdbcli`** (`crates/sqlplus`) — a standalone SQL*Plus-style client:
  multi-line input with **line-number continuation prompts**, `;` to execute,
  blank line to end input without executing, **`/` to re-run the current
  buffer**, buffer editing (`LIST`/`DEL`/`APPEND`/`INPUT`/`CHANGE`/`CLEAR
  BUFFER`), `SET`/`SHOW` session parameters, `SPOOL`, `@file`/`START`/`@@`
  scripts with `&name` substitution and `WHENEVER SQLERROR EXIT`, `DESCRIBE`
  in SQL*Plus layout, `HOST`/`PROMPT`/`REM`/`TIMING`/`HELP`/`EXIT [code]`,
  right-aligned numeric columns, page breaks at `PAGESIZE`, `N rows selected.`
  and `Elapsed: 00:00:00.01`.
- **On-disk layout aligned with the frozen storage design**: the root area now
  holds `control/control01.ctl`, `control/control02.ctl`, `data/<ws>_meta`
  (file 0), `data/<ws>_undo` (file 1) and `wal/redo_g<g>_m<m>`, alongside
  `bicdb.ini` — instead of the flat `file0.dat`/`undo.dat`/`cf_a`/`cf_b` the
  CLI used before.
- **User manual** ([docs/使用手册.md](docs/使用手册.md), Chinese): five-minute
  quick start, instance addressing, the parameter-file reference, command
  reference for `bicdb`/`bicdbcli`, the supported SQL surface with its named
  refusals, operations (locking, crash recovery, backup) and troubleshooting.
- **Connection routing** — a running service means clients (and `bicdb sql`)
  go over the control socket, **one connection = one session**, so
  `BEGIN … COMMIT` spans statements; otherwise the client opens the instance
  directly.
- Control protocol (length-prefixed text frames: `HELLO`/`STATUS`/`SQL`/
  `DESCRIBE`/`SHUTDOWN`) — explicitly a **transitional** local protocol; the
  versioned client protocol lands with `bicdb-net`.
- **Instance parameter file and Oracle-style instance addressing.** The
  instance is addressed by its parameter file, `<db_root>/bicdb.ini`, not by a
  directory: `bicdb init <db_root>` is the only command that takes a directory
  (it *points at the filesystem* and **generates the default parameter file**),
  and every other command (`start`/`stop`/`status`/`restart`/`params`/`sql`/
  `shell`/`bicdbcli`) locates the instance through the parameter file —
  `-p <file|db_root>` > `$BICDB_INI` > `./bicdb.ini`. The root directory is
  **registered inside** the file (`[instance] db_root`) and is authoritative.
  **Key parameters are no longer hardcoded**: `[init]` carries the creation-time
  set (file/undo initial blocks, WAL groups, members per group, group pages —
  `bicdb init -c init.wal_groups=4` builds a 4-group instance) and the other
  sections carry the runtime set (buffer pool frames, file auto-extend
  increment, control socket, service log, lock park timeout, deadlock
  threshold). Creation-time parameters are refused after the fact with a
  per-item diff ("the control file is authoritative; changing them requires a
  rebuild"). `bicdb params` prints every knob with its value, source
  (default / file / command line) and class (creation-time / runtime); unknown
  sections and keys are rejected by name. When the daemon dies during startup,
  `start` now reports the **last log lines** instead of just timing out.
- **Connection routing** — a running service means clients (and `bicdb sql`)
  go over the control socket, **one connection = one session**, so
  `BEGIN … COMMIT` spans statements; otherwise the client opens the instance
  directly.
- Control protocol (length-prefixed text frames: `HELLO`/`STATUS`/`SQL`/
  `DESCRIBE`/`SHUTDOWN`) — explicitly a **transitional** local protocol; the
  versioned client protocol lands with `bicdb-net`.
- **Instance parameter file** `<dir>/bicdb.conf` (PostgreSQL's `postgresql.conf`
  in the data directory; the text half of Oracle's pfile/spfile split).
  `bicdb init` writes it with all defaults, `bicdb params <dir>` prints the
  effective values **with their source** (default / file / command line), and
  `-c key=value` on `bicdb start` overrides the file. Unknown keys are
  **rejected by name** (closed set — an accepted-but-ignored parameter is
  exactly the kind of shell the 0.1.1 audit removed), and every parameter has a
  real sink: buffer pool frames, file auto-extend increment, control socket,
  service log, lock park timeout, deadlock threshold. Creation-time parameters
  (WAL groups/members/group pages, initial file blocks) live in the control
  file and are shown read-only. When the daemon dies during startup, `start`
  now reports the **last log lines** instead of just timing out.

### Fixed

- The lexer accepted ASCII identifiers only — `CREATE TABLE t (名称
  VARCHAR2(8))` failed with "unrecognized character". Non-ASCII bytes are now
  identifier characters, matching PostgreSQL's scanner (unquoted names are
  still folded, ASCII-only).
- `bicdb … | head` no longer panics with a broken pipe.

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
[0.2.0]: https://github.com/xuji755/bicdb/releases/tag/v0.2.0
