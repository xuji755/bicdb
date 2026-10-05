//! 页文件的读写（**经 [`FileIo`] 替换接口**；REQ-STO-012："关闭重开后
//! 结果一致"）。
//!
//! # 形态
//!
//! 页文件 = **块 = 页**（16 KiB）的定长文件；块 `b` 占字节区间
//! `[b × 16384, (b+1) × 16384)`。块 0 是文件头页（§5.6 页类型 11），
//! 数据页从块 1 起——本模块只负责**定址与读写**，不解释块的角色。
//!
//! # 纪律
//!
//! - **写 = `seal` → 定址写 → （调用方决定何时）`sync_data`**：
//!   校验和与页尾副本在写盘前落定（`seal`），调用方在需要持久性点调用
//!   [`sync`]（提交路径的次序由事务域接管；这里不做隐式 fsync）；
//! - **读 = 定址读 → 两层完整性校验**（[`read_page_verified`]）：
//!   检出损坏即返回 [`PageFileError::Damaged`]——**不静默返回数据**
//!   （REQ-STO-012 的"明确判定"）。
//!
//! 故障注入（P1 的 `FaultInjecting`）可直接套在外层：撕裂写会被
//! 读路径的页尾副本/校验和检出——两者在本模块的测试里闭环。

use std::io;
use std::path::Path;

use bicdb_workspace::io::{FileHandle, FileIo, OpenOptions};

use crate::page::{Page, PageCheck, PAGE_SIZE};

/// 页文件的定址单位（= 页大小）。
pub const BLOCK_SIZE: usize = PAGE_SIZE;

/// 页文件操作错误（**明确判定**，不静默）。
#[derive(Debug)]
pub enum PageFileError {
    /// 底层 I/O 错误。
    Io(io::Error),
    /// 页损坏（两层完整性检出）——按损坏处理：报错、告警、从备份恢复。
    Damaged {
        /// 块号。
        block: u32,
        /// 检出结果（`FracturedBlock` / `ChecksumMismatch` / `UnknownPageType`）。
        check: PageCheck,
    },
}

impl std::fmt::Display for PageFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PageFileError::Io(e) => write!(f, "页文件 I/O 错误：{e}"),
            PageFileError::Damaged { block, check } => {
                write!(f, "块 {block} 损坏（{check:?}）——按损坏处理")
            }
        }
    }
}

impl std::error::Error for PageFileError {}

impl From<io::Error> for PageFileError {
    fn from(e: io::Error) -> Self {
        PageFileError::Io(e)
    }
}

/// 新建页文件（`blocks` 块；`set_len` 预置长度，内容为零）。
pub fn create(io: &dyn FileIo, path: &Path, blocks: u64) -> io::Result<FileHandle> {
    let handle = io.open(
        path,
        OpenOptions::new().read(true).write(true).create_new(true),
    )?;
    io.set_len(handle, blocks * BLOCK_SIZE as u64)?;
    Ok(handle)
}

/// 打开既有页文件（读写）。
pub fn open(io: &dyn FileIo, path: &Path) -> io::Result<FileHandle> {
    io.open(path, OpenOptions::new().read(true).write(true))
}

/// 页文件块数（`size / 16384`；非整除视为文件损坏，向上取整给调用方判定）。
pub fn blocks(io: &dyn FileIo, handle: FileHandle) -> io::Result<u64> {
    Ok(io.size(handle)?.div_ceil(BLOCK_SIZE as u64))
}

/// 写入一块：**先 `seal`**（页尾副本 + 校验和），再定址写。
///
/// 返回写入的校验和（供日志/诊断记录）。**不做 fsync**——持久性点由
/// 调用方用 [`sync`] 表达。
pub fn write_page(
    io: &dyn FileIo,
    handle: FileHandle,
    block: u32,
    page: &mut Page,
) -> io::Result<u32> {
    page.seal();
    let offset = u64::from(block) * BLOCK_SIZE as u64;
    io.write_at(handle, page.as_bytes().as_slice(), offset)?;
    Ok(page.stored_checksum())
}

/// 读入一块（**不校验**；完整性用 [`read_page_verified`] 或调用方自检）。
pub fn read_page(io: &dyn FileIo, handle: FileHandle, block: u32) -> io::Result<Page> {
    let mut buf = Box::new([0u8; PAGE_SIZE]);
    let offset = u64::from(block) * BLOCK_SIZE as u64;
    io.read_exact_at(handle, buf.as_mut_slice(), offset)?;
    Ok(Page::from_bytes(buf))
}

/// 读入一块并做两层完整性校验（页尾副本 → 校验和）。
///
/// 检出损坏 → [`PageFileError::Damaged`]（**明确判定**）。
pub fn read_page_verified(
    io: &dyn FileIo,
    handle: FileHandle,
    block: u32,
) -> Result<Page, PageFileError> {
    let page = read_page(io, handle, block)?;
    match page.verify() {
        PageCheck::Ok => Ok(page),
        check => Err(PageFileError::Damaged { block, check }),
    }
}

/// **区读（多块读）**：一次 `pread` 读入 `count` 个**连续**块，逐块做两层
/// 完整性校验（§5.12：全表扫描 / 索引快速全扫描 / 批量回表的读形态）。
///
/// - `count = 0` ⇒ 空返回；越出文件尾 ⇒ `Io`（`read_exact_at` 的
///   `UnexpectedEof`）——调用方用扫描边界（§4.3.1）保证不越界读；
/// - 单块损坏按 [`PageFileError::Damaged`] 报**具体块号**（不是整段笼统失败）。
pub fn read_run(
    io: &dyn FileIo,
    handle: FileHandle,
    first_block: u32,
    count: u32,
) -> Result<Vec<Page>, PageFileError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let total = PAGE_SIZE
        .checked_mul(count as usize)
        .ok_or_else(|| io::Error::other("区读长度溢出"))?;
    let mut buf = vec![0u8; total];
    let offset = u64::from(first_block) * BLOCK_SIZE as u64;
    io.read_exact_at(handle, &mut buf, offset)?;
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let mut page_buf = Box::new([0u8; PAGE_SIZE]);
        page_buf.copy_from_slice(&buf[i * PAGE_SIZE..(i + 1) * PAGE_SIZE]);
        let page = Page::from_bytes(page_buf);
        match page.verify() {
            PageCheck::Ok => out.push(page),
            check => {
                return Err(PageFileError::Damaged {
                    block: first_block + i as u32,
                    check,
                })
            }
        }
    }
    Ok(out)
}

/// 持久性点：数据落盘（`fdatasync`）。
pub fn sync(io: &dyn FileIo, handle: FileHandle) -> io::Result<()> {
    io.sync_data(handle)
}

/// 关闭（句柄归还）。
pub fn close(io: &dyn FileIo, handle: FileHandle) -> io::Result<()> {
    io.close(handle)
}
