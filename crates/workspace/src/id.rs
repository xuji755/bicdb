//! 标识类型：`UserId`（主体，UUID 形态）与 `WorkspaceId`（工作区，48 位序列），
//! 以及**工作区在数据文件里的 8 字节标识** [`workspace_ref`]。
//!
//! 宽度与序列的约定见 `CONV` §1.1 / §1.2：**48 位、从 1 起、0 保留为"无"、不回绕**；
//! 对外文本表示按**十进制**。

use std::fmt;

/// 48 位上限（`CONV` §1.2 的宽度约定：与提交序号、LSN、ROWID 同为 48 位）。
pub const WORKSPACE_ID_MAX: u64 = (1 << 48) - 1;

/// 工作区标识（48 位；由 `public` 的 `seq$` 分配——实例级序列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceId(u64);

impl WorkspaceId {
    /// 从原始数值构造。`0`（保留为"无"）或越过 48 位上限时返回 `None`。
    #[must_use]
    pub fn from_raw(raw: u64) -> Option<Self> {
        if raw == 0 || raw > WORKSPACE_ID_MAX {
            None
        } else {
            Some(Self(raw))
        }
    }

    /// 原始数值。
    #[must_use]
    pub fn as_raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// **工作区标识（数据血统根）**：`workspace_ref = H(workspace_id)` 截断至 8 字节。
///
/// 依据 `doc/arch/07-ROWID与工作区标识.md` §7.2 与 `doc/workspace_ref设计_v0.1.md`：
/// - `H` = **SHA-256**（**本实现冻结**：见 [`bicdb_common::sha256`] 的模块文档——有官方
///   测试向量、无第三方依赖；原设计留的"P0 冻结具体算法"在此落定），**取前 8 字节**；
/// - **只在打开工作区时算一次并缓存**（每页访问只是一次 8 字节比较，热路径零额外开销）；
/// - **它是"数据血统根"，不是"当前身份"**：克隆出的工作区沿用源的 `workspace_ref`
///   （它的数据确实源自同一血统）。本函数只按 `workspace_id` 算——
///   克隆时**不要**用它，要沿用源值。
/// - **不是密钥**，不构成密码学边界（同文档"威胁模型"）：它把"静默的跨区数据串读"
///   降级为"可检出的错误"。
#[must_use]
pub fn workspace_ref(id: WorkspaceId) -> [u8; 8] {
    let d = bicdb_common::sha256::digest(&id.as_raw().to_le_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&d[..8]);
    out
}

/// 主体标识（48 位实例级序列值；`CONV` §1.1/§1.2——由 `public` 的 `seq$` 分配，
/// 与 `workspace_id` 同规：**从 1 起、0 保留为"无"、不回绕**；文本按十进制）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserId(u64);

impl UserId {
    /// 从原始数值构造。`0`（保留为"无"）或越过 48 位上限时返回 `None`。
    #[must_use]
    pub fn from_raw(raw: u64) -> Option<Self> {
        if raw == 0 || raw > WORKSPACE_ID_MAX {
            None
        } else {
            Some(Self(raw))
        }
    }

    /// 原始数值。
    #[must_use]
    pub fn as_raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_id_bounds() {
        assert!(WorkspaceId::from_raw(0).is_none(), "0 保留为『无』");
        assert!(WorkspaceId::from_raw(1).is_some());
        assert!(WorkspaceId::from_raw(WORKSPACE_ID_MAX).is_some());
        assert!(WorkspaceId::from_raw(WORKSPACE_ID_MAX + 1).is_none());
    }

    #[test]
    fn workspace_id_text_is_decimal() {
        assert_eq!(WorkspaceId::from_raw(12345).unwrap().to_string(), "12345");
    }

    #[test]
    fn user_id_has_the_same_shape_as_workspace_id() {
        assert!(UserId::from_raw(0).is_none(), "0 保留为『无』");
        assert!(UserId::from_raw(1).is_some());
        assert!(UserId::from_raw(WORKSPACE_ID_MAX).is_some());
        assert!(UserId::from_raw(WORKSPACE_ID_MAX + 1).is_none());
        assert_eq!(
            UserId::from_raw(42).unwrap().to_string(),
            "42",
            "文本十进制"
        );
    }

    #[test]
    fn refs_are_stable_distinct_and_decoupled_from_the_raw_id() {
        let a = workspace_ref(WorkspaceId::from_raw(1).unwrap());
        let b = workspace_ref(WorkspaceId::from_raw(2).unwrap());
        assert_ne!(a, b, "不同工作区的 ref 必须不同（哈希的目的）");
        assert_eq!(a, workspace_ref(WorkspaceId::from_raw(1).unwrap()));
        // 连号 id 的 ref 不相邻（低比特不暴露）：
        assert_ne!(a[..7], b[..7], "连号的哈希不该只差最后一字节");
        // 具体值钉住（`sha256(LE64(1))` 前 8 字节；与 Python `hashlib` 独立核对过）
        // ——换哈希、换字节序、换截断方式，这里立刻变红。
        assert_eq!(a, [0x7c, 0x9f, 0xa1, 0x36, 0xd4, 0x41, 0x3f, 0xa6]);
        assert_eq!(b, [0xd8, 0x6e, 0x81, 0x12, 0xf3, 0xc4, 0xc4, 0x44]);
    }
}
