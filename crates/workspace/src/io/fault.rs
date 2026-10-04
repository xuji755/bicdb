//! 故障注入：包装任意 [`FileIo`]，按**第 n 次操作**确定性注入失败。
//!
//! 设计依据：总体方案 §17（P1 的"故障注入"）与 `NFR` REQ-REL-006
//! （进程 / 线程 / **I/O 层** / 断电模拟的多层次注入）。
//!
//! 确定性是硬要求：故障按**每类操作的调用序**触发，同一测试序列必得同一
//! 结果，不做随机、不依赖时序。三类动作对应真实世界的三类现场：
//!
//! | 动作 | 模拟的现场 |
//! | --- | --- |
//! | [`FaultAction::Error`] | 操作未发生即失败（`EIO`、`ENOSPC`……） |
//! | [`FaultAction::TornWrite`] | **部分写之后失败**（撕裂写——崩溃恢复的经典输入） |
//! | [`FaultAction::ShortRead`] | 短读（成功，但少于请求字节数） |

use std::io;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use super::{FileHandle, FileIo, OpenOptions};

/// 可注入故障的操作类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultOp {
    /// `open`。
    Open,
    /// `open_dir`。
    OpenDir,
    /// `read_at`。
    Read,
    /// `write_at`。
    Write,
    /// `size`。
    Size,
    /// `set_len`。
    SetLen,
    /// `sync_data`。
    SyncData,
    /// `sync_all`。
    SyncAll,
    /// `sync_dir`。
    SyncDir,
    /// `close`。
    Close,
}

impl FaultOp {
    /// 操作类别数。
    pub const COUNT: usize = 10;

    fn index(self) -> usize {
        self as usize
    }
}

/// 注入动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultAction {
    /// 操作不产生任何效果，直接返回该错误。
    Error(io::ErrorKind),
    /// **部分写**：只写入前 `bytes` 字节，随后返回错误（仅 `Write` 有意义）。
    TornWrite {
        /// 先成功写入的字节数。
        bytes: usize,
        /// 随后返回的错误类别。
        kind: io::ErrorKind,
    },
    /// **短读**：只读取并返回前 `bytes` 字节（成功，仅 `Read` 有意义）。
    ShortRead {
        /// 返回的字节数上限。
        bytes: usize,
    },
}

/// 一条故障规则：从第 `from` 次该操作起，连续 `times` 次注入 `action`。
#[derive(Debug, Clone, Copy)]
pub struct FaultRule {
    op: FaultOp,
    from: u64,
    times: u64,
    action: FaultAction,
}

impl FaultRule {
    /// 从第 `from` 次起，连续 `times` 次返回 `kind`。
    #[must_use]
    pub fn error(op: FaultOp, from: u64, times: u64, kind: io::ErrorKind) -> Self {
        Self {
            op,
            from,
            times,
            action: FaultAction::Error(kind),
        }
    }

    /// 仅第 `nth` 次返回 `kind`。
    #[must_use]
    pub fn once(op: FaultOp, nth: u64, kind: io::ErrorKind) -> Self {
        Self::error(op, nth, 1, kind)
    }

    /// 从第 `from` 次起，此后一直返回 `kind`。
    #[must_use]
    pub fn fail_from(op: FaultOp, from: u64, kind: io::ErrorKind) -> Self {
        Self::error(op, from, u64::MAX, kind)
    }

    /// 仅第 `from` 次 `write_at`：先写入前 `bytes` 字节，随后返回 `kind`。
    #[must_use]
    pub fn torn_write(from: u64, bytes: usize, kind: io::ErrorKind) -> Self {
        Self {
            op: FaultOp::Write,
            from,
            times: 1,
            action: FaultAction::TornWrite { bytes, kind },
        }
    }

    /// 仅第 `from` 次 `read_at`：成功返回前 `bytes` 字节（短读）。
    #[must_use]
    pub fn short_read(from: u64, bytes: usize) -> Self {
        Self {
            op: FaultOp::Read,
            from,
            times: 1,
            action: FaultAction::ShortRead { bytes },
        }
    }

    fn matches(&self, op: FaultOp, nth: u64) -> bool {
        self.op == op && nth >= self.from && nth - self.from < self.times
    }
}

struct FaultState {
    counters: [u64; FaultOp::COUNT],
    rules: Vec<FaultRule>,
}

impl Default for FaultState {
    fn default() -> Self {
        Self {
            counters: [0; FaultOp::COUNT],
            rules: Vec::new(),
        }
    }
}

/// 故障注入包装：`FaultInjecting<OsFileIo>`、`FaultInjecting<MemFileIo>`……
#[derive(Default)]
pub struct FaultInjecting<Io> {
    inner: Io,
    state: Mutex<FaultState>,
}

impl<Io: FileIo> FaultInjecting<Io> {
    /// 包装一个实现（初始无规则）。
    pub fn new(inner: Io) -> Self {
        Self {
            inner,
            state: Mutex::new(FaultState::default()),
        }
    }

    /// 追加一条规则。
    pub fn add_rule(&self, rule: FaultRule) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.rules.push(rule);
    }

    /// 清空各类操作的调用计数（规则保留）。
    pub fn reset_counters(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.counters = [0; FaultOp::COUNT];
    }

    /// 底层实现（供测试检查实际落盘结果等）。
    pub fn inner(&self) -> &Io {
        &self.inner
    }

    /// 推进计数并判定命中。
    fn observe(&self, op: FaultOp) -> Option<FaultAction> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.counters[op.index()] += 1;
        let nth = state.counters[op.index()];
        state
            .rules
            .iter()
            .find(|r| r.matches(op, nth))
            .map(|r| r.action)
    }

    /// 只有 `Error` 动作适用的操作的统一外壳。
    fn guarded<T>(
        &self,
        op: FaultOp,
        name: &'static str,
        f: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        if let Some(FaultAction::Error(kind)) = self.observe(op) {
            return Err(injected(name, kind));
        }
        f()
    }
}

fn injected(op: &str, kind: io::ErrorKind) -> io::Error {
    io::Error::new(kind, format!("注入故障：{op}"))
}

impl<Io: FileIo> FileIo for FaultInjecting<Io> {
    fn open(&self, path: &Path, opts: OpenOptions) -> io::Result<FileHandle> {
        self.guarded(FaultOp::Open, "open", || self.inner.open(path, opts))
    }

    fn open_dir(&self, path: &Path) -> io::Result<FileHandle> {
        self.guarded(FaultOp::OpenDir, "open_dir", || self.inner.open_dir(path))
    }

    fn read_at(&self, handle: FileHandle, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        match self.observe(FaultOp::Read) {
            Some(FaultAction::Error(kind)) => Err(injected("read_at", kind)),
            Some(FaultAction::ShortRead { bytes }) => {
                let n = bytes.min(buf.len());
                self.inner.read_at(handle, &mut buf[..n], offset)
            }
            _ => self.inner.read_at(handle, buf, offset),
        }
    }

    fn write_at(&self, handle: FileHandle, buf: &[u8], offset: u64) -> io::Result<()> {
        match self.observe(FaultOp::Write) {
            Some(FaultAction::Error(kind)) => Err(injected("write_at", kind)),
            Some(FaultAction::TornWrite { bytes, kind }) => {
                let n = bytes.min(buf.len());
                self.inner.write_at(handle, &buf[..n], offset)?;
                Err(injected("write_at", kind))
            }
            _ => self.inner.write_at(handle, buf, offset),
        }
    }

    fn size(&self, handle: FileHandle) -> io::Result<u64> {
        self.guarded(FaultOp::Size, "size", || self.inner.size(handle))
    }

    fn set_len(&self, handle: FileHandle, len: u64) -> io::Result<()> {
        self.guarded(FaultOp::SetLen, "set_len", || {
            self.inner.set_len(handle, len)
        })
    }

    fn sync_data(&self, handle: FileHandle) -> io::Result<()> {
        self.guarded(FaultOp::SyncData, "sync_data", || {
            self.inner.sync_data(handle)
        })
    }

    fn sync_all(&self, handle: FileHandle) -> io::Result<()> {
        self.guarded(FaultOp::SyncAll, "sync_all", || self.inner.sync_all(handle))
    }

    fn sync_dir(&self, handle: FileHandle) -> io::Result<()> {
        self.guarded(FaultOp::SyncDir, "sync_dir", || self.inner.sync_dir(handle))
    }

    fn close(&self, handle: FileHandle) -> io::Result<()> {
        self.guarded(FaultOp::Close, "close", || self.inner.close(handle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::MemFileIo;
    use std::path::PathBuf;

    #[test]
    fn rule_window_arithmetic() {
        let rule = FaultRule::error(FaultOp::Write, 2, 3, io::ErrorKind::Other);
        assert!(!rule.matches(FaultOp::Write, 1));
        assert!(rule.matches(FaultOp::Write, 2));
        assert!(rule.matches(FaultOp::Write, 4));
        assert!(!rule.matches(FaultOp::Write, 5));
        assert!(!rule.matches(FaultOp::Read, 2), "类别不同不命中");
    }

    #[test]
    fn fail_from_covers_everything_after() {
        let rule = FaultRule::fail_from(FaultOp::Write, 2, io::ErrorKind::Other);
        assert!(!rule.matches(FaultOp::Write, 1));
        assert!(rule.matches(FaultOp::Write, 2));
        assert!(rule.matches(FaultOp::Write, 1_000_000));
    }

    #[test]
    fn torn_write_leaves_partial_data() {
        let dir = PathBuf::from("/mem");
        let inner = MemFileIo::new();
        inner.add_dir(dir.clone());
        let io = FaultInjecting::new(inner);
        io.add_rule(FaultRule::torn_write(1, 3, io::ErrorKind::Other));

        let h = io
            .open(
                &dir.join("a.dat"),
                OpenOptions::new().write(true).create(true),
            )
            .unwrap();
        let err = io.write_at(h, b"abcdefg", 0).expect_err("第一次写即撕裂");
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert_eq!(
            io.inner().contents(&dir.join("a.dat")).unwrap(),
            b"abc".to_vec(),
            "前三字节已写入"
        );
    }
}
