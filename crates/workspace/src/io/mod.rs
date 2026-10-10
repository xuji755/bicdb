//! FileIO 替换接口：引擎唯一的文件操作入口（P1 测试底座）。
//!
//! 设计依据：`ISO` REQ-ISO-007 / REQ-ISO-008、总体方案 §17（P1 的
//! "FileIO 替换接口、故障注入"）。
//!
//! # 契约
//!
//! - **只有 [`FileIo::open`] 与 [`FileIo::open_dir`] 接受路径**；符号链接一律
//!   **拒绝打开**（`PermissionDenied`），且打开后复核"路径当前指向的对象"
//!   与**已打开句柄**同源（`dev`/`ino` 一致）——破坏性动作（截断）只在复核
//!   之后执行。之后的一切操作基于 [`FileHandle`]。这是 "**以句柄为准，
//!   不以路径为准**" 在接口形状上的落点（实现细节见 [`OsFileIo`]）。
//! - 句柄**单调分配、绝不复用**：误用已关闭的旧句柄得到 `NotFound`，
//!   而不会命中后来打开的其他文件（"句柄不串区"）。
//! - **短读是正常现象**：`read_at` 返回实际读取的字节数（`0` = 文件末尾）；
//!   `write_at` 则是**全量或错误**（内部处理短写），出错时文件可能已被部分修改。
//! - 接口**对象安全**：`dyn FileIo` 可整体替换、可直接包装。
//!
//! # 三类实现
//!
//! | 实现 | 用途 |
//! | --- | --- |
//! | [`OsFileIo`] | 生产：真实文件系统（拒绝符号链接 + 打开后按句柄复核） |
//! | [`MemFileIo`] | 测试：内存后端，同一套契约 |
//! | [`FaultInjecting`] | 故障注入：包装任意实现，按第 n 次操作注入失败 |
//!
//! # 边界（v0.1 不含，按需扩展、只增语义）
//!
//! - `O_DIRECT` / 对齐要求：存储层启用时再加；
//! - `remove` / `rename` / `list`：文件系统**命名空间**操作，不属于
//!   本接口的"字节设备"定位，由消费方经各自阶段引入；
//! - 直接暴露文件描述符：统一经句柄（后续如需零拷贝路径，作为**新增**
//!   操作加入，不改既有语义）。

mod fault;
mod mem;
mod os;

pub use fault::{FaultAction, FaultInjecting, FaultOp, FaultRule};
pub use mem::MemFileIo;
pub use os::OsFileIo;

use std::io;
use std::path::Path;

/// 不透明文件句柄：其数值含义由实现解释，调用方不得推导。
///
/// 提供 `from_raw` / `as_raw` 仅面向 [`FileIo`] 的**实现者**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileHandle(u64);

impl FileHandle {
    /// 由实现自有的数值构造（面向实现者）。
    #[must_use]
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// 取实现自有的数值（面向实现者）。
    #[must_use]
    pub fn as_raw(self) -> u64 {
        self.0
    }
}

/// 打开选项（引擎自有的一小组；与 [`std::fs::OpenOptions`] 语义一致）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    create: bool,
    create_new: bool,
    truncate: bool,
}

impl OpenOptions {
    /// 全部关闭的默认值（须再设置 `read` 或 `write`）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 以读方式打开。
    #[must_use]
    pub fn read(mut self, yes: bool) -> Self {
        self.read = yes;
        self
    }

    /// 以写方式打开。
    #[must_use]
    pub fn write(mut self, yes: bool) -> Self {
        self.write = yes;
        self
    }

    /// 文件不存在时创建。
    #[must_use]
    pub fn create(mut self, yes: bool) -> Self {
        self.create = yes;
        self
    }

    /// 创建新文件；**已存在则失败**（`AlreadyExists`）。
    #[must_use]
    pub fn create_new(mut self, yes: bool) -> Self {
        self.create_new = yes;
        self
    }

    /// 打开时截断为 0 长度（要求写方式）。
    #[must_use]
    pub fn truncate(mut self, yes: bool) -> Self {
        self.truncate = yes;
        self
    }

    /// 是否以读方式打开。
    #[must_use]
    pub fn is_read(&self) -> bool {
        self.read
    }

    /// 是否以写方式打开。
    #[must_use]
    pub fn is_write(&self) -> bool {
        self.write
    }

    /// 是否"不存在时创建"。
    #[must_use]
    pub fn is_create(&self) -> bool {
        self.create
    }

    /// 是否"必须新建"。
    #[must_use]
    pub fn is_create_new(&self) -> bool {
        self.create_new
    }

    /// 是否打开时截断。
    #[must_use]
    pub fn is_truncate(&self) -> bool {
        self.truncate
    }

    /// 校验组合合法性（实现者在打开前调用，保证各实现行为一致）。
    ///
    /// 规则：至少 `read` 或 `write`；`truncate` 要求 `write`；
    /// `create_new` 要求 `write`。
    pub fn validate(&self) -> io::Result<()> {
        if !self.read && !self.write {
            return Err(invalid_input("打开选项必须至少含 read 或 write"));
        }
        if self.truncate && !self.write {
            return Err(invalid_input("truncate 需要 write"));
        }
        if self.create_new && !self.write {
            return Err(invalid_input("create_new 需要 write"));
        }
        Ok(())
    }
}

/// 句柄所指对象的类型（实现内部使用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandleKind {
    /// 普通文件。
    File,
    /// 目录。
    Dir,
}

/// 文件操作接口（引擎唯一的文件操作入口）。
///
/// 所有方法都是**句柄基**的；路径只在 `open` / `open_dir` 出现。
pub trait FileIo: Send + Sync {
    /// Current number of live opaque handles when the implementation can
    /// report it. Used by service status and resource-leak regression tests.
    fn open_handle_count(&self) -> Option<usize> {
        None
    }

    /// 打开（或创建）普通文件。实现须：不跟随符号链接，并在打开后校验
    /// 对象为普通文件（非目录、非设备）。
    fn open(&self, path: &Path, opts: OpenOptions) -> io::Result<FileHandle>;

    /// 打开目录（只读）。目录句柄允许的操作：`sync_dir`、`close`。
    fn open_dir(&self, path: &Path) -> io::Result<FileHandle>;

    /// 从 `offset` 读入 `buf`，返回实际读取的字节数（`0` = 文件末尾）。
    fn read_at(&self, handle: FileHandle, buf: &mut [u8], offset: u64) -> io::Result<usize>;

    /// 从 `offset` 写入全部 `buf`；**全量或错误**（出错时可能已部分写入）。
    fn write_at(&self, handle: FileHandle, buf: &[u8], offset: u64) -> io::Result<()>;

    /// 文件长度（字节）。
    fn size(&self, handle: FileHandle) -> io::Result<u64>;

    /// 截断或扩展文件（扩展部分读为 0）。要求写方式。
    fn set_len(&self, handle: FileHandle, len: u64) -> io::Result<()>;

    /// 数据落盘（`fdatasync` 语义）。
    fn sync_data(&self, handle: FileHandle) -> io::Result<()>;

    /// 数据与元数据落盘（`fsync` 语义）。
    fn sync_all(&self, handle: FileHandle) -> io::Result<()>;

    /// 目录项变更落盘（对目录句柄 `fsync`）。要求在 `open_dir` 得到的句柄上调用。
    fn sync_dir(&self, handle: FileHandle) -> io::Result<()>;

    /// 关闭句柄。重复关闭得到 `NotFound`。
    fn close(&self, handle: FileHandle) -> io::Result<()>;

    /// 读满 `buf`；提前到达文件末尾时返回 `UnexpectedEof`。
    fn read_exact_at(&self, handle: FileHandle, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let n = self.read_at(handle, &mut buf[done..], offset + done as u64)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "read_exact_at 提前到达文件末尾",
                ));
            }
            done += n;
        }
        Ok(())
    }
}

/// 统一的 `InvalidInput` 错误。
pub(crate) fn invalid_input(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

/// 统一的"句柄无效"错误。
pub(crate) fn handle_not_found(handle: FileHandle) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("句柄 {handle:?} 无效或已关闭"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_options_validation() {
        assert!(OpenOptions::new().validate().is_err(), "无 read/write");
        assert!(OpenOptions::new().read(true).validate().is_ok());
        assert!(
            OpenOptions::new()
                .read(true)
                .truncate(true)
                .validate()
                .is_err(),
            "truncate 需要 write"
        );
        assert!(
            OpenOptions::new()
                .read(true)
                .create_new(true)
                .validate()
                .is_err(),
            "create_new 需要 write"
        );
        assert!(OpenOptions::new()
            .write(true)
            .create_new(true)
            .validate()
            .is_ok());
    }
}
