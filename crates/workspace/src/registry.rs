//! 工作区登记与路由：`subject → 本人的工作区`（`ISO` REQ-ISO-001/002/006）。
//!
//! # 权威与形态
//!
//! 权威登记在 `public` 的 `ws$`（存储架构 §2.11；表的三列要点：
//! `workspace_id` 是**身份**、`name` 是**可改的标签**、属主 `user_id`
//! **不是唯一索引**——一个主体可以有多个工作区，REQ-ISO-012 的克隆要求）。
//! 本模块是运行期投影：**P1 为内存实现**（平台装载），P2 起由同一接口
//! 挂到 `ws$` 的读路径上。
//!
//! # 三条纪律（都在类型与错误里落地）
//!
//! 1. **路由是身份的函数**：入参是 [`AuthenticatedSubject`] 与名称，
//!    **没有 user_id 参数**（REQ-ISO-002）；
//! 2. **他人的工作区报"不存在"，不得报"无权限"**（REQ-ISO-006）——
//!    与"名字压根不存在"给出**不可区分**的同一错误；
//! 3. **名字只在同属主内唯一，跨属主不查重**（存储架构 §2.11）——
//!    跨属主查重会让一次改名变成一次存在性探测。
//!
//! `admin` 不经本模块跨工作区访问——管理走 `OPS` §0.4 的管理通道，
//! 那是**另一条**受审计的入口，不在路由函数里开洞。

use crate::id::{UserId, WorkspaceId};
use crate::identity::AuthenticatedSubject;
use crate::root::{RootName, WorkspaceRoot};

/// 工作区登记条目（`ws$` 一行的运行期投影）。
#[derive(Debug, Clone)]
pub struct WorkspaceEntry {
    id: WorkspaceId,
    owner: UserId,
    name: Option<String>,
    root: WorkspaceRoot,
}

impl WorkspaceEntry {
    /// 构造条目。`name` 为 `None` 表示**未命名**（存储架构 §2.11：
    /// 显示时回退到属主名；`SET NAME = NULL` 即回到跟随）。
    #[must_use]
    pub fn new(id: WorkspaceId, owner: UserId, name: Option<String>, root: WorkspaceRoot) -> Self {
        Self {
            id,
            owner,
            name,
            root,
        }
    }

    /// 工作区标识（**唯一持久的身份**；持久引用不得用名字）。
    #[must_use]
    pub fn id(&self) -> WorkspaceId {
        self.id
    }

    /// 属主。
    #[must_use]
    pub fn owner(&self) -> UserId {
        self.owner
    }

    /// 名称标签（`None` = 未命名）。
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// 根目录句柄。
    #[must_use]
    pub fn root(&self) -> &WorkspaceRoot {
        &self.root
    }
}

/// 登记期错误（装载 / 建工作区时；不是运行期路由错误）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    /// 同一属主内已有同名条目（**含未命名槽位**：未命名按"一个默认槽位"处理）。
    DuplicateName,
    /// `public` 是保留名，普通工作区不得占用。
    ReservedName,
    /// 工作区标识已登记（标识**全局唯一**，跨属主同样拒绝）。
    DuplicateId,
    /// `public` 已经登记过。
    PublicAlreadyRegistered,
    /// `public` 的根名不是保留根名 `public`（装配错误）。
    PublicRootMisnamed,
}

/// 路由失败：**只有"不存在"一种**。
///
/// "名字属于他人"与"名字压根不存在"必须落入同一变体——即两者在
/// 调用方看来**不可区分**（`ISO` REQ-ISO-006；也见存储架构 §2.11
/// "不做跨属主唯一性检查"的同一理由）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingError {
    /// 该主体名下不存在这个名字（或该名字属于他人）。
    NotFound,
}

/// 路由结果：主体自己的工作区，或保留工作区 `public`。
#[derive(Debug)]
pub enum Routed<'a> {
    /// 主体自己的工作区（全部权限）。
    Owned(&'a WorkspaceEntry),
    /// 保留工作区 `public`（任何已认证主体可达；**只读**由过滤层执行，
    /// `ISO` REQ-ISO-004）。
    Public(&'a WorkspaceRoot),
}

impl Routed<'_> {
    /// 根目录句柄（两类结果都给出）。
    #[must_use]
    pub fn root(&self) -> &WorkspaceRoot {
        match self {
            Routed::Owned(entry) => entry.root(),
            Routed::Public(root) => root,
        }
    }
}

/// 工作区登记表（P1 内存实现；装载完成后只读）。
///
/// 容量按 P0 冻结的活跃工作区档位（默认 8 / 上限 32），线性扫描即够；
/// 索引化留待需要时再谈。
#[derive(Debug, Default)]
pub struct WorkspaceRegistry {
    public: Option<WorkspaceRoot>,
    owned: Vec<WorkspaceEntry>,
}

/// 保留工作区的名字（`public` 工作区；也是保留根名）。
pub const PUBLIC_WORKSPACE: &str = "public";

impl WorkspaceRegistry {
    /// 空登记表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记保留工作区 `public` 的根（实例初始化时一次）。
    pub fn register_public(&mut self, root: WorkspaceRoot) -> Result<(), RegistryError> {
        if self.public.is_some() {
            return Err(RegistryError::PublicAlreadyRegistered);
        }
        if root.name() != &RootName::public() {
            return Err(RegistryError::PublicRootMisnamed);
        }
        self.public = Some(root);
        Ok(())
    }

    /// 登记普通工作区。
    ///
    /// 拒绝：保留名 `public`；同一属主内重名（含未命名槽位）；标识重复。
    /// **跨属主不查重**——同名工作区属于不同主体是合法的、也是设计使然。
    pub fn register(&mut self, entry: WorkspaceEntry) -> Result<(), RegistryError> {
        if entry.name.as_deref() == Some(PUBLIC_WORKSPACE) {
            return Err(RegistryError::ReservedName);
        }
        if self.owned.iter().any(|e| e.id == entry.id) {
            return Err(RegistryError::DuplicateId);
        }
        if self
            .owned
            .iter()
            .any(|e| e.owner == entry.owner && e.name == entry.name)
        {
            return Err(RegistryError::DuplicateName);
        }
        self.owned.push(entry);
        Ok(())
    }

    /// 路由：把已认证主体解析到工作区。
    ///
    /// - `name = Some("public")` → [`Routed::Public`]（任何已认证主体可达）；
    /// - `name = Some(n)` → 该主体自己名下名为 `n` 的工作区；
    /// - `name = None` → 该主体自己的**未命名**槽位；
    /// - 其余（含"别人的名字"）→ [`RoutingError::NotFound`]，
    ///   且与"不存在"**不可区分**。
    pub fn route(
        &self,
        subject: &AuthenticatedSubject,
        name: Option<&str>,
    ) -> Result<Routed<'_>, RoutingError> {
        if name == Some(PUBLIC_WORKSPACE) {
            return match self.public.as_ref() {
                Some(root) => Ok(Routed::Public(root)),
                None => Err(RoutingError::NotFound),
            };
        }
        self.owned
            .iter()
            .find(|e| e.owner == subject.user() && e.name.as_deref() == name)
            .map(Routed::Owned)
            .ok_or(RoutingError::NotFound)
    }

    /// 已登记条目数（含 `public` 与否不影响；装载自检用）。
    #[must_use]
    pub fn owned_len(&self) -> usize {
        self.owned.len()
    }

    /// `public` 是否已登记。
    #[must_use]
    pub fn has_public(&self) -> bool {
        self.public.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn subject(raw: u64) -> AuthenticatedSubject {
        AuthenticatedSubject::new(UserId::from_raw(raw).unwrap())
    }

    fn entry(id: u64, owner: u64, name: Option<&str>) -> WorkspaceEntry {
        WorkspaceEntry::new(
            WorkspaceId::from_raw(id).unwrap(),
            UserId::from_raw(owner).unwrap(),
            name.map(str::to_owned),
            WorkspaceRoot::new(
                Path::new("/srv"),
                RootName::for_workspace(WorkspaceId::from_raw(id).unwrap()),
            ),
        )
    }

    const A: u64 = 1;
    const B: u64 = 2;

    #[test]
    fn registration_guards() {
        let mut reg = WorkspaceRegistry::new();
        reg.register(entry(1, A, Some("main"))).unwrap();
        assert_eq!(
            reg.register(entry(2, A, Some("main"))),
            Err(RegistryError::DuplicateName),
            "同属主同名拒绝"
        );
        assert_eq!(
            reg.register(entry(3, A, Some("public"))),
            Err(RegistryError::ReservedName)
        );
        assert_eq!(
            reg.register(entry(1, B, Some("other"))),
            Err(RegistryError::DuplicateId),
            "标识全局唯一"
        );
        // 跨属主同名：合法。
        reg.register(entry(4, B, Some("main"))).unwrap();
        // 一个主体多个工作区：合法（REQ-ISO-012）。
        reg.register(entry(5, A, Some("clone"))).unwrap();
    }

    #[test]
    fn unnamed_slot_is_single() {
        let mut reg = WorkspaceRegistry::new();
        reg.register(entry(1, A, None)).unwrap();
        assert_eq!(
            reg.register(entry(2, A, None)),
            Err(RegistryError::DuplicateName),
            "未命名按一个默认槽位处理"
        );
        reg.register(entry(3, B, None)).unwrap();
    }

    #[test]
    fn public_root_must_be_the_reserved_root() {
        let mut reg = WorkspaceRegistry::new();
        let wrong = WorkspaceRoot::new(
            Path::new("/srv"),
            RootName::for_workspace(WorkspaceId::from_raw(9).unwrap()),
        );
        assert_eq!(
            reg.register_public(wrong),
            Err(RegistryError::PublicRootMisnamed)
        );
        reg.register_public(WorkspaceRoot::new(Path::new("/srv"), RootName::public()))
            .unwrap();
        assert_eq!(
            reg.register_public(WorkspaceRoot::new(Path::new("/srv"), RootName::public())),
            Err(RegistryError::PublicAlreadyRegistered)
        );
    }

    #[test]
    fn route_foreign_name_is_indistinguishable_from_missing() {
        let mut reg = WorkspaceRegistry::new();
        reg.register(entry(1, A, Some("main"))).unwrap();
        reg.register(entry(2, B, Some("bobs-secret"))).unwrap();

        let alice = subject(A);
        assert!(matches!(
            reg.route(&alice, Some("main")),
            Ok(Routed::Owned(_))
        ));
        let foreign = reg.route(&alice, Some("bobs-secret"));
        let missing = reg.route(&alice, Some("no-such-name"));
        assert_eq!(
            foreign.as_ref().err(),
            missing.as_ref().err(),
            "两种失败必须不可区分"
        );
        assert_eq!(foreign.err(), Some(RoutingError::NotFound));
    }
}
