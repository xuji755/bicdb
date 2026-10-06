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
//! **已落地**：
//! - [`dict`]（C1c）：**字典表的内核常量**——表/列/键/类型码/选项码（建区时
//!   据此写字典行，打开时据此自检 `self_check`）；自举条目数的定量
//!   （普通 **15** = 7 表 + 8 索引；`public` **23**）+ 自检；
//! - [`create`]（C2a）：**建区期自举集**——7 个堆段 + 8 个空 B+Tree 段 +
//!   引导页三件套（直写、无 redo，见模块文档的可见性分界）；
//! - [`row`]（C2b）：**字典行的值域与编解码**（全变长 + NULL 位图；NUMBER
//!   走保序编码 ⇒ 索引键直接取行字节）；
//! - [`open`]（C2b）：**打开链**（file 0 → 引导页自愈读 → 表/索引映射 →
//!   `scan`/`fetch`/`lookup`/`scan_index`）+ **自举种子**（自洽字典）；
//! - [`consistency`]（C2c）：**`file_scn` ↔ 控制文件检查点**核对（§2.5 判定表）。
//!
//! **未落地**：**字典行缓存**（C3，§4.2）、`resolve`/`columns`/`object_version`
//! 只读面（C3）、DDL 写侧（C4）。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod consistency;
pub mod create;
pub mod dict;
pub mod open;
pub mod row;

pub use consistency::{check_files, ConsistencyReport, FileCheck, FilePoint, FileVerdict, Finding};
pub use create::{create_dictionary, BootstrapObject, BuiltDictionary, CreateError};
pub use dict::{
    bootstrap_entries_normal, bootstrap_entries_public, index_kind, is_public_only, namespace,
    obj_kind, self_check, table_opt, ColDef, ColTypeCode, DictTable, KeyDef, DICT_TABLES,
};
pub use open::{comp_num, comp_text, Catalog, OpenError};
pub use row::{decode as decode_row, encode as encode_row, DictValue};
