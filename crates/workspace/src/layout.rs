//! 工作区目录布局（总体方案 §4）：
//! 根目录下分别设置 `catalog`、`data`、`index`、`wal`、`undo`、`assets`、
//! `staging`、`tmp`、`backup` 与 `audit` 子目录。
//!
//! `public` 拥有独立根目录（同一布局）。

/// 工作区根下的固定子目录（十项，即全部）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkspaceDir {
    /// 数据字典（字典表所在的表空间文件等）。
    Catalog,
    /// 数据文件。
    Data,
    /// 索引（B+Tree、ANN、倒排等派生结构）。
    Index,
    /// 每工作区独立的 redo 日志。
    Wal,
    /// Undo 表空间。
    Undo,
    /// 外部资产（不可变文件，受管理）。
    Assets,
    /// 资产登记暂存区（登记式协议：内容先于名字）。
    Staging,
    /// 临时文件（排序 / 哈希溢出等）。
    Tmp,
    /// 备份产物。
    Backup,
    /// 审计（`public` 侧的审计行落在 `public`；此处为工作区侧受控目录）。
    Audit,
}

impl WorkspaceDir {
    /// 全部子目录（顺序即创建顺序）。
    pub const ALL: [WorkspaceDir; 10] = [
        WorkspaceDir::Catalog,
        WorkspaceDir::Data,
        WorkspaceDir::Index,
        WorkspaceDir::Wal,
        WorkspaceDir::Undo,
        WorkspaceDir::Assets,
        WorkspaceDir::Staging,
        WorkspaceDir::Tmp,
        WorkspaceDir::Backup,
        WorkspaceDir::Audit,
    ];

    /// 目录名（小写、固定；不随部署变化）。
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            WorkspaceDir::Catalog => "catalog",
            WorkspaceDir::Data => "data",
            WorkspaceDir::Index => "index",
            WorkspaceDir::Wal => "wal",
            WorkspaceDir::Undo => "undo",
            WorkspaceDir::Assets => "assets",
            WorkspaceDir::Staging => "staging",
            WorkspaceDir::Tmp => "tmp",
            WorkspaceDir::Backup => "backup",
            WorkspaceDir::Audit => "audit",
        }
    }

    /// 该子目录在某工作区根下的路径。
    #[must_use]
    pub fn path_in(self, root: &std::path::Path) -> std::path::PathBuf {
        root.join(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_exactly_the_ten_documented_dirs() {
        let names: Vec<&str> = WorkspaceDir::ALL.iter().map(|d| d.name()).collect();
        assert_eq!(
            names,
            vec![
                "catalog", "data", "index", "wal", "undo", "assets", "staging", "tmp", "backup",
                "audit"
            ]
        );
        // 目录名不重复、不使用路径元字符。
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
    }
}
