//! **目录只读面的真件实现**（`bicdb-catalog::Catalog` → [`CatalogView`]）。
//!
//! ```text
//! sql ──读──▶ catalog ──读──▶ storage（表访问服务）
//! ```
//!
//! **两处形态映射**（都在这里消化，三格规则在 [`super`]）：
//!
//! 1. **快照**：目录只读面的每个调用都带语句快照（`CommitSeq`）——本适配器
//!    在构造时固定它（`NameResolver` 随语句走，快照随语句定）；
//! 2. **不可区分**：目录的 `CatalogError::NotFound` 映射为 [`BindError::NotFound`]，
//!    其余（存储/行编解码）映射为 [`BindError::Catalog`]——**不把基础设施错误
//!    伪装成"不存在"**。

use bicdb_catalog::api::CatalogError;
use bicdb_catalog::Catalog;
use bicdb_common::seq::CommitSeq;

use super::{BindError, CatalogColumn, CatalogIndex, CatalogObject, CatalogView, NameSpace};

/// **目录只读面的真件**（借一个 `bicdb-catalog::Catalog` + 固定语句快照）。
pub struct CatalogViewImpl<'c, 'io> {
    catalog: &'c mut Catalog<'io>,
    snapshot: CommitSeq,
}

impl<'c, 'io> CatalogViewImpl<'c, 'io> {
    /// 建视图（`snapshot` = 语句快照）。
    #[must_use]
    pub fn new(catalog: &'c mut Catalog<'io>, snapshot: CommitSeq) -> Self {
        Self { catalog, snapshot }
    }

    fn ns_of(code: u32) -> NameSpace {
        if code == NameSpace::Index.code() {
            NameSpace::Index
        } else {
            NameSpace::Table
        }
    }

    fn map_err(name: &str, ns: NameSpace, e: CatalogError) -> BindError {
        match e {
            CatalogError::NotFound => BindError::NotFound {
                name: name.to_owned(),
                ns,
            },
            other => BindError::Catalog(other.to_string()),
        }
    }
}

impl CatalogView for CatalogViewImpl<'_, '_> {
    fn resolve(&mut self, ns: NameSpace, name: &str) -> Result<CatalogObject, BindError> {
        let r = self
            .catalog
            .resolve(self.snapshot, ns.code(), name)
            .map_err(|e| Self::map_err(name, ns, e))?;
        Ok(CatalogObject {
            obj: r.obj,
            name: r.name,
            namespace: Self::ns_of(r.namespace),
            type_code: r.type_code,
            dataobj: r.dataobj,
            status: r.status,
            mtime: r.mtime,
        })
    }

    fn columns(&mut self, obj: u32) -> Result<Vec<CatalogColumn>, BindError> {
        let cols = self
            .catalog
            .columns(self.snapshot, obj)
            .map_err(|e| Self::map_err("<列枚举>", NameSpace::Table, e))?;
        Ok(cols
            .into_iter()
            .map(|c| CatalogColumn {
                col: c.col,
                name: c.name,
                type_code: c.type_code,
                length: c.length,
                nullable: c.nullable,
            })
            .collect())
    }

    fn indexes_of(&mut self, obj: u32) -> Result<Vec<CatalogIndex>, BindError> {
        let idx = self
            .catalog
            .indexes_of(self.snapshot, obj)
            .map_err(|e| Self::map_err("<索引枚举>", NameSpace::Table, e))?;
        Ok(idx
            .into_iter()
            .map(|i| CatalogIndex {
                obj: i.obj,
                bobj: i.bobj,
                cols: i.cols.iter().map(|c| c.col).collect(),
                unique: i.is_unique,
                status: i.status,
                mtime: 0, // 由 `object_version` 单独取（索引版本随其对象行）
            })
            .collect())
    }

    fn object_version(&mut self, obj: u32) -> Result<(u64, u32), BindError> {
        let v = self
            .catalog
            .object_version(self.snapshot, obj)
            .map_err(|e| Self::map_err("<版本>", NameSpace::Table, e))?;
        Ok((v.mtime, v.status))
    }

    fn fixed_columns(&mut self, name: &str) -> Result<Option<Vec<CatalogColumn>>, BindError> {
        // **不查字典**（`arch/03` §3.1.4）：固定表的存在性由清单本身回答；
        // 行的产生在查询期（`file$` 的内容 = 控制文件内存映像）。
        if !super::FIXED_TABLES.contains(&name) {
            return Ok(None);
        }
        // V1.0 只有 `file$` 有列形状（`session$`/`lock$` 随会话层）。
        let Some(cols) = bicdb_catalog::fixed::columns(name) else {
            return Ok(None);
        };
        Ok(Some(
            cols.iter()
                .enumerate()
                .map(|(i, c)| CatalogColumn {
                    col: i as u32 + 1,
                    name: c.name.to_owned(),
                    type_code: c.type_code as u32,
                    length: 0,
                    nullable: true,
                })
                .collect(),
        ))
    }

    fn is_public(&self) -> bool {
        self.catalog.is_public()
    }

    fn segment_block(&mut self, obj: u32) -> Result<u32, BindError> {
        bicdb_catalog::ddl::live_segment_block(self.catalog, obj)
            .map_err(|e| BindError::Catalog(e.to_string()))
    }
}

/// **给物理计划层同一个目录口**（[`crate::plan::PlanCatalog`]）。
///
/// 为什么不让计划层直接用 [`CatalogView`]：计划层的取数面**只有**段/索引三件事，
/// 用窄口能挡住"计划期顺手查了别的东西"（也便于用例给假件）。
impl crate::plan::PlanCatalog for CatalogViewImpl<'_, '_> {
    fn segment_block(&mut self, obj: u32) -> Result<u32, BindError> {
        CatalogView::segment_block(self, obj)
    }

    fn index_segment(&mut self, obj: u32) -> Result<(u16, u32), BindError> {
        bicdb_catalog::ddl::live_segment_location(self.catalog, obj)
            .map_err(|e| BindError::Catalog(e.to_string()))
    }

    fn indexes_of(&mut self, obj: u32) -> Result<Vec<CatalogIndex>, BindError> {
        CatalogView::indexes_of(self, obj)
    }
}
