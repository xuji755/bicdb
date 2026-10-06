//! **溢出底座**（切片 6b）：行落 temp 段、按 run 读回（设计 §4.2）。
//!
//! ```text
//! SpillSpace（一个临时段）：write_run(rows) → run 号；open_run(run) → 流式读回
//! 页布局：68B 页头 + [u32 本页块长度][块]——**run 的字节流跨页连续**
//! 行序列化：行 = [u32 值数] + 各值（tag + 载荷）
//! ```
//!
//! **口径**：temp 页 **no-redo**（§4.8：临时空间不进日志、崩溃即丢）；
//! 走**段直写/直读**（不经缓冲池——顺序溢出流不需要缓存，池键/淘汰面随之
//! 不引入；池化留待实测有需求时）。**SORT 与 HASH 共用本底座**（用户口径）。

use bicdb_storage::datafile::DataFile;
use bicdb_storage::page::{Page, PageType};
use bicdb_storage::segment::{Segment, SegmentSpaceError};
use bicdb_types::Number;

use crate::error::ExecError;
use crate::value::{Row, Value};

/// 页头长度（68B——§5.1）。
const PAGE_HEADER: usize = 68;
/// 每页可放**块**的上限 = 页大小 − 页头 − 4B 块长度前缀 − 4B 页尾副本。
/// （页尾 4B 是校验和副本——正文绝不能压到它上面：实测踩过，每页末 4B 被覆盖。）
const PAGE_CHUNK_MAX: usize = 16384 - PAGE_HEADER - 4 - 4;

/// 一个 run 的页区间（逻辑页起 + 页数）。
#[derive(Debug, Clone, Copy)]
struct RunRange {
    first_logical: u32,
    pages: u32,
}

/// **溢出空间**（一个临时段的顺序读写口）。
///
/// **可共享**（内部 `RefCell`）：一次执行的全部溢出算子（Sort/HashJoin/…）
/// 共用同一空间、各写各的 run——单线程拉取执行下 `write_run` 是原子的
/// （中间不让出），因此各 run 的页区间天然连续。
pub struct SpillSpace<'io> {
    inner: std::cell::RefCell<SpillInner<'io>>,
    /// **extra bytes**（`V$PGASTAT` 口径：溢出写/读的载荷字节——设计 §4.2 ④）。
    bytes_written: std::cell::Cell<u64>,
    bytes_read: std::cell::Cell<u64>,
}

struct SpillInner<'io> {
    segment: Segment<'io, 'io>,
    ws: [u8; 8],
    runs: Vec<RunRange>,
}

impl<'io> SpillSpace<'io> {
    /// 新建（`file` 应是**临时数据文件**——`DataFile::open_temp_reset` 打开的）。
    pub fn create(
        file: &'io mut DataFile<'io>,
        kind: bicdb_storage::temp::TempKind,
        ws: [u8; 8],
    ) -> Result<Self, ExecError> {
        let ws8 = ws;
        let (segment, _state) = bicdb_storage::temp::create_temp_segment(file, kind, 0, 0, 8)
            .map_err(|e| ExecError::Spill(e.to_string()))?;
        Ok(Self {
            inner: std::cell::RefCell::new(SpillInner {
                segment,
                ws: ws8,
                runs: Vec::new(),
            }),
            bytes_written: std::cell::Cell::new(0),
            bytes_read: std::cell::Cell::new(0),
        })
    }

    /// 累计**写出的额外字节**（载荷口径；诊断/`note_extra_bytes` 用）。
    #[must_use]
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written.get()
    }

    /// 累计**读回的额外字节**。
    #[must_use]
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read.get()
    }

    /// 把一批行写成一个 **run**（返回 run 号）。
    pub fn write_run(&self, rows: &[Row]) -> Result<usize, ExecError> {
        let blob = serialize_rows(rows)?;
        self.bytes_written
            .set(self.bytes_written.get() + blob.len() as u64);
        let mut inner = self.inner.borrow_mut();
        let mut first = 0u32;
        let mut pages = 0u32;
        for chunk in blob.chunks(PAGE_CHUNK_MAX) {
            let logical = allocate_page(&mut inner)?;
            if pages == 0 {
                first = logical;
            }
            let block = inner
                .segment
                .logical_block(logical)
                .ok_or_else(|| ExecError::Spill("临时段的逻辑页无物理块".to_owned()))?;
            let mut page = Page::new(
                PageType::Temporary,
                inner.ws,
                inner.segment.file_id(),
                block,
            );
            {
                let bytes = page.as_bytes_mut();
                bytes[PAGE_HEADER..PAGE_HEADER + 4]
                    .copy_from_slice(&(chunk.len() as u32).to_le_bytes());
                bytes[PAGE_HEADER + 4..PAGE_HEADER + 4 + chunk.len()].copy_from_slice(chunk);
            }
            page.seal();
            inner
                .segment
                .write_page(logical, &mut page)
                .map_err(space_err)?;
            pages += 1;
        }
        inner.runs.push(RunRange {
            first_logical: first,
            pages,
        });
        Ok(inner.runs.len() - 1)
    }

    /// 读回一个 run 的全部行（顺序读；合并排序只持**一个 run 一行**时用
    /// [`SpillSpace::open_run`] 的流式形态）。
    pub fn read_run(&self, run: usize) -> Result<Vec<Row>, ExecError> {
        let blob = self.read_blob(run)?;
        deserialize_rows(&blob)
    }

    /// 一个 run 的页数（诊断/测试）。
    #[must_use]
    pub fn run_pages(&self, run: usize) -> u32 {
        self.inner.borrow().runs[run].pages
    }

    /// run 数（诊断/测试）。
    #[must_use]
    pub fn runs(&self) -> usize {
        self.inner.borrow().runs.len()
    }

    /// 读回一个 run 的字节流（拼接各页块）。
    fn read_blob(&self, run: usize) -> Result<Vec<u8>, ExecError> {
        let inner = self.inner.borrow();
        let range = inner.runs[run];
        let mut blob = Vec::new();
        for i in 0..range.pages {
            let logical = range.first_logical + i;
            let page = inner.segment.read_page(logical).map_err(space_err)?;
            let len = u32::from_le_bytes(
                page.as_bytes()[PAGE_HEADER..PAGE_HEADER + 4]
                    .try_into()
                    .expect("4 字节"),
            ) as usize;
            blob.extend_from_slice(&page.as_bytes()[PAGE_HEADER + 4..PAGE_HEADER + 4 + len]);
            self.bytes_read.set(self.bytes_read.get() + len as u64);
        }
        Ok(blob)
    }
}

/// 分配一张临时页（必要时先扩展段——no-redo 直写形态；对照撤销段的
/// `plan_extend`：那里经池 + redo，这里不需要）。
fn allocate_page(inner: &mut SpillInner<'_>) -> Result<u32, ExecError> {
    let logical = inner.segment.allocate_append_page().map_err(space_err)?;
    if inner.segment.logical_block(logical).is_none() {
        inner.segment.extend().map_err(space_err)?;
    }
    Ok(logical)
}

fn space_err(e: SegmentSpaceError) -> ExecError {
    ExecError::Spill(e.to_string())
}

/// **行序列化**：`[u32 值数] + 各值 [u8 tag][载荷]`。
fn serialize_rows(rows: &[Row]) -> Result<Vec<u8>, ExecError> {
    let mut out = Vec::new();
    for row in rows {
        out.extend_from_slice(&(row.values.len() as u32).to_le_bytes());
        for v in &row.values {
            match v {
                Value::Null => out.push(0),
                Value::Bool(b) => {
                    out.push(1);
                    out.push(u8::from(*b));
                }
                Value::Number(n) => {
                    let bytes = n.encode();
                    out.push(2);
                    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
                    out.extend_from_slice(&bytes);
                }
                Value::Bytes(b) => {
                    out.push(3);
                    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                    out.extend_from_slice(b);
                }
            }
        }
    }
    Ok(out)
}

/// 从 `blob[*at..]` 解析一行；字节不足 ⇒ `Ok(None)`（**不消费**，等更多字节）。
fn try_parse_one_row(blob: &[u8], at: &mut usize) -> Result<Option<Row>, ExecError> {
    let mut p = *at;
    if p == blob.len() {
        return Ok(None);
    }
    if p + 4 > blob.len() {
        return Ok(None);
    }
    let n = u32::from_le_bytes(blob[p..p + 4].try_into().expect("4 字节")) as usize;
    p += 4;
    // 上限防御（损坏流里 n 是个巨数时不要预先分配）。
    if n > 1 << 20 {
        return Err(ExecError::Spill(format!("溢出流值数异常（{n}）")));
    }
    let mut values = Vec::with_capacity(n);
    for _ in 0..n {
        if p + 1 > blob.len() {
            return Ok(None);
        }
        let tag = blob[p];
        p += 1;
        let v = match tag {
            0 => Value::Null,
            1 => {
                if p + 1 > blob.len() {
                    return Ok(None);
                }
                let b = blob[p] != 0;
                p += 1;
                Value::Bool(b)
            }
            2 => {
                if p + 2 > blob.len() {
                    return Ok(None);
                }
                let len = u16::from_le_bytes(blob[p..p + 2].try_into().expect("2 字节")) as usize;
                p += 2;
                if p + len > blob.len() {
                    return Ok(None);
                }
                let num = Number::decode(&blob[p..p + len])
                    .map_err(|e| ExecError::Spill(format!("溢出 NUMBER 解码：{e}")))?;
                p += len;
                Value::Number(num)
            }
            3 => {
                if p + 4 > blob.len() {
                    return Ok(None);
                }
                let len = u32::from_le_bytes(blob[p..p + 4].try_into().expect("4 字节")) as usize;
                p += 4;
                if p + len > blob.len() {
                    return Ok(None);
                }
                let b = blob[p..p + len].to_vec();
                p += len;
                Value::Bytes(b)
            }
            _ => return Err(ExecError::Spill(format!("溢出流未知值 tag {tag}"))),
        };
        values.push(v);
    }
    *at = p;
    Ok(Some(Row::new(values)))
}

/// **行反序列化**（整流；截断/未知 tag 即具名错误）。
fn deserialize_rows(blob: &[u8]) -> Result<Vec<Row>, ExecError> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < blob.len() {
        match try_parse_one_row(blob, &mut at)? {
            Some(row) => out.push(row),
            None => return Err(ExecError::Spill("溢出流截断".to_owned())),
        }
    }
    Ok(out)
}

/// **run 的流式读游标**（一次只持一页字节 + 一行——归并用）。
struct RunCursor {
    next_logical: u32,
    pages_left: u32,
    bytes: Vec<u8>,
    at: usize,
}

impl RunCursor {
    fn new(range: RunRange) -> Self {
        Self {
            next_logical: range.first_logical,
            pages_left: range.pages,
            bytes: Vec::new(),
            at: 0,
        }
    }

    /// 取下一行（跨页按需续读；字节耗尽 ⇒ `None`）。
    fn next_row(&mut self, space: &SpillSpace<'_>) -> Result<Option<Row>, ExecError> {
        loop {
            if let Some(row) = try_parse_one_row(&self.bytes, &mut self.at)? {
                // 已消费部分及时丢弃（内存有界：≤ 一行 + 一页）。
                if self.at >= PAGE_CHUNK_MAX {
                    self.bytes.drain(..self.at);
                    self.at = 0;
                }
                return Ok(Some(row));
            }
            if self.pages_left == 0 {
                if self.bytes.len() > self.at {
                    return Err(ExecError::Spill("溢出流尾部残缺（截断）".to_owned()));
                }
                return Ok(None);
            }
            let page = space
                .inner
                .borrow()
                .segment
                .read_page(self.next_logical)
                .map_err(space_err)?;
            let len = u32::from_le_bytes(
                page.as_bytes()[PAGE_HEADER..PAGE_HEADER + 4]
                    .try_into()
                    .expect("4 字节"),
            ) as usize;
            self.bytes
                .extend_from_slice(&page.as_bytes()[PAGE_HEADER + 4..PAGE_HEADER + 4 + len]);
            space.bytes_read.set(space.bytes_read.get() + len as u64);
            self.next_logical += 1;
            self.pages_left -= 1;
        }
    }
}

/// **跨 run 的流式读游标**（哈希族的读回口：一个分区的若干 run 顺序拼接，
/// 一次只持一行 + 一页——与 `Sort` 的归并同内存界）。
pub struct RunStream<'s, 'io> {
    space: &'s SpillSpace<'io>,
    ranges: Vec<RunRange>,
    at: usize,
    cur: Option<RunCursor>,
}

impl<'s, 'io> RunStream<'s, 'io> {
    /// 取下一条（跨 run 自动续读；全部耗尽 ⇒ `None`）。
    pub fn next_row(&mut self) -> Result<Option<Row>, ExecError> {
        loop {
            if let Some(cur) = &mut self.cur {
                if let Some(row) = cur.next_row(self.space)? {
                    return Ok(Some(row));
                }
            }
            if self.at >= self.ranges.len() {
                return Ok(None);
            }
            self.cur = Some(RunCursor::new(self.ranges[self.at]));
            self.at += 1;
        }
    }
}

impl<'io> SpillSpace<'io> {
    /// **打开一个分区的读回流**：`runs` 里各 run 顺序拼接、流式读
    /// （哈希族的分区 = 若干 run——每次缓冲满写出一个 run）。
    pub fn open_runs(&self, runs: &[usize]) -> Result<RunStream<'_, 'io>, ExecError> {
        let inner = self.inner.borrow();
        let ranges: Vec<RunRange> = runs.iter().map(|&r| inner.runs[r]).collect();
        drop(inner);
        Ok(RunStream {
            space: self,
            ranges,
            at: 0,
            cur: None,
        })
    }
}

/// 归并堆项：`(行, run 号)`——比较由调用方给的闭包定。
struct MergeItem {
    row: Row,
    run: usize,
}

impl<'io> SpillSpace<'io> {
    /// **k 路归并**（有序 run → 有序流）：一次只持各 run 的**一行 + 一页**。
    ///
    /// `cmp` 由调用方给（排序键与方向的语义在 `Sort` 侧；本层只管归并）。
    /// 返回合并后的全部行（`Sort` 的上层再逐行吐出；结果集大小由调用方界定）。
    pub fn merge_runs<F>(&self, runs: &[usize], cmp: F) -> Result<Vec<Row>, ExecError>
    where
        F: Fn(&Row, &Row) -> Result<std::cmp::Ordering, ExecError>,
    {
        let mut cursors: Vec<RunCursor> = Vec::with_capacity(runs.len());
        let mut heap: Vec<MergeItem> = Vec::with_capacity(runs.len());
        for &r in runs {
            let mut c = RunCursor::new(self.inner.borrow().runs[r]);
            if let Some(row) = c.next_row(self)? {
                heap.push(MergeItem {
                    row,
                    run: cursors.len(),
                });
            }
            cursors.push(c);
        }
        // 手工最小堆（比较要能报错——不用 BinaryHeap）。
        fn sift_down<F>(heap: &mut [MergeItem], mut i: usize, cmp: &F) -> Result<(), ExecError>
        where
            F: Fn(&Row, &Row) -> Result<std::cmp::Ordering, ExecError>,
        {
            loop {
                let (l, r) = (2 * i + 1, 2 * i + 2);
                let mut min = i;
                if l < heap.len() && cmp(&heap[l].row, &heap[min].row)? == std::cmp::Ordering::Less
                {
                    min = l;
                }
                if r < heap.len() && cmp(&heap[r].row, &heap[min].row)? == std::cmp::Ordering::Less
                {
                    min = r;
                }
                if min == i {
                    return Ok(());
                }
                heap.swap(i, min);
                i = min;
            }
        }
        for i in (0..heap.len() / 2).rev() {
            sift_down(&mut heap, i, &cmp)?;
        }
        let mut out = Vec::new();
        while !heap.is_empty() {
            let top = heap.swap_remove(0);
            let run = top.run;
            out.push(top.row);
            if !heap.is_empty() {
                sift_down(&mut heap, 0, &cmp)?;
            }
            if let Some(next) = cursors[run].next_row(self)? {
                heap.push(MergeItem { row: next, run });
                // 上浮。
                let mut i = heap.len() - 1;
                while i > 0 {
                    let parent = (i - 1) / 2;
                    if cmp(&heap[i].row, &heap[parent].row)? == std::cmp::Ordering::Less {
                        heap.swap(i, parent);
                        i = parent;
                    } else {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bicdb_workspace::io::MemFileIo;
    use std::path::Path;

    const WS: [u8; 8] = [9u8; 8];

    #[test]
    fn serialize_roundtrip_is_lossless() {
        let rows = vec![
            Row::new(vec![Value::Null, Value::Bool(true)]),
            Row::new(vec![
                Value::Number(Number::parse("123.45").unwrap()),
                Value::Bytes(vec![1, 2, 3]),
            ]),
            Row::new(vec![Value::Bytes(b"hello".to_vec())]),
        ];
        let blob = serialize_rows(&rows).unwrap();
        assert_eq!(deserialize_rows(&blob).unwrap(), rows, "往返无损");
        // 截断 ⇒ 具名错误。
        assert!(deserialize_rows(&blob[..blob.len() - 1]).is_err());
    }

    #[test]
    fn spill_space_writes_and_reads_runs_across_pages() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let file = Box::leak(Box::new(
            DataFile::open_temp_reset(&io, Path::new("/mem/spill.dat"), WS, 512)
                .expect("临时数据文件"),
        ));
        let space =
            SpillSpace::create(file, bicdb_storage::temp::TempKind::Sort, WS).expect("溢出空间");
        // 3 万行（远超单页）⇒ 多页 run。
        let big: Vec<Row> = (0..30_000)
            .map(|i| {
                Row::new(vec![
                    Value::Number(Number::parse(&i.to_string()).unwrap()),
                    Value::Bytes(format!("payload-{i}").into_bytes()),
                ])
            })
            .collect();
        space.write_run(&big).unwrap();
        space.write_run(&big[..10]).unwrap();
        assert_eq!(space.runs(), 2);
        assert!(space.run_pages(0) > 1, "大 run 跨多页");
        assert_eq!(space.read_run(0).unwrap(), big, "跨页 run 往返一致");
        assert_eq!(space.read_run(1).unwrap(), big[..10].to_vec());
    }

    #[test]
    fn run_stream_concatenates_a_partition_across_runs_streaming() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let file = Box::leak(Box::new(
            DataFile::open_temp_reset(&io, Path::new("/mem/stream.dat"), WS, 512)
                .expect("临时数据文件"),
        ));
        let space =
            SpillSpace::create(file, bicdb_storage::temp::TempKind::Sort, WS).expect("溢出空间");
        let mk = |i: usize| Row::new(vec![Value::Number(Number::parse(&i.to_string()).unwrap())]);
        // 一个"分区" = 3 个 run（模拟分区缓冲满一次写一个 run）。
        let a: Vec<Row> = (0..20_000).map(mk).collect();
        let w0 = space.bytes_written();
        space.write_run(&a).unwrap();
        space.write_run(&a[..5]).unwrap();
        space.write_run(&a[..7]).unwrap();
        let part_bytes = space.bytes_written() - w0;
        let _other = space.write_run(&[mk(999)]).unwrap(); // 别的分区（不读）
        let r0 = space.bytes_read();

        let mut stream = space.open_runs(&[0, 1, 2]).unwrap();
        let mut got = Vec::new();
        while let Some(row) = stream.next_row().unwrap() {
            got.push(row);
        }
        let expect: Vec<Row> = a
            .iter()
            .cloned()
            .chain(a[..5].iter().cloned())
            .chain(a[..7].iter().cloned())
            .collect();
        assert_eq!(got, expect, "跨 run 顺序拼接、逐行一致");
        assert_eq!(
            space.bytes_read() - r0,
            part_bytes,
            "读回字节 = 该分区写出字节（不含别的 run）"
        );
    }
}
