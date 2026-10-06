//! # bicdb-access — 表访问服务（REQ-ENG-003 的**写侧**形态）
//!
//! ```text
//! 表访问服务 = 页选址/分配 + 行写（ITL/undo/redo 全在事务引擎里）+ 索引维护
//!              调用方只给"行字节/键字节"，拿回 ROWID
//! ```
//!
//! **为什么单独一个 crate**（2026-10-06 定案）：写侧有两个消费者——**执行器
//! DML**（用户表）与**目录 DDL**（字典表，`目录详设` §5.2 "字典表 = 普通表 ⇒
//! 读写全部经表引擎"）。两者要的能力**完全同形**（选址、增长、行写、索引维护、
//! 树头持久化），实现只应有一份 ⇒ 落在本 crate，两侧都依赖它。
//!
//! # 三条纪律（逐条有据）
//!
//! 1. **表增长经 redo**：新页 = 段追加位置计划（`plan_advance_append`，经池 +
//!    redo）+ **先格式化落盘 + fsync、再进 redo**（[`bicdb_txn::write::fresh_page_with_redo`]
//!    ——与撤销页/索引页同一条纪律：物理增量无法重建不存在的页）；
//! 2. **行写不在这里**：ITL 占用、行锁/等待、撤销记录、页差异 redo 全在
//!    [`bicdb_txn::write`]——本模块只做"选址 + 转发"，**不重造事务语义**；
//! 3. **索引维护与行写同一事务**（`arch/09` §9.1.2：索引项**不做独立撤销**，
//!    随行的插入/删除走同一 redo 流；事务回滚由行侧/页侧的重放与撤销覆盖）。
//!
//! # 读侧在哪里
//!
//! 读侧已在 [`bicdb_storage::scan`]（区读 + 按块批量回表 + CR 重建）与执行器的
//! 扫描算子里落地——本 crate **只补写侧**（此前"表增长"是空缺：执行器夹具里
//! 明写"表增长不在本切片"）。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod heap;
pub mod index;

pub use heap::{TableAccess, TableAccessError};
pub use index::{delete_entry, insert_entry, write_tree_head_redo, IndexTarget};
