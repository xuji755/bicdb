//! 工作区上下文：`WorkspaceContext`（总体方案 §14 的模块接口行）。
//!
//! **由认证与监督器生成，业务不可修改**——字段全部私有，只提供只读访问。
//! 引擎内的任何接口都不带"身份 / 工作区"覆盖参数；需要上下文的地方经此处传入。

use crate::id::{UserId, WorkspaceId};
use crate::quota::Quota;
use crate::root::WorkspaceRoot;

/// 工作区上下文：主体、工作区、根句柄与配额。
#[derive(Debug, Clone)]
pub struct WorkspaceContext {
    user: UserId,
    workspace: WorkspaceId,
    root: WorkspaceRoot,
    quota: Quota,
}

impl WorkspaceContext {
    /// 由认证结果与监督器构造。
    #[must_use]
    pub fn new(user: UserId, workspace: WorkspaceId, root: WorkspaceRoot, quota: Quota) -> Self {
        Self {
            user,
            workspace,
            root,
            quota,
        }
    }

    /// 主体（已认证）。
    #[must_use]
    pub fn user(&self) -> UserId {
        self.user
    }

    /// 工作区。
    #[must_use]
    pub fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    /// 根目录句柄。
    #[must_use]
    pub fn root(&self) -> &WorkspaceRoot {
        &self.root
    }

    /// 四条配额。
    #[must_use]
    pub fn quota(&self) -> Quota {
        self.quota
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::root::RootName;
    use std::path::Path;

    #[test]
    fn context_exposes_readonly_fields() {
        let user = UserId::parse("018f2a7c-3b4d-7e01-9a2b-c3d4e5f60718").unwrap();
        let ws = WorkspaceId::from_raw(7).unwrap();
        let root = WorkspaceRoot::new(Path::new("/srv/bicdb/workspaces"), RootName::public());
        let quota = Quota::new(1, 2, 3, 4);
        let ctx = WorkspaceContext::new(user, ws, root, quota);

        assert_eq!(ctx.user(), user);
        assert_eq!(ctx.workspace(), ws);
        assert_eq!(ctx.quota(), quota);
        assert_eq!(ctx.root().name().as_str(), "public");
    }
}
