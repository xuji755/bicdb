//! # bicdb-catalog — 目录（数据字典）
//!
//! 设计依据：[`doc/目录详设_v0.1.md`]（草案 v0.1，**已冻结**）与
//! `doc/spec/ENG.md` REQ-ENG-006（目录服务接口：`resolve` / `type_descriptor` /
//! `object_version`）、`doc/arch/03` §3.1（字典表的数据模型）。
//!
//! **两条纪律**：
//! 1. **字典表 = 普通表**——读写全部经表引擎 + 事务 + redo，不发明第二套
//!    元数据机制；
//! 2. **目录接口只读**——写一律走 DDL 路径（独立事务 + 行锁）。
//!
//! **本切片（C1c）已落地**：
//! - [`dict`]：**字典表的内核常量**——表/列/键/类型码/选项码（建区时据此写
//!   字典行，打开时据此自检 `self_check`）；自举条目数的定量（普通 **15** =
//!   7 表 + 8 索引；`public` **23**）+ 自检。
//!
//! **未落地**：引导页的**内容装配**（`bicdb-storage::bootstrap` 只给格式——
//! "哪 15 个自举对象、各自的段头在哪"由 C2 的打开链/建区协议接）、
//! **打开链**（C2）、**字典行缓存**（C2）、`resolve`/`columns`/`object_version`
//! 只读面（C3）、DDL 写侧（C4）。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod dict;

pub use dict::{
    bootstrap_entries_normal, bootstrap_entries_public, index_kind, is_public_only, namespace,
    obj_kind, self_check, table_opt, ColDef, ColTypeCode, DictTable, KeyDef, DICT_TABLES,
};
