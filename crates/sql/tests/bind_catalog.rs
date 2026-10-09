//! **S2 验收（真件）**：`bind::CatalogViewImpl` 接**真目录**（`bicdb-catalog`
//! 的 file 0 + DDL 写侧）。
//!
//! 钉住 S2 的三条验收（`SQL前端设计` §10）：
//! - **跨区名不可区分**：A 区解析只在 B 区存在的名字 ⇒ 与"从未存在"**同一个**错误；
//! - **保留名拒绝**：`CREATE` 名字 `$` 结尾/预置名一律拒绝；
//! - **`(obj#, mtime)` 被记入 Bound**：解析即捕获版本，`Move` 失效后
//!   `(obj#, mtime, status)` 变化 ⇒ **键失配可观测**。

use std::path::Path;

use bicdb_catalog::ddl::{self, ColumnSpec, IndexSpec, TableOptions, TableSpec};
use bicdb_catalog::{Catalog, ColTypeCode};
use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_sql::bind::{
    check_new_object_name, BindError, CatalogView, CatalogViewImpl, NameResolver, NameSpace,
    ResolvedName,
};
use bicdb_storage::buffer::{BufferPool, CacheConfig, SystemClock, WalGuard};
use bicdb_storage::controlfile::{
    ArchiveMode, ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry,
};
use bicdb_storage::datafile::DataFile;
use bicdb_storage::undo::{create_undo_segment, UndoChain};
use bicdb_txn::engine::Engine;
use bicdb_wal::group::{GroupSpec, GroupWriter};
use bicdb_workspace::io::MemFileIo;
use bicdb_workspace::WorkspaceId;

const WS: [u8; 8] = [17u8; 8];

fn seq(v: u64) -> CommitSeq {
    CommitSeq::from_raw(v).unwrap()
}
fn lsn(v: u64) -> Lsn {
    Lsn::from_raw(v).unwrap()
}

struct FakeWal;
impl WalGuard for FakeWal {
    fn durable_lsn(&self) -> Lsn {
        Lsn::from_raw(u64::MAX >> 16).unwrap()
    }
    fn ensure_durable(&self, _t: Lsn) -> std::io::Result<()> {
        Ok(())
    }
}

/// 一个工作区（file 0 + 池 + 日志 + 引擎）。
struct Ws {
    pool: &'static BufferPool<'static>,
    engine: &'static Engine<'static, 'static, 'static, 'static>,
    path: String,
}

fn workspace(io: &'static MemFileIo, tag: &str) -> Ws {
    let undo_path = format!("/mem/{tag}_undo.dat");
    let data_path = format!("/mem/{tag}_file0.dat");
    let undo_file: &'static mut DataFile<'static> = Box::leak(Box::new(
        DataFile::create(io, Path::new(&undo_path), 1, 1, WS, 512).unwrap(),
    ));
    let undo_handle = undo_file.handle();
    let undo_seg = create_undo_segment(undo_file, 2, 3, 4).unwrap();
    let mut file0 = DataFile::create(
        io,
        Path::new(&data_path),
        0,
        bicdb_storage::bitmap::META_ROLE,
        WS,
        bicdb_storage::bitmap::FileLayout::meta().min_file_blocks() + 512,
    )
    .unwrap();
    let built = bicdb_catalog::create_dictionary(&mut file0, WS, false).unwrap();
    let mut cat = Catalog::from_entries(file0, built.entries.clone()).unwrap();
    cat.seed_own_dictionary(&built).unwrap();
    drop(cat);
    let handle = DataFile::open(io, Path::new(&data_path)).unwrap().handle();

    let pool: &'static BufferPool<'static> = Box::leak(Box::new(
        BufferPool::with_config(
            io,
            64,
            move |_ws, r| match r.file_id() {
                0 => Some((handle, r.block_id())),
                1 => Some((undo_handle, r.block_id())),
                _ => None,
            },
            FakeWal,
            SystemClock,
            CacheConfig::for_capacity(64),
        )
        .unwrap(),
    ));
    let cf_a = format!("/mem/{tag}_cf_a");
    let cf_b = format!("/mem/{tag}_cf_b");
    let wal = format!("/mem/{tag}_wal");
    let cf: &'static mut ControlFile<'static> = Box::leak(Box::new(
        ControlFile::format(
            io,
            Path::new(&cf_a),
            Path::new(&cf_b),
            &WorkspaceEntry {
                workspace_id: WorkspaceId::from_raw(1).unwrap(),
                created_at: 0,
                derived_from: None,
                derived_at_seq: seq(0),
            },
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::new(ArchiveMode::NoArchive),
        )
        .unwrap(),
    ));
    let spec = GroupSpec::new(2, 1, 8192).unwrap();
    let writer = GroupWriter::create(io, cf, Path::new(&wal), spec, lsn(0)).unwrap();
    let engine: &'static Engine<'static, 'static, 'static, 'static> =
        Box::leak(Box::new(Engine::new(
            pool,
            writer,
            UndoChain::open(undo_seg).with_pool(pool),
            seq(0),
        )));
    Ws {
        pool,
        engine,
        path: data_path,
    }
}

fn open(io: &'static MemFileIo, ws: &Ws) -> Catalog<'static> {
    let mut cat = Catalog::open(io, Path::new(&ws.path)).unwrap();
    cat.attach_pool(ws.pool);
    cat
}

fn spec(name: &str) -> TableSpec {
    TableSpec {
        name: name.to_owned(),
        columns: vec![
            ColumnSpec {
                name: "id".to_owned(),
                type_code: ColTypeCode::Number,
                length: 0,
                precision: None,
                scale: None,
                nullable: false,
            },
            ColumnSpec {
                name: "tag".to_owned(),
                type_code: ColTypeCode::Varchar2,
                length: 64,
                precision: None,
                scale: None,
                nullable: true,
            },
        ],
        options: TableOptions::default(),
    }
}

#[test]
fn binder_resolves_against_the_real_catalog() {
    let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
    io.add_dir("/mem");
    // A 区与 B 区（各自一份 file 0）——跨区名不可区分的对照。
    let ws_a = workspace(io, "sa");
    let ws_b = workspace(io, "sb");
    let mut cat_a = open(io, &ws_a);
    let mut cat_b = open(io, &ws_b);
    ddl::init_dictionary_tables(&mut cat_a, ws_a.engine).unwrap();
    ddl::init_dictionary_tables(&mut cat_b, ws_b.engine).unwrap();
    let ta = ddl::create_table(&mut cat_a, ws_a.engine, &spec("ta")).unwrap();
    let _tb = ddl::create_table(&mut cat_b, ws_b.engine, &spec("tb")).unwrap();
    let snap = seq(ta.commit_seq);

    // ── 第 ① 格 + 版本捕获 ──
    {
        let mut view = CatalogViewImpl::new(&mut cat_a, snap);
        let mut r = NameResolver::new(&mut view);
        let hit = r.resolve_table("ta").unwrap();
        let obj = hit.object().expect("对象形态").clone();
        assert!(obj.obj >= 100, "用户对象 obj# ≥ 100：{}", obj.obj);
        assert_eq!(obj.mtime, ta.commit_seq, "mtime = DDL 提交序号");
        let refs = r.into_refs();
        assert_eq!(
            refs.objects().get(&obj.obj),
            Some(&ta.commit_seq),
            "(obj#, mtime) 进 Bound"
        );
    }

    // ── 跨区名不可区分：A 区解析只在 B 区存在的名字 ──
    {
        let mut view = CatalogViewImpl::new(&mut cat_a, snap);
        let mut r = NameResolver::new(&mut view);
        let cross = r.resolve_table("tb").unwrap_err();
        let never = r.resolve_table("never_existed").unwrap_err();
        // **不可区分 = 同一个错误种类**（携带的名字当然不同——那是调用方自己给的）。
        assert_eq!(
            std::mem::discriminant(&cross),
            std::mem::discriminant(&never),
            "跨区名与从未存在的名字必须同类：{cross} / {never}"
        );
        assert_eq!(
            cross.to_string(),
            "对象 `tb` 不存在",
            "错误文案不含任何「别的工作区有它」的暗示"
        );
        assert!(matches!(cross, BindError::NotFound { .. }));
        // A 区自己也确实没有 tb 的痕迹（B 的表不在 A 的字典里）。
        assert!(cat_a
            .resolve(snap, bicdb_catalog::namespace::TABLE, "tb")
            .is_err());
    }

    // ── 第 ③ 格：自举对象出局（目录能解析、会话不能）──
    {
        assert!(
            cat_a
                .resolve(snap, bicdb_catalog::namespace::TABLE, "obj$")
                .is_ok(),
            "目录层能读到自举对象"
        );
        let mut view = CatalogViewImpl::new(&mut cat_a, snap);
        let mut r = NameResolver::new(&mut view);
        assert_eq!(
            r.resolve_table("obj$").unwrap(),
            ResolvedName::FixedTable("obj$")
        );
        assert!(matches!(
            r.resolve_write_target("obj$").unwrap_err(),
            BindError::NotWritable(_)
        ));
        assert!(matches!(
            r.resolve_index("i_obj_pk").unwrap_err(),
            BindError::NotFound { .. }
        ));
    }

    // ── 第 ② 格：固定表（可读、无写入口）──
    {
        let mut view = CatalogViewImpl::new(&mut cat_a, snap);
        let mut r = NameResolver::new(&mut view);
        assert_eq!(
            r.resolve_table("file$").unwrap(),
            ResolvedName::FixedTable("file$")
        );
        assert!(matches!(
            r.resolve_write_target("file$").unwrap_err(),
            BindError::NotFound { .. }
        ));
    }

    // ── 保留名拒绝 ──
    for bad in ["t$", "memory", "audit"] {
        assert!(matches!(
            check_new_object_name(bad).unwrap_err(),
            BindError::ReservedName(_)
        ));
    }

    // ── 索引版本捕获 + `Move` 失效 ⇒ 键失配（断点查验）──
    {
        let idx_spec = IndexSpec {
            name: "i_ta_id".to_owned(),
            table: "ta".to_owned(),
            unique: true,
            columns: vec!["id".to_owned()],
        };
        let iout = ddl::create_index(&mut cat_a, ws_a.engine, &idx_spec).unwrap();
        let snap2 = seq(iout.commit_seq);

        let before = {
            let mut view = CatalogViewImpl::new(&mut cat_a, snap2);
            let mut r = NameResolver::new(&mut view);
            let idx = r.resolve_index("i_ta_id").unwrap();
            assert_eq!(idx.obj, iout.obj);
            let (mtime, status) = r.view().object_version(idx.obj).unwrap();
            r.note_index_version(idx.obj, mtime, status);
            r.into_refs()
        };
        assert_eq!(before.indexes().get(&iout.obj), Some(&(iout.commit_seq, 1)));

        // Move 失效（字典侧）：status → 0。
        ddl::invalidate_indexes_for_move(&mut cat_a, ws_a.engine, "ta").unwrap();
        let after = {
            let mut view = CatalogViewImpl::new(&mut cat_a, snap2);
            let mut r = NameResolver::new(&mut view);
            let idx = r.resolve_index("i_ta_id").unwrap();
            let (mtime, status) = r.view().object_version(idx.obj).unwrap();
            r.note_index_version(idx.obj, mtime, status);
            r.into_refs()
        };
        assert_eq!(
            after.indexes().get(&iout.obj).map(|(_, s)| *s),
            Some(0),
            "Move 后 status = 0"
        );
        assert_ne!(before, after, "**键失配可观测**（失效靠比对、不靠通知）");
        // 列枚举也走真件（类型/可空来自 col$）。
        let cols = {
            let mut view = CatalogViewImpl::new(&mut cat_a, snap2);
            view.columns(ta.obj).unwrap()
        };
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name, "id");
        assert!(!cols[0].nullable);
        assert!(cols[1].nullable);
    }

    // ── 索引枚举（含 status）──
    {
        let mut view = CatalogViewImpl::new(&mut cat_a, seq(9_999));
        let idxs = view.indexes_of(ta.obj).unwrap();
        assert_eq!(idxs.len(), 1);
        assert_eq!(idxs[0].cols, vec![1], "键列 = id");
        assert!(idxs[0].unique);
        assert_eq!(idxs[0].status, 0, "Move 失效后的状态如实呈现");
        let _ = NameSpace::Table;
    }
}
