//! 资源上限的**取值与校验**（`NFR` REQ-RES-001；P0 冻结决议第 8 项）。
//!
//! 双层额度（实例总上限 + 工作区上限）在这里只是**取值**；"谁在什么时刻
//! 按哪一层拒绝"由 [`crate::supervisor`] 落地。实例级数值照 P0 冻结；
//! 工作区的存储四条在 `ws$`（`bicdb_workspace::Quota`），执行侧的工作区额度
//! （`max_task_queue` 等）在此。

/// 实例执行线程数（P0 冻结：8）。
pub const P0_EXECUTION_THREADS: usize = 8;

/// 实例最大连接数（P0 冻结：512）。
pub const P0_MAX_CONNECTIONS: usize = 512;

/// 实例缓存预算（P0 冻结：16 GiB）。
pub const P0_CACHE_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// 活跃工作区默认值（P0 冻结：默认 8）。
pub const P0_MAX_ACTIVE_WORKSPACES: usize = 8;

/// 活跃工作区**上限**（P0 冻结：上限 32；配置只能在此之内调整）。
pub const P0_MAX_ACTIVE_WORKSPACES_CEILING: usize = 32;

/// 每工作区任务队列额度（P0 冻结：1024）。
pub const P0_MAX_TASK_QUEUE: usize = 1024;

/// 上限配置不合法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitsError {
    /// 执行线程数必须 ≥ 1。
    NoExecutionThreads,
    /// 活跃工作区上限必须 ≥ 1。
    NoActiveWorkspaces,
    /// 活跃工作区**不得超过 P0 上限（32）**——防止把预算校验调空。
    ActiveWorkspacesAboveCeiling {
        /// 试图设置的值。
        requested: usize,
        /// P0 上限。
        ceiling: usize,
    },
    /// 每工作区任务队列额度必须 ≥ 1。
    NoTaskQueue,
}

/// 实例级上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceLimits {
    /// 执行线程数（有界执行池的线程数）。
    pub execution_threads: usize,
    /// 最大连接数（接入层消费；P1 只携带）。
    pub max_connections: usize,
    /// 活跃工作区数上限（默认 8 / 上限 32）。
    pub max_active_workspaces: usize,
    /// 每工作区任务队列额度（在途 + 排队）。
    pub max_task_queue_per_workspace: usize,
    /// 实例缓存预算（字节；P2 起由缓冲池消费）。
    pub cache_bytes: u64,
}

impl InstanceLimits {
    /// P0 冻结的默认值。
    #[must_use]
    pub fn p0_defaults() -> Self {
        Self {
            execution_threads: P0_EXECUTION_THREADS,
            max_connections: P0_MAX_CONNECTIONS,
            max_active_workspaces: P0_MAX_ACTIVE_WORKSPACES,
            max_task_queue_per_workspace: P0_MAX_TASK_QUEUE,
            cache_bytes: P0_CACHE_BYTES,
        }
    }

    /// 校验（启动时执行：拒绝把额度调空或越过 P0 上限）。
    pub fn validate(&self) -> Result<(), LimitsError> {
        if self.execution_threads == 0 {
            return Err(LimitsError::NoExecutionThreads);
        }
        if self.max_active_workspaces == 0 {
            return Err(LimitsError::NoActiveWorkspaces);
        }
        if self.max_active_workspaces > P0_MAX_ACTIVE_WORKSPACES_CEILING {
            return Err(LimitsError::ActiveWorkspacesAboveCeiling {
                requested: self.max_active_workspaces,
                ceiling: P0_MAX_ACTIVE_WORKSPACES_CEILING,
            });
        }
        if self.max_task_queue_per_workspace == 0 {
            return Err(LimitsError::NoTaskQueue);
        }
        Ok(())
    }
}

impl Default for InstanceLimits {
    fn default() -> Self {
        Self::p0_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p0_defaults_validate() {
        assert!(InstanceLimits::p0_defaults().validate().is_ok());
    }

    #[test]
    fn ceiling_cannot_be_raised_by_configuration() {
        let mut limits = InstanceLimits::p0_defaults();
        limits.max_active_workspaces = P0_MAX_ACTIVE_WORKSPACES_CEILING;
        assert!(limits.validate().is_ok(), "上限值本身可用");
        limits.max_active_workspaces = P0_MAX_ACTIVE_WORKSPACES_CEILING + 1;
        assert_eq!(
            limits.validate(),
            Err(LimitsError::ActiveWorkspacesAboveCeiling {
                requested: 33,
                ceiling: 32
            })
        );
    }

    #[test]
    fn degenerate_values_are_rejected() {
        let mut limits = InstanceLimits::p0_defaults();
        limits.execution_threads = 0;
        assert_eq!(limits.validate(), Err(LimitsError::NoExecutionThreads));
        let mut limits = InstanceLimits::p0_defaults();
        limits.max_active_workspaces = 0;
        assert_eq!(limits.validate(), Err(LimitsError::NoActiveWorkspaces));
        let mut limits = InstanceLimits::p0_defaults();
        limits.max_task_queue_per_workspace = 0;
        assert_eq!(limits.validate(), Err(LimitsError::NoTaskQueue));
    }
}
