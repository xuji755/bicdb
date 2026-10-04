//! # bicdb-tools
//!
//! 诊断与检查工具：`page_dump`、`db_check`
//!
//! - 设计依据：§17（P0 至 P3 工作与验收）、`STO` REQ-STO-012
//!   （P2 必须交付页编解码、`page_dump`、`db_check`、损坏识别与处理路径；
//!   **先于完整 SQL 执行器**）
//! - 对应阶段：**P2**（已启动）
//! - 当前状态：**v0.1**——[`check`]（页级结构检查 + 片段链检查 +
//!   损坏判定三档）、[`dump`]（页转储）；两个只读二进制
//!   （`page_dump` / `db_check`）面向 16 KiB 页镜像文件。
//!
//! **只读纪律**：本 crate 不引入写路径（工具不得改动被检数据）。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod check;
pub mod dump;

pub use check::{
    check_heap, check_page, check_page_image, CheckReport, Finding, Severity, Verdict,
};
pub use dump::{page_dump, page_image_dump};
