//! 工作区根目录：名（服务端生成）与句柄。
//!
//! 设计依据：总体方案 §4（根目录使用**服务端生成的不透明 ID**，注册表映射身份，
//! **不得直接拼接用户输入**）与 `ISO` REQ-ISO-008。
//!
//! - 普通工作区根名：`w-` + 12 位小写十六进制（由 [`WorkspaceId`] 派生）；
//! - `public`：保留根名（`public` 拥有独立根目录）；
//! - **没有"从任意字符串直接构造"的公开入口**——[`RootName::parse`] 只接受上述
//!   两种规范形态；用户输入因此进不了路径拼接（路径穿越失去构造材料）。
//!
//! **TOCTOU 的边界**：本节只负责创建与既有对象的校验。`REQ-ISO-008` 要求的
//! "打开不跟随符号链接、**以句柄为准不以路径为准**"在 P2 的文件打开层落实——
//! 那里的检查作用于已打开的**描述符**，而不是先查路径再按路径打开。

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::id::WorkspaceId;
use crate::layout::WorkspaceDir;

/// 根目录名（服务端生成；`w-` + 12 位十六进制，或保留名 `public`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RootName(String);

impl RootName {
    /// 由工作区标识派生根名：`w-` + 12 位小写十六进制。
    #[must_use]
    pub fn for_workspace(id: WorkspaceId) -> Self {
        let raw = id.as_raw();
        Self(format!("w-{raw:012x}"))
    }

    /// 保留工作区 `public` 的根名。
    #[must_use]
    pub fn public() -> Self {
        Self("public".to_owned())
    }

    /// 解析并校验根名（供注册表装载等服务端路径使用）。
    ///
    /// 只接受规范形态；`w-` 后的 12 位必须是**非零**的小写十六进制
    /// （对应 1..=2⁴⁸−1 的工作区序列域）。
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        if s == "public" {
            return Some(Self(s.to_owned()));
        }
        let hex = s.strip_prefix("w-")?;
        if hex.len() != 12 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        if hex.bytes().all(|b| b == b'0') {
            return None;
        }
        Some(Self(s.to_owned()))
    }

    /// 文本形态。
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RootName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 工作区根目录句柄：`base/name`。
///
/// `base` 是平台管理的数据根（实现期由部署配置给出）；句柄本身只做路径组合，
/// 不读取任何用户输入。
#[derive(Debug, Clone)]
pub struct WorkspaceRoot {
    name: RootName,
    path: PathBuf,
}

impl WorkspaceRoot {
    /// 组合 `base` 与 `name`（纯路径运算，无 IO）。
    #[must_use]
    pub fn new(base: &Path, name: RootName) -> Self {
        let path = base.join(name.as_str());
        Self { name, path }
    }

    /// 根名。
    #[must_use]
    pub fn name(&self) -> &RootName {
        &self.name
    }

    /// 绝对路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 创建（或校验）根目录与 [`WorkspaceDir::ALL`] 十个子目录。
    ///
    /// 规则（总体方案 §4）：
    /// - 目录权限收紧为 `0700`（不依赖 umask）；
    /// - 目标**已存在但不是目录**（含**符号链接**）→ 拒绝；
    /// - 幂等：重复调用成功。
    pub fn create_layout(&self) -> io::Result<()> {
        ensure_private_dir(&self.path)?;
        for dir in WorkspaceDir::ALL {
            ensure_private_dir(&self.path.join(dir.name()))?;
        }
        Ok(())
    }
}

/// 确保 `path` 是一个权限为 `0700` 的目录；符号链接与普通文件一律拒绝。
fn ensure_private_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{path:?} 已存在但不是目录（可能为符号链接或普通文件）"),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(path)?;
        }
        Err(e) => return Err(e),
    }
    // 显式收紧权限：umask 不应成为接口行为的一部分（既有目录同样收紧）。
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_name_derivation() {
        let id = WorkspaceId::from_raw(1).unwrap();
        assert_eq!(RootName::for_workspace(id).as_str(), "w-000000000001");
        let id = WorkspaceId::from_raw((1 << 48) - 1).unwrap();
        assert_eq!(RootName::for_workspace(id).as_str(), "w-ffffffffffff");
        assert_eq!(RootName::public().as_str(), "public");
    }

    #[test]
    fn root_name_parse_accepts_only_canonical_forms() {
        assert!(RootName::parse("public").is_some());
        assert!(RootName::parse("w-000000000001").is_some());
        assert!(RootName::parse("w-ffffffffffff").is_some());
    }

    #[test]
    fn root_name_parse_rejects_user_input() {
        for bad in [
            "",
            "..",
            "../etc",
            "/absolute",
            "w-",
            "w-short",
            "w-0123456789abc", // 13 位
            "w-0123456789a",   // 11 位
            "w-0123456789AB",  // 大写
            "W-0123456789ab",  // 前缀大写
            "w-000000000000",  // 全零（0 保留为“无”）
            "w-0123456789gz",  // 非十六进制
            "public/..",
            "public ",
            "w-0123456789ab/../../etc",
        ] {
            assert!(RootName::parse(bad).is_none(), "应拒绝：{bad:?}");
        }
    }
}
