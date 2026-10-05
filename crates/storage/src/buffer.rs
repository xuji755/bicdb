//! 缓冲池（§5.10）：**数据库不直接读写文件**——所有页的读写都经这里。
//!
//! ```text
//! pin(key) ──▶ 命中：pin++、LRU 置前                    ──▶ PageGuard（Drop = unpin）
//!          └─ 未命中：取空闲槽 / 按 LRU 淘汰
//!                       └─ 受害者脏 ⇒ 写回：
//!                            **WAL 规则 2**（redo 先持久化到页的 page_lsn）
//!                            → pagefile 写（seal）
//!                       读入页（两层校验）→ 身份核对（rdba/工作区）
//! ```
//!
//! # 键必须带工作区（§5.10）
//!
//! 池是**实例级共享**的：不同工作区会出现相同 `RDBA`，只按 `RDBA` 索引会
//! **串页**。所以键 =（工作区标识, `RDBA`）；页头自述的 `workspace_ref` /
//! `file_id` / `block_id` 与键**逐项核对**（定位器给错 файл/块 = 响亮失败，
//! 不静默串页）。
//!
//! # 两条链（§5.10）
//!
//! - **LRU 链**：挑淘汰对象；**脏缓冲可以被淘汰**（steal）——写回后腾出；
//! - **脏链**：**每工作区一条**，按**首次变脏的 LSN** 升序（检查点队列的
//!   前身）；乱序写会让低水位推不动，所以本切片只提供**按序写回**
//!   （`flush_workspace` 从最老一头走）。
//!
//! # no-force 与 WAL 规则 2
//!
//! 提交**不要求**数据页落盘（no-force）——提交的关键路径上没有数据页 I/O；
//! 但脏页**被写回之前**，其 `page_lsn` 之后的 redo **必须已持久化**（规则 2），
//! 否则崩溃后无法重放这次修改。判据就是页的 `page_lsn` 与 WAL 水位比较，
//! 不足则先 `ensure_durable`（可能退化为一次同步 `fsync`）。
//!
//! # 本切片的分区取默认值：N = 1（§5.10"分区是可选能力"）
//!
//! 单闩锁起步（`Mutex<Inner>`）——**一次只能持有一个 `PageGuard`**：
//! 卫兵在生命周期内持有池闩锁，**同一线程再 `pin` 会自锁**（std 互斥不可重入）。
//! 需要跨页操作时先取副本、放开卫兵再取下一页；分区 / 工作集与每分区 latch
//! 在 P4 接入（默认 1 是文档允许的合法配置，届时"持两把"由分片自然支持）。
//! `PoolFull` 因此在本形态下**不可达**（钉住即持锁）——保留它是为分片形态的
//! "全钉住"情形（写线程独占某分区时）。

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

use bicdb_common::seq::Lsn;
use bicdb_workspace::io::{FileHandle, FileIo};

use crate::page::Page;
use crate::pagefile::{self, PageFileError};
use crate::rowid::Rdba;

/// 块定位器：`(工作区标识, rdba) →（页文件句柄、块号）`。
///
/// 带上工作区是因为**池是实例级共享**的：不同工作区各有自己的文件句柄。
pub type PoolResolver<'io> = dyn FnMut(&[u8; 8], Rdba) -> Option<(FileHandle, u32)> + 'io;

/// 缓冲池的键：**工作区标识 + RDBA**（§5.10——池实例级共享，键必须带工作区）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BufferKey {
    /// 工作区受校验标识（页头同一字段）。
    pub workspace: [u8; 8],
    /// 块地址。
    pub rdba: Rdba,
}

impl BufferKey {
    /// 由工作区标识与块地址构造。
    #[must_use]
    pub fn new(workspace: [u8; 8], rdba: Rdba) -> Self {
        Self { workspace, rdba }
    }
}

/// WAL 协调口（**WAL 规则 2** 的落点）。
///
/// 真实实现由 `bicdb-wal` 的日志缓冲/组写入器适配（`ensure_durable` =
/// 刷到目标位置）；测试用假实现替换。
pub trait WalGuard: Send {
    /// 当前**已持久化**的 LSN 水位。
    fn durable_lsn(&self) -> Lsn;
    /// 把 redo 持久化到 `target`（返回后 `durable_lsn() >= target`）；
    /// **失败即错误**——页不得写出。
    fn ensure_durable(&mut self, target: Lsn) -> std::io::Result<()>;
}

/// 缓冲池错误。
#[derive(Debug)]
pub enum BufferError {
    /// 底层 I/O。
    Io(std::io::Error),
    /// 目标页损坏（两层校验失败）——按损坏处理，不静默返回数据。
    Damaged {
        /// 块地址。
        rdba: Rdba,
    },
    /// 块无法定位（定位器给不出句柄/块号）。
    Unresolved {
        /// 块地址。
        rdba: Rdba,
    },
    /// 页身份与键不符（串页防线：页头自述的 `rdba`/工作区与请求不一致）。
    IdentityMismatch {
        /// 请求的键。
        expected: BufferKey,
        /// 页头自述的工作区标识。
        found_workspace: [u8; 8],
        /// 页头自述的文件号。
        found_file: u16,
        /// 页头自述的块号。
        found_block: u32,
    },
    /// 所有帧都被钉住，淘汰不出受害者。
    PoolFull,
    /// WAL 规则 2 的刷盘失败——页**没有**写出。
    WalFlush(std::io::Error),
    /// 容量为零（非法配置）。
    ZeroCapacity,
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BufferError::Io(e) => write!(f, "缓冲池 I/O：{e}"),
            BufferError::Damaged { rdba } => write!(
                f,
                "缓冲池目标页损坏（文件 {} 块 {}）——按损坏处理",
                rdba.file_id(),
                rdba.block_id()
            ),
            BufferError::Unresolved { rdba } => write!(
                f,
                "缓冲池块无法定位（文件 {} 块 {}）",
                rdba.file_id(),
                rdba.block_id()
            ),
            BufferError::IdentityMismatch {
                expected,
                found_workspace,
                found_file,
                found_block,
            } => write!(
                f,
                "页身份与键不符：期望文件 {} 块 {}（工作区 {expected:?}），页头自述文件 {found_file} 块 {found_block}（工作区 {found_workspace:?}）",
                expected.rdba.file_id(),
                expected.rdba.block_id()
            ),
            BufferError::PoolFull => f.write_str("缓冲池所有帧都被钉住，无受害者可淘汰"),
            BufferError::WalFlush(e) => write!(f, "WAL 规则 2 前置刷盘失败（页未写出）：{e}"),
            BufferError::ZeroCapacity => f.write_str("缓冲池容量为零"),
        }
    }
}

impl std::error::Error for BufferError {}

impl From<std::io::Error> for BufferError {
    fn from(e: std::io::Error) -> Self {
        BufferError::Io(e)
    }
}

/// 缓冲池统计（测试与诊断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BufferStats {
    /// 命中次数。
    pub hits: u64,
    /// 未命中（读入）次数。
    pub misses: u64,
    /// 淘汰次数。
    pub evictions: u64,
    /// 写回次数（脏页落盘）。
    pub writes: u64,
    /// 因 WAL 规则 2 而触发的日志刷盘次数。
    pub wal_syncs: u64,
}

/// 一次 `flush_workspace` 的报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlushReport {
    /// 写回的页数。
    pub pages_written: u64,
    /// 写回后该工作区的**新低水位**（脏链已空 ⇒ `None` = 无脏页，
    /// 调用方用当前日志位置当低水位）。
    pub low_water: Option<Lsn>,
}

/// 一个缓冲帧（内存专有结构——**不落盘**，§5.10 的"两种页头必须分清"）。
struct Frame {
    key: Option<BufferKey>,
    page: Page,
    /// 脏标志。
    dirty: bool,
    /// **首次变脏的 LSN**（脏链排序键）。
    first_dirty: Option<Lsn>,
    /// 引用计数（钉住）。
    pins: u32,
}

impl Frame {
    fn empty() -> Self {
        Self {
            key: None,
            page: Page::from_bytes(Box::new([0u8; crate::page::PAGE_SIZE])),
            dirty: false,
            first_dirty: None,
            pins: 0,
        }
    }
}

/// 池内状态（单闩锁）。
struct Inner<'io> {
    frames: Vec<Frame>,
    map: HashMap<BufferKey, usize>,
    /// 空闲帧号。
    free: Vec<usize>,
    /// LRU：**最近使用在前**，淘汰取后。
    lru: VecDeque<usize>,
    /// 脏链：**每工作区一条**，按（首次变脏的 LSN, rdba）升序。
    dirty: BTreeMap<[u8; 8], BTreeSet<(Lsn, Rdba)>>,
    resolve: Box<PoolResolver<'io>>,
    wal: Box<dyn WalGuard + 'io>,
    stats: BufferStats,
}

/// **缓冲池**（§5.10；本切片 N = 1 分区）。
pub struct BufferPool<'io> {
    io: &'io dyn FileIo,
    capacity: usize,
    inner: Mutex<Inner<'io>>,
}

impl std::fmt::Debug for BufferPool<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferPool")
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl<'io> BufferPool<'io> {
    /// 建池。`resolve` 给出 `rdba →（页文件句柄、块号）`；`wal` 提供
    /// 规则 2 的持久化水位与刷盘口。
    pub fn new(
        io: &'io dyn FileIo,
        capacity: usize,
        resolve: impl FnMut(&[u8; 8], Rdba) -> Option<(FileHandle, u32)> + 'io,
        wal: impl WalGuard + 'io,
    ) -> Result<Self, BufferError> {
        if capacity == 0 {
            return Err(BufferError::ZeroCapacity);
        }
        Ok(Self {
            io,
            capacity,
            inner: Mutex::new(Inner {
                frames: (0..capacity).map(|_| Frame::empty()).collect(),
                map: HashMap::new(),
                free: (0..capacity).rev().collect(),
                lru: VecDeque::new(),
                dirty: BTreeMap::new(),
                resolve: Box::new(resolve),
                wal: Box::new(wal),
                stats: BufferStats::default(),
            }),
        })
    }

    /// 容量（帧数）。
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 当前驻留帧数。
    #[must_use]
    pub fn resident(&self) -> usize {
        self.lock().map.len()
    }

    /// 统计快照。
    #[must_use]
    pub fn stats(&self) -> BufferStats {
        self.lock().stats
    }

    /// 某工作区脏链长度。
    #[must_use]
    pub fn dirty_len(&self, workspace: [u8; 8]) -> usize {
        self.lock().dirty.get(&workspace).map_or(0, BTreeSet::len)
    }

    /// **低水位**：该工作区最老的脏页的（首次变脏 LSN）——它之前的修改
    /// 已全部落盘；`None` = 无脏页（检查点可用当前日志位置）。
    #[must_use]
    pub fn low_water(&self, workspace: [u8; 8]) -> Option<Lsn> {
        self.lock()
            .dirty
            .get(&workspace)
            .and_then(|s| s.iter().next().map(|(lsn, _)| *lsn))
    }

    /// **钉住一页**（命中/读入/淘汰都由这里统一处理）。
    pub fn pin(&self, key: BufferKey) -> Result<PageGuard<'_, 'io>, BufferError> {
        let mut inner = self.lock();
        if let Some(&idx) = inner.map.get(&key) {
            inner.stats.hits += 1;
            inner.frames[idx].pins += 1;
            touch(&mut inner.lru, idx);
            return Ok(PageGuard { inner, idx });
        }
        inner.stats.misses += 1;

        // 取槽：空闲优先，否则按 LRU 淘汰（脏受害者先写回）。
        let idx = match inner.free.pop() {
            Some(i) => i,
            None => {
                let victim = inner
                    .lru
                    .iter()
                    .rev()
                    .copied()
                    .find(|&i| inner.frames[i].pins == 0)
                    .ok_or(BufferError::PoolFull)?;
                inner.stats.evictions += 1;
                write_back(self.io, &mut inner, victim)?;
                if let Some(old) = inner.frames[victim].key.take() {
                    inner.map.remove(&old);
                }
                if let Some(p) = inner.lru.iter().position(|&i| i == victim) {
                    inner.lru.remove(p);
                }
                victim
            }
        };

        // 读入 + 身份核对（串页防线）。
        let (handle, block) = (inner.resolve)(&key.workspace, key.rdba)
            .ok_or(BufferError::Unresolved { rdba: key.rdba })?;
        let page = pagefile::read_page_verified(self.io, handle, block).map_err(|e| match e {
            PageFileError::Damaged { .. } => BufferError::Damaged { rdba: key.rdba },
            PageFileError::Io(e) => BufferError::Io(e),
        })?;
        let header = page
            .header()
            .ok_or(BufferError::Damaged { rdba: key.rdba })?;
        if header.file_id != key.rdba.file_id()
            || header.block_id != key.rdba.block_id()
            || header.workspace_ref != key.workspace
        {
            return Err(BufferError::IdentityMismatch {
                expected: key,
                found_workspace: header.workspace_ref,
                found_file: header.file_id,
                found_block: header.block_id,
            });
        }
        inner.frames[idx] = Frame {
            key: Some(key),
            page,
            dirty: false,
            first_dirty: None,
            pins: 1,
        };
        inner.map.insert(key, idx);
        inner.lru.push_front(idx);
        Ok(PageGuard { inner, idx })
    }

    /// 写回某一页（若脏）。返回是否真的写了。
    pub fn flush(&self, key: BufferKey) -> Result<bool, BufferError> {
        let mut inner = self.lock();
        let Some(&idx) = inner.map.get(&key) else {
            return Ok(false);
        };
        if !inner.frames[idx].dirty {
            return Ok(false);
        }
        write_back(self.io, &mut inner, idx)?;
        Ok(true)
    }

    /// **按序写回一个工作区的全部脏页**（从最老一头；§11.7 的检查点队列）。
    pub fn flush_workspace(&self, workspace: [u8; 8]) -> Result<FlushReport, BufferError> {
        let mut inner = self.lock();
        let mut report = FlushReport::default();
        loop {
            let next = inner
                .dirty
                .get(&workspace)
                .and_then(|s| s.iter().next().copied());
            let Some((first_dirty, rdba)) = next else {
                break;
            };
            let key = BufferKey::new(workspace, rdba);
            let Some(&idx) = inner.map.get(&key) else {
                // 脏链与驻留表失步（不应发生）——防呆清理后继续。
                if let Some(chain) = inner.dirty.get_mut(&workspace) {
                    chain.remove(&(first_dirty, rdba));
                    if chain.is_empty() {
                        inner.dirty.remove(&workspace);
                    }
                }
                continue;
            };
            write_back(self.io, &mut inner, idx)?;
            report.pages_written += 1;
        }
        Ok(report)
    }

    fn lock(&self) -> MutexGuard<'_, Inner<'io>> {
        self.inner.lock().expect("缓冲池闩锁中毒")
    }
}

/// LRU 置前（去重后插入队首）。
fn touch(lru: &mut VecDeque<usize>, idx: usize) {
    if let Some(p) = lru.iter().position(|&i| i == idx) {
        lru.remove(p);
    }
    lru.push_front(idx);
}

/// **写回一帧**：WAL 规则 2 → 页文件写 → 清脏、出脏链。
fn write_back(io: &dyn FileIo, inner: &mut Inner<'_>, idx: usize) -> Result<(), BufferError> {
    if !inner.frames[idx].dirty {
        return Ok(());
    }
    let Some(key) = inner.frames[idx].key else {
        // 无主脏帧（不应发生）——清脏防呆。
        inner.frames[idx].dirty = false;
        inner.frames[idx].first_dirty = None;
        return Ok(());
    };
    let page_lsn = inner.frames[idx]
        .page
        .header()
        .map_or(Lsn::from_raw(0).expect("0 合法"), |h| h.page_lsn);

    // **WAL 规则 2**：redo 必须先持久化到该页的 `page_lsn`。
    if page_lsn > inner.wal.durable_lsn() {
        inner
            .wal
            .ensure_durable(page_lsn)
            .map_err(BufferError::WalFlush)?;
        inner.stats.wal_syncs += 1;
    }

    let (handle, block) = (inner.resolve)(&key.workspace, key.rdba)
        .ok_or(BufferError::Unresolved { rdba: key.rdba })?;
    pagefile::write_page(io, handle, block, &mut inner.frames[idx].page)
        .map_err(BufferError::Io)?;

    inner.stats.writes += 1;
    if let Some(lsn) = inner.frames[idx].first_dirty.take() {
        if let Some(chain) = inner.dirty.get_mut(&key.workspace) {
            chain.remove(&(lsn, key.rdba));
            if chain.is_empty() {
                inner.dirty.remove(&key.workspace);
            }
        }
    }
    inner.frames[idx].dirty = false;
    Ok(())
}

/// **页卫兵**（`PageGuard`）：钉住一帧，`Drop` = unpin + LRU 置前。
///
/// `Deref`/`DerefMut` 直达页字节；`mark_dirty` 记"首次变脏的 LSN"入脏链。
pub struct PageGuard<'a, 'io> {
    inner: MutexGuard<'a, Inner<'io>>,
    idx: usize,
}

impl std::fmt::Debug for PageGuard<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageGuard").field("idx", &self.idx).finish()
    }
}

impl PageGuard<'_, '_> {
    /// 本帧的键。
    #[must_use]
    pub fn key(&self) -> BufferKey {
        self.inner.frames[self.idx].key.expect("钉住的帧必有主")
    }

    /// 页是否脏。
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.inner.frames[self.idx].dirty
    }

    /// **标脏**（写路径在追加完 redo 后调用）：`first_dirty_lsn` 只在
    /// **首次**变脏时记入——脏链按它排序，重复标脏不改变排序键。
    pub fn mark_dirty(&mut self, first_dirty_lsn: Lsn) {
        let frame = &mut self.inner.frames[self.idx];
        if frame.dirty {
            return;
        }
        frame.dirty = true;
        frame.first_dirty = Some(first_dirty_lsn);
        let key = frame.key.expect("钉住的帧必有主");
        self.inner
            .dirty
            .entry(key.workspace)
            .or_default()
            .insert((first_dirty_lsn, key.rdba));
    }
}

impl std::ops::Deref for PageGuard<'_, '_> {
    type Target = Page;
    fn deref(&self) -> &Page {
        &self.inner.frames[self.idx].page
    }
}

impl std::ops::DerefMut for PageGuard<'_, '_> {
    fn deref_mut(&mut self) -> &mut Page {
        &mut self.inner.frames[self.idx].page
    }
}

impl Drop for PageGuard<'_, '_> {
    fn drop(&mut self) {
        let idx = self.idx;
        self.inner.frames[idx].pins = self.inner.frames[idx].pins.saturating_sub(1);
        touch(&mut self.inner.lru, idx);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use bicdb_workspace::io::{MemFileIo, OpenOptions};

    use super::*;
    use crate::page::PageType;

    fn rdba(file_id: u16, block: u32) -> Rdba {
        Rdba::from_parts(file_id, block).unwrap()
    }

    const WS_A: [u8; 8] = [1u8; 8];
    const WS_B: [u8; 8] = [2u8; 8];
    const F_A: &str = "/mem/a.dat";
    const F_B: &str = "/mem/b.dat";

    fn lsn(v: u64) -> Lsn {
        Lsn::from_raw(v).unwrap()
    }

    /// 记录 I/O 事件到共享日志（与假 WAL 共用一份，断言**次序**）。
    struct RecordingIo {
        inner: MemFileIo,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl FileIo for RecordingIo {
        fn open(&self, path: &Path, opts: OpenOptions) -> std::io::Result<FileHandle> {
            self.inner.open(path, opts)
        }
        fn open_dir(&self, path: &Path) -> std::io::Result<FileHandle> {
            self.inner.open_dir(path)
        }
        fn read_at(&self, h: FileHandle, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
            self.inner.read_at(h, buf, off)
        }
        fn write_at(&self, h: FileHandle, buf: &[u8], off: u64) -> std::io::Result<()> {
            self.log.lock().unwrap().push(format!("io:write:{off}"));
            self.inner.write_at(h, buf, off)
        }
        fn size(&self, h: FileHandle) -> std::io::Result<u64> {
            self.inner.size(h)
        }
        fn set_len(&self, h: FileHandle, len: u64) -> std::io::Result<()> {
            self.inner.set_len(h, len)
        }
        fn sync_data(&self, h: FileHandle) -> std::io::Result<()> {
            self.log.lock().unwrap().push("io:sync".into());
            self.inner.sync_data(h)
        }
        fn sync_all(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.sync_all(h)
        }
        fn sync_dir(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.sync_dir(h)
        }
        fn close(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.close(h)
        }
    }

    /// 假 WAL 协调口。
    struct FakeWal {
        durable: Lsn,
        fail: bool,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl WalGuard for FakeWal {
        fn durable_lsn(&self) -> Lsn {
            self.durable
        }
        fn ensure_durable(&mut self, target: Lsn) -> std::io::Result<()> {
            if self.fail {
                return Err(std::io::Error::other("假刷盘失败"));
            }
            self.log
                .lock()
                .unwrap()
                .push(format!("wal:ensure:{}", target.as_raw()));
            if target > self.durable {
                self.durable = target;
            }
            Ok(())
        }
    }

    struct Harness {
        io: RecordingIo,
        log: Arc<Mutex<Vec<String>>>,
        a: FileHandle,
        b: FileHandle,
    }

    /// 双向夹具：文件 A（file_id 7，工作区 A）与文件 B（file_id 8，工作区 B），
    /// 各两页。
    fn harness() -> Harness {
        let mem = MemFileIo::new();
        mem.add_dir("/mem");
        let log = Arc::new(Mutex::new(Vec::new()));
        let io = RecordingIo {
            inner: mem,
            log: Arc::clone(&log),
        };
        let a = io
            .open(
                Path::new(F_A),
                OpenOptions::new().read(true).write(true).create_new(true),
            )
            .unwrap();
        io.set_len(a, 2 * crate::page::PAGE_SIZE as u64).unwrap();
        let b = io
            .open(
                Path::new(F_B),
                OpenOptions::new().read(true).write(true).create_new(true),
            )
            .unwrap();
        io.set_len(b, 2 * crate::page::PAGE_SIZE as u64).unwrap();
        let mut h = Harness { io, log, a, b };
        for block in 0..2u32 {
            h.put_page(WS_A, 7, block, 0xA0 + block as u8);
            h.put_page(WS_B, 8, block, 0xB0 + block as u8);
        }
        h
    }

    impl Harness {
        fn put_page(&mut self, ws: [u8; 8], file_id: u16, block: u32, byte: u8) {
            let handle = if file_id == 7 { self.a } else { self.b };
            let mut page = Page::new(PageType::HeapTable, ws, file_id, block);
            page.as_bytes_mut()[4096] = byte;
            pagefile::write_page(&self.io, handle, block, &mut page).unwrap();
        }
        fn pool<'io>(&'io self, capacity: usize, wal: impl WalGuard + 'io) -> BufferPool<'io> {
            let (a, b) = (self.a, self.b);
            BufferPool::new(
                &self.io,
                capacity,
                move |ws, r| {
                    if *ws == WS_A && r.file_id() == 7 {
                        Some((a, r.block_id()))
                    } else if *ws == WS_B && r.file_id() == 8 {
                        Some((b, r.block_id()))
                    } else {
                        None
                    }
                },
                wal,
            )
            .unwrap()
        }
        fn fake_wal(&self) -> FakeWal {
            FakeWal {
                durable: lsn(0),
                fail: false,
                log: Arc::clone(&self.log),
            }
        }
        fn events(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
        fn clear_events(&self) {
            self.log.lock().unwrap().clear();
        }
        fn read_byte(&self, file_id: u16, block: u32) -> u8 {
            let handle = if file_id == 7 { self.a } else { self.b };
            let page = pagefile::read_page_verified(&self.io, handle, block).unwrap();
            page.as_bytes()[4096]
        }
        fn set_page_lsn(&self, page: &mut Page, v: u64) {
            let mut h = page.header().unwrap();
            h.page_lsn = lsn(v);
            page.write_header(&h);
        }
    }

    #[test]
    fn pin_reads_on_miss_then_hits() {
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        let k = BufferKey::new(WS_A, rdba(7, 0));
        {
            let g = pool.pin(k).unwrap();
            assert_eq!(g.as_bytes()[4096], 0xA0, "内容来自文件");
        }
        {
            let _g = pool.pin(k).unwrap();
        }
        let s = pool.stats();
        assert_eq!((s.misses, s.hits), (1, 1));
        assert_eq!(pool.resident(), 1);
    }

    #[test]
    fn identity_mismatch_is_rejected() {
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        // 请求 (7,0) 但把页头的工作区改成 B——串页防线必须响亮失败。
        let mut page = {
            let g = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
            Page::from_bytes(Box::new(*g.as_bytes()))
        };
        drop(pool);
        let mut header = page.header().unwrap();
        header.workspace_ref = WS_B;
        page.write_header(&header);
        pagefile::write_page(&h.io, h.a, 0, &mut page).unwrap();

        let pool = h.pool(4, h.fake_wal());
        assert!(matches!(
            pool.pin(BufferKey::new(WS_A, rdba(7, 0))),
            Err(BufferError::IdentityMismatch { .. })
        ));
    }

    #[test]
    fn dirty_eviction_writes_back_after_wal_flush() {
        let h = harness();
        let pool = h.pool(1, h.fake_wal());
        {
            let mut g = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
            g.as_bytes_mut()[4096] = 0xEE;
            h.set_page_lsn(&mut g, 7);
            g.mark_dirty(lsn(7));
        }
        h.clear_events();
        // 容量 1：读第二页必须淘汰第一页（脏 → 先 WAL 规则 2 再写）。
        {
            let _g = pool.pin(BufferKey::new(WS_A, rdba(7, 1))).unwrap();
        }
        assert_eq!(h.events(), vec!["wal:ensure:7", "io:write:0"]);
        assert_eq!(h.read_byte(7, 0), 0xEE, "脏页已写回");
        let s = pool.stats();
        assert_eq!((s.evictions, s.writes, s.wal_syncs), (1, 1, 1));
    }

    #[test]
    fn wal_failure_blocks_write_back() {
        let h = harness();
        let mut wal = h.fake_wal();
        wal.fail = true; // 刷盘必失败——页就不该写出
        let pool = h.pool(1, wal);
        {
            let mut g = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
            g.as_bytes_mut()[4096] = 0xEE;
            h.set_page_lsn(&mut g, 9);
            g.mark_dirty(lsn(9));
        }
        // 容量 1：读第二页要淘汰脏页 → WAL 规则 2 刷盘失败 ⇒ 整体失败。
        let err = pool.pin(BufferKey::new(WS_A, rdba(7, 1))).unwrap_err();
        assert!(matches!(err, BufferError::WalFlush(_)), "{err}");
        assert_eq!(h.read_byte(7, 0), 0xA0, "页**没有**写出");
        assert_eq!(pool.dirty_len(WS_A), 1, "脏状态保留");
        assert_eq!(pool.low_water(WS_A), Some(lsn(9)));
    }

    #[test]
    fn clean_eviction_writes_nothing() {
        let h = harness();
        let pool = h.pool(1, h.fake_wal());
        {
            let _g = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
        }
        h.clear_events();
        {
            let _g = pool.pin(BufferKey::new(WS_A, rdba(7, 1))).unwrap();
        }
        assert!(h.events().is_empty(), "干净页淘汰不产生任何 I/O");
        assert_eq!(pool.stats().writes, 0);
    }

    #[test]
    fn dirty_chain_is_ordered_and_low_water_tracks_oldest() {
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        {
            let mut g0 = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
            h.set_page_lsn(&mut g0, 9);
            g0.mark_dirty(lsn(9));
        }
        {
            let mut g1 = pool.pin(BufferKey::new(WS_A, rdba(7, 1))).unwrap();
            h.set_page_lsn(&mut g1, 4);
            g1.mark_dirty(lsn(4));
        }
        assert_eq!(pool.dirty_len(WS_A), 2);
        assert_eq!(
            pool.low_water(WS_A),
            Some(lsn(4)),
            "低水位 = 最老的首次变脏 LSN"
        );

        // 写掉较新的一页，低水位不动。
        assert!(pool.flush(BufferKey::new(WS_A, rdba(7, 0))).unwrap());
        assert_eq!(pool.low_water(WS_A), Some(lsn(4)));
        assert_eq!(pool.dirty_len(WS_A), 1);
    }

    #[test]
    fn flush_workspace_writes_in_first_dirty_order() {
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        {
            let mut g0 = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
            h.set_page_lsn(&mut g0, 9);
            g0.mark_dirty(lsn(9));
        }
        {
            let mut g1 = pool.pin(BufferKey::new(WS_A, rdba(7, 1))).unwrap();
            h.set_page_lsn(&mut g1, 4);
            g1.mark_dirty(lsn(4));
        }
        h.clear_events();
        let report = pool.flush_workspace(WS_A).unwrap();
        assert_eq!(report.pages_written, 2);
        assert_eq!(
            h.events(),
            vec![
                "wal:ensure:4",
                "io:write:16384",
                "wal:ensure:9",
                "io:write:0"
            ],
            "按首次变脏 LSN 升序写；每页写前先满足 WAL 规则 2"
        );
        assert_eq!(pool.dirty_len(WS_A), 0);
        assert_eq!(pool.low_water(WS_A), None);
    }

    #[test]
    fn workspaces_have_separate_frames_and_chains() {
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        // 单闩锁形态：卫兵要逐个放开（§5.10 的 N = 1 默认）。
        {
            let _a = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
        }
        {
            let mut b = pool.pin(BufferKey::new(WS_B, rdba(8, 0))).unwrap();
            h.set_page_lsn(&mut b, 2);
            b.mark_dirty(lsn(2));
        }
        assert_eq!(pool.resident(), 2, "同块号不同工作区 = 两帧");
        assert_eq!(pool.dirty_len(WS_A), 0);
        assert_eq!(pool.dirty_len(WS_B), 1, "脏链按工作区分");
    }
}
