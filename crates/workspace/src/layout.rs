//! **工作区目录布局**（权威：`doc/安装布局_v0.1.md` §2；存储侧：`docs/storage/02-工作区存储布局.md` §2.1）。
//!
//! ```text
//! <工作区根>/
//! ├── bicdb.ini      实例参数文件（入口；db_root 注册在它里面）
//! ├── bicdb.pid      实例锁   ├── bicdb.sock  控制套接字
//! ├── control/       control01.ctl  control02.ctl   （恢复锚点，双副本）
//! ├── wal/           redo_g<g>_m<m>                  （组 × 成员）
//! └── data/          <ws>_meta(file 0) · <ws>_undo(file 1) · <ws>_temp(file 2) · <ws>_data_NN(file 3..)
//! ```
//!
//! **只有三个子目录**，因为只有三样东西的生命周期不同：
//!
//! | 目录 | 生命周期 | 为什么单独一层 |
//! | --- | --- | --- |
//! | `control/` | 只在建区与日志切换时写 | 恢复**先**要它——不知道"打开哪些文件"就打不开任何文件 |
//! | `wal/` | **循环复用**（组轮转） | 与只增长的 `data/` 完全不同的运维动作（归档/清日志） |
//! | `data/` | 只增长 | 表空间文件同一命名规则、同一配额口径；**角色写在文件名后缀里** |
//!
//! **旧的十目录（本文件的上一版）已收敛**：`catalog`/`undo`/`tmp` 是 file 0/1/2 的
//! **角色**，不是一个目录；`index` 是数据文件里的段（与表同文件），不是目录；
//! `backup` 提到 `<BICDB_HOME>/backup/`；`audit` 的行落在 `public` 工作区自己的表里；
//! `assets`/`staging` **不在工作区根下**——它们按卷分布在"文件系统池"里
//! （`docs/storage/02-工作区存储布局.md` §2.1/§2.9），见 [`POOL_DIRS`]。
//!
//! `public` 拥有独立根目录（同一布局）——它是 V1.0 唯一的工作区。

/// 工作区根下的固定子目录（三项，即全部）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkspaceDir {
    /// 控制文件双副本（恢复锚点）。
    Control,
    /// 每工作区独立的 redo 日志。
    Wal,
    /// 表空间文件（按**文件角色**命名：`<ws>_meta`/`_undo`/`_temp`/`_data_NN`）。
    Data,
}

impl WorkspaceDir {
    /// 全部子目录（顺序即创建顺序）。
    pub const ALL: [WorkspaceDir; 3] =
        [WorkspaceDir::Control, WorkspaceDir::Wal, WorkspaceDir::Data];

    /// 目录名（小写、固定；不随部署变化）。
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            WorkspaceDir::Control => "control",
            WorkspaceDir::Wal => "wal",
            WorkspaceDir::Data => "data",
        }
    }

    /// 该子目录在某工作区根下的路径。
    #[must_use]
    pub fn path_in(self, root: &std::path::Path) -> std::path::PathBuf {
        root.join(self.name())
    }
}

/// **池中的**目录名（**不在工作区根下**）——按卷分布在文件系统池里，
/// 同样的相对路径挂在不同文件系统上（`存储布局` §2.8–§2.10）。
///
/// 本版（单机单工作区）还没有池，常量先立在这里给资产/容量切片用。
pub const POOL_DIRS: [&str; 2] = ["assets", "staging"];

/// 数据文件的**角色后缀**（`data/` 下的命名：`<工作区标识>_<角色>`）。
///
/// 角色写在**文件名**里，运维看一眼就知道这是哪一类文件，不用记目录位置
/// （`存储布局` §2.2 的"文件号 = 角色"约定）。
pub mod file_role {
    /// file 0：数据字典 + 引导页（不承载用户数据行）。
    pub const META: &str = "meta";
    /// file 1：撤销。
    pub const UNDO: &str = "undo";
    /// file 2：临时（溢出；不持久化、不备份）。
    pub const TEMP: &str = "temp";
    /// file 3..：用户数据（`_data_01`、`_data_02` …）。
    pub const DATA: &str = "data";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_exactly_the_three_documented_dirs() {
        let names: Vec<&str> = WorkspaceDir::ALL.iter().map(|d| d.name()).collect();
        assert_eq!(names, vec!["control", "wal", "data"]);
    }

    #[test]
    fn paths_hang_under_the_workspace_root() {
        let root = std::path::Path::new("/ws/public");
        assert_eq!(WorkspaceDir::Control.path_in(root), root.join("control"));
        assert_eq!(WorkspaceDir::Data.path_in(root), root.join("data"));
        // 池中目录不在根下（资产/版本切片用）。
        assert_eq!(POOL_DIRS, ["assets", "staging"]);
        assert_eq!(file_role::META, "meta");
        assert_eq!(file_role::TEMP, "temp");
    }
}
