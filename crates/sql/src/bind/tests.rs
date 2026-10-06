//! S2 单元用例（假目录件：三格规则在本模块，故可用内存件测全）。

use super::*;

/// 内存假目录（三格规则由 `NameResolver` 承担，假件只回答"目录里有什么"）。
#[derive(Default)]
struct FakeView {
    tables: BTreeMap<String, CatalogObject>,
    indexes: BTreeMap<String, CatalogObject>,
    public: bool,
    /// 目录层错误注入（基础设施错误不得被伪装成"不存在"）。
    fail_next: bool,
}

impl FakeView {
    fn with_table(mut self, name: &str, obj: u32) -> Self {
        self.tables.insert(
            name.to_owned(),
            CatalogObject {
                obj,
                name: name.to_owned(),
                namespace: NameSpace::Table,
                type_code: bicdb_catalog::obj_kind::TABLE,
                dataobj: obj,
                status: 1,
                mtime: 7,
            },
        );
        self
    }

    fn with_bootstrap(self, name: &str, obj: u32) -> Self {
        self.with_table(name, obj)
    }
}

impl CatalogView for FakeView {
    fn resolve(&mut self, ns: NameSpace, name: &str) -> Result<CatalogObject, BindError> {
        if self.fail_next {
            self.fail_next = false;
            return Err(BindError::Catalog("注入的基础设施错误".to_owned()));
        }
        let hit = match ns {
            NameSpace::Table => self.tables.get(name),
            NameSpace::Index => self.indexes.get(name),
        };
        hit.cloned().ok_or_else(|| BindError::NotFound {
            name: name.to_owned(),
            ns,
        })
    }
    fn columns(&mut self, _obj: u32) -> Result<Vec<CatalogColumn>, BindError> {
        Ok(vec![])
    }
    fn indexes_of(&mut self, _obj: u32) -> Result<Vec<CatalogIndex>, BindError> {
        Ok(vec![])
    }
    fn object_version(&mut self, _obj: u32) -> Result<(u64, u32), BindError> {
        Ok((7, 1))
    }
    fn fixed_table(&mut self, name: &str) -> Result<bool, BindError> {
        Ok(FIXED_TABLES.contains(&name))
    }
    fn is_public(&self) -> bool {
        self.public
    }
}

#[test]
fn tier_one_resolves_user_objects_and_captures_version() {
    let mut view = FakeView::default().with_table("t1", 100);
    let mut r = NameResolver::new(&mut view);
    let hit = r.resolve_table("t1").unwrap();
    assert_eq!(hit.object().unwrap().obj, 100);
    assert_eq!(hit.name(), "t1");
    let refs = r.into_refs();
    assert_eq!(refs.objects().get(&100), Some(&7), "版本捕获 (obj#, mtime)");
    assert!(refs.indexes().is_empty());
}

#[test]
fn tier_three_bootstrap_objects_are_out_of_scope() {
    let mut view = FakeView::default().with_bootstrap("obj$", 1);
    let mut r = NameResolver::new(&mut view);
    // 自举对象在目录里**能**解析到，但会话解析范围**不含**它 ⇒ 一视同仁地"不存在"。
    let err = r.resolve_table("obj$").unwrap_err();
    assert!(
        matches!(err, BindError::NotFound { ref name, .. } if name == "obj$"),
        "{err}"
    );
    // 写目标同样出局。
    let err2 = r.resolve_write_target("obj$").unwrap_err();
    assert!(matches!(err2, BindError::NotFound { .. }), "{err2}");
}

#[test]
fn tier_two_fixed_tables_are_readable_but_never_writable() {
    let mut view = FakeView::default();
    let mut r = NameResolver::new(&mut view);
    let hit = r.resolve_table("file$").unwrap();
    assert_eq!(hit, ResolvedName::FixedTable("file$"));
    assert!(hit.object().is_none(), "固定表没有 ObjectRef");
    let refs = r.into_refs();
    assert!(refs.fixed_tables().contains("file$"), "固定表进捕获集");
    assert!(refs.objects().is_empty(), "固定表无版本");

    // **写目标没有第 ② 格**：写 `file$` 的表现是"不存在"。
    let mut view2 = FakeView::default();
    let mut r2 = NameResolver::new(&mut view2);
    let err = r2.resolve_write_target("file$").unwrap_err();
    assert!(matches!(err, BindError::NotFound { .. }), "{err}");
    // `session$`/`lock$` 同理（清单已占名，行随后续切片产生）。
    let err2 = r2.resolve_write_target("session$").unwrap_err();
    assert!(matches!(err2, BindError::NotFound { .. }), "{err2}");
}

#[test]
fn not_found_is_indistinguishable_across_reasons() {
    let mut view = FakeView::default().with_table("mine", 100);
    let mut r = NameResolver::new(&mut view);
    // 三类原因走同一个出口：别人的（假件天然查不到）/ 已删的 / 从未存在的。
    for name in ["someone_elses", "dropped", "never_existed"] {
        let err = r.resolve_table(name).unwrap_err();
        assert_eq!(
            err,
            BindError::NotFound {
                name: name.to_owned(),
                ns: NameSpace::Table
            },
            "不可区分：{name}"
        );
    }
    // 基础设施错误**不伪装**为不存在。
    let mut view3 = FakeView {
        fail_next: true,
        ..FakeView::default()
    };
    let mut r3 = NameResolver::new(&mut view3);
    let err = r3.resolve_table("t").unwrap_err();
    assert!(matches!(err, BindError::Catalog(_)), "{err}");
}

#[test]
fn public_workspace_hides_file_from_ordinary_sessions() {
    let mut view = FakeView {
        public: true,
        ..FakeView::default()
    };
    // 普通会话（默认策略）：`file$` 不可见 ⇒ 不存在。
    let mut r = NameResolver::new(&mut view);
    let err = r.resolve_table("file$").unwrap_err();
    assert!(matches!(err, BindError::NotFound { .. }), "{err}");
    drop(r);
    // admin 视角：可见。
    let mut r2 = NameResolver::with_policy(&mut view, ResolvePolicy { is_admin: true });
    assert_eq!(
        r2.resolve_table("file$").unwrap(),
        ResolvedName::FixedTable("file$")
    );
}

#[test]
fn reserved_names_are_rejected_for_new_objects() {
    for bad in ["t$", "memory", "audit", "ref$", "asset$", "session"] {
        let err = check_new_object_name(bad).unwrap_err();
        assert!(
            matches!(err, BindError::ReservedName(_)),
            "保留名应拒绝：{bad} → {err}"
        );
    }
    for ok in ["t1", "memory2", "audit_log", "my_table"] {
        assert!(check_new_object_name(ok).is_ok(), "非保留名应通过：{ok}");
    }
    assert!(is_reserved_name("x$") && !is_reserved_name("x"));
}

#[test]
fn index_resolution_records_its_own_version() {
    let mut view = FakeView::default();
    view.indexes.insert(
        "i_t1".to_owned(),
        CatalogObject {
            obj: 101,
            name: "i_t1".to_owned(),
            namespace: NameSpace::Index,
            type_code: bicdb_catalog::obj_kind::INDEX,
            dataobj: 101,
            status: 1,
            mtime: 9,
        },
    );
    let mut r = NameResolver::new(&mut view);
    let idx = r.resolve_index("i_t1").unwrap();
    assert_eq!(idx.obj, 101);
    r.note_index_version(idx.obj, idx.mtime, 0); // `Move` 后 status = 0
    let refs = r.into_refs();
    assert_eq!(
        refs.indexes().get(&101),
        Some(&(9, 0)),
        "索引键 (obj#, mtime, status)"
    );
    // 自举索引出局。
    let mut view2 = FakeView::default();
    view2.indexes.insert(
        "i_obj_pk".to_owned(),
        CatalogObject {
            obj: 2,
            name: "i_obj_pk".to_owned(),
            namespace: NameSpace::Index,
            type_code: bicdb_catalog::obj_kind::INDEX,
            dataobj: 2,
            status: 1,
            mtime: 0,
        },
    );
    let mut r2 = NameResolver::new(&mut view2);
    assert!(matches!(
        r2.resolve_index("i_obj_pk").unwrap_err(),
        BindError::NotFound { .. }
    ));
}
