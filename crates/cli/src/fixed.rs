//! **固定表的内容源**（`file$`：**控制文件的内存映像**）。
//!
//! `bicdb-sql` 的 `FixedTableSource` 端口在 CLI 侧的真件——为什么是端口：
//! 目录层（`bicdb-catalog`）**不持有控制文件**（它的生命周期在实例/工作区打开
//! 链；`catalog::api::fixed_table` 取两参形态就是这个理由），所以"谁来读控制
//! 文件"由装配层决定。这里读 `<工作区>/control/control0{1,2}.ctl`。
//!
//! **每次查询现读**（不缓存）：固定表的定义就是"行由引擎**在查询时即时产生**"
//! （`arch/03` §3.1.4）——缓存一份就又多了一个可能与控制文件不一致的事实源。
//! 控制文件是两页级的小文件（打开 = 校验两副本 + 读头部），代价可接受。
//!
//! **文件清单的权威是控制文件，不是目录**（`目录详设` 纪律 7）：所以 `file$` 的
//! 内容不会与 `data/` 下的实际文件"各说各话"——不一致由打开链的一致性核对检出。

use std::path::{Path, PathBuf};

use bicdb_catalog::fixed::FixedTable;
use bicdb_sql::session::FixedTableSource;
use bicdb_storage::controlfile::ControlFile;
use bicdb_workspace::io::FileIo;

use crate::boot;

/// CLI 侧真件（工作区目录 + 一条 I/O）。
pub struct CliFixedTables {
    /// 工作区根区目录（控制文件在 `<dir>/control/`）。
    dir: PathBuf,
    /// 控制文件的 I/O（实例的 OS I/O，或测试注入的内存 I/O）。
    io: &'static dyn FileIo,
}

impl CliFixedTables {
    /// 新建。
    #[must_use]
    pub fn new(dir: &Path, io: &'static dyn FileIo) -> Self {
        Self {
            dir: dir.to_path_buf(),
            io,
        }
    }

    /// **进程级一份**（会话借用要 `'static`；与供给方同一手法）。
    #[must_use]
    pub fn new_static(dir: &Path, io: &'static dyn FileIo) -> &'static Self {
        Box::leak(Box::new(Self::new(dir, io)))
    }
}

impl FixedTableSource for CliFixedTables {
    fn fixed_table(&self, name: &str) -> Option<FixedTable> {
        let a = self.dir.join(boot::CF_A);
        let b = self.dir.join(boot::CF_B);
        // 读不到控制文件 ⇒ `None`（调用方**具名拒绝**，不静默给空集）。
        let cf = ControlFile::open(self.io, &a, &b).ok()?;
        let result = if name == "recovery$" {
            let workspace = cf.workspace_entry().ok()?.workspace_id;
            let records = bicdb_storage::recovery_journal::read_records(
                self.io,
                &self.dir.join("recovery.audit"),
                bicdb_workspace::workspace_ref(workspace),
            )
            .ok()?;
            Some(bicdb_catalog::fixed::recovery_table(&records))
        } else {
            cf.data_file_records()
                .ok()
                .and_then(|records| bicdb_catalog::fixed::table(name, &records))
        };
        // Close eagerly; ControlFile's Drop remains the error-path safety net.
        let _ = cf.close();
        result
    }
}
