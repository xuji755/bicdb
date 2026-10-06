//! 工作区四条配额（`ws$`；总体方案 §2.2）：数据 / undo / temp / 资产。
//!
//! 实例级的资源族（`max_buffers`、`max_connections` 等）不在此类型内——
//! 它们由监督器的公平调度持有；本类型只表达**每工作区一行的四个数**。

/// 工作区配额（单位：字节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    data: u64,
    undo: u64,
    temp: u64,
    asset: u64,
}

impl Quota {
    /// 构造四条配额（单位：字节）。
    #[must_use]
    pub fn new(data: u64, undo: u64, temp: u64, asset: u64) -> Self {
        Self {
            data,
            undo,
            temp,
            asset,
        }
    }

    /// 数据配额（字节）。
    #[must_use]
    pub fn data(&self) -> u64 {
        self.data
    }

    /// Undo 配额（字节）。
    #[must_use]
    pub fn undo(&self) -> u64 {
        self.undo
    }

    /// 临时空间配额（字节）。
    #[must_use]
    pub fn temp(&self) -> u64 {
        self.temp
    }

    /// 资产配额（字节）。
    #[must_use]
    pub fn asset(&self) -> u64 {
        self.asset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_roundtrip() {
        let q = Quota::new(1 << 30, 1 << 28, 1 << 28, 100 << 30);
        assert_eq!(q.data(), 1 << 30);
        assert_eq!(q.undo(), 1 << 28);
        assert_eq!(q.temp(), 1 << 28);
        assert_eq!(q.asset(), 100 << 30);
    }
}
