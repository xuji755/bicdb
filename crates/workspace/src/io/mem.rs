//! 内存后端：与 [`OsFileIo`](super::OsFileIo) 同一套契约，供测试使用。
//!
//! 目的是**同一份契约测试可在两个实现上跑**（见 `tests/file_io.rs` 的一致性
//! 用例），以及让依赖文件 IO 的后续阶段（存储、WAL……）在无盘条件下单测。
//!
//! 与 OS 实现的已知差异（均已在契约测试中回避约定）：
//! - **不做父目录检查**——测试路径通常扁平（直接位于测试根下）；
//! - **无符号链接概念**——符号链接用例是 OS 实现专属；
//! - `sync_data` / `sync_all` / `sync_dir` 为 no-op（内存里已"落盘"）。

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::{handle_not_found, invalid_input, FileHandle, FileIo, HandleKind, OpenOptions};

struct MemHandle {
    path: PathBuf,
    kind: HandleKind,
    read: bool,
    write: bool,
}

#[derive(Default)]
struct State {
    next_id: u64,
    files: HashMap<PathBuf, Vec<u8>>,
    dirs: HashSet<PathBuf>,
    handles: HashMap<u64, MemHandle>,
}

/// 内存文件系统实现（测试用）。
#[derive(Default)]
pub struct MemFileIo {
    state: Mutex<State>,
}

impl MemFileIo {
    /// 新建空的文件系统。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 预置一个目录（**仅本测试后端提供**；生产路径的目录由文件系统维护）。
    pub fn add_dir(&self, path: impl Into<PathBuf>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.dirs.insert(path.into());
    }

    /// 读取某个路径的完整内容（**仅本测试后端提供**；比如断言部分写的结果）。
    #[must_use]
    pub fn contents(&self, path: &Path) -> Option<Vec<u8>> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.files.get(path).cloned()
    }
}

impl FileIo for MemFileIo {
    fn open(&self, path: &Path, opts: OpenOptions) -> io::Result<FileHandle> {
        opts.validate()?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.dirs.contains(path) {
            return Err(invalid_input(format!("{path:?} 是目录")));
        }
        let exists = state.files.contains_key(path);
        if exists && opts.is_create_new() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{path:?} 已存在"),
            ));
        }
        if !exists {
            if opts.is_create() || opts.is_create_new() {
                state.files.insert(path.to_path_buf(), Vec::new());
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{path:?} 不存在"),
                ));
            }
        }
        if opts.is_truncate() {
            if let Some(data) = state.files.get_mut(path) {
                data.clear();
            }
        }
        state.next_id += 1;
        let id = state.next_id;
        state.handles.insert(
            id,
            MemHandle {
                path: path.to_path_buf(),
                kind: HandleKind::File,
                read: opts.is_read(),
                write: opts.is_write(),
            },
        );
        Ok(FileHandle::from_raw(id))
    }

    fn open_dir(&self, path: &Path) -> io::Result<FileHandle> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.files.contains_key(path) {
            return Err(invalid_input(format!("{path:?} 不是目录")));
        }
        if !state.dirs.contains(path) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("目录 {path:?} 不存在"),
            ));
        }
        state.next_id += 1;
        let id = state.next_id;
        state.handles.insert(
            id,
            MemHandle {
                path: path.to_path_buf(),
                kind: HandleKind::Dir,
                read: true,
                write: false,
            },
        );
        Ok(FileHandle::from_raw(id))
    }

    fn read_at(&self, handle: FileHandle, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let h = state
            .handles
            .get(&handle.as_raw())
            .ok_or_else(|| handle_not_found(handle))?;
        if h.kind != HandleKind::File {
            return Err(invalid_input("目录句柄不支持 read_at"));
        }
        if !h.read {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "句柄未以读方式打开",
            ));
        }
        let data = state
            .files
            .get(&h.path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "文件不存在"))?;
        let Ok(off) = usize::try_from(offset) else {
            return Ok(0);
        };
        if off >= data.len() {
            return Ok(0);
        }
        let n = buf.len().min(data.len() - off);
        buf[..n].copy_from_slice(&data[off..off + n]);
        Ok(n)
    }

    fn write_at(&self, handle: FileHandle, buf: &[u8], offset: u64) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let h = state
            .handles
            .get(&handle.as_raw())
            .ok_or_else(|| handle_not_found(handle))?;
        if h.kind != HandleKind::File {
            return Err(invalid_input("目录句柄不支持 write_at"));
        }
        if !h.write {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "句柄未以写方式打开",
            ));
        }
        let path = h.path.clone();
        let data = state
            .files
            .get_mut(&path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "文件不存在"))?;
        let start = usize::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "偏移超出可寻址范围"))?;
        let end = start
            .checked_add(buf.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "偏移超出可寻址范围"))?;
        // 越过末尾的写入以 0 扩展（与 OS 稀疏语义一致）。
        if data.len() < end {
            data.resize(end, 0);
        }
        data[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn size(&self, handle: FileHandle) -> io::Result<u64> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let h = state
            .handles
            .get(&handle.as_raw())
            .ok_or_else(|| handle_not_found(handle))?;
        if h.kind != HandleKind::File {
            return Err(invalid_input("目录句柄不支持 size"));
        }
        let data = state
            .files
            .get(&h.path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "文件不存在"))?;
        Ok(data.len() as u64)
    }

    fn set_len(&self, handle: FileHandle, len: u64) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let h = state
            .handles
            .get(&handle.as_raw())
            .ok_or_else(|| handle_not_found(handle))?;
        if h.kind != HandleKind::File {
            return Err(invalid_input("目录句柄不支持 set_len"));
        }
        if !h.write {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "句柄未以写方式打开",
            ));
        }
        let path = h.path.clone();
        let new_len = usize::try_from(len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "长度超出可寻址范围"))?;
        let data = state
            .files
            .get_mut(&path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "文件不存在"))?;
        data.resize(new_len, 0);
        Ok(())
    }

    fn sync_data(&self, handle: FileHandle) -> io::Result<()> {
        self.check_file_handle(handle, "sync_data")
    }

    fn sync_all(&self, handle: FileHandle) -> io::Result<()> {
        self.check_file_handle(handle, "sync_all")
    }

    fn sync_dir(&self, handle: FileHandle) -> io::Result<()> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let h = state
            .handles
            .get(&handle.as_raw())
            .ok_or_else(|| handle_not_found(handle))?;
        if h.kind != HandleKind::Dir {
            return Err(invalid_input("sync_dir 需要目录句柄"));
        }
        Ok(())
    }

    fn close(&self, handle: FileHandle) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.handles.remove(&handle.as_raw()) {
            Some(_) => Ok(()),
            None => Err(handle_not_found(handle)),
        }
    }
}

impl MemFileIo {
    fn check_file_handle(&self, handle: FileHandle, what: &str) -> io::Result<()> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let h = state
            .handles
            .get(&handle.as_raw())
            .ok_or_else(|| handle_not_found(handle))?;
        if h.kind != HandleKind::File {
            return Err(invalid_input(format!("目录句柄不支持 {what}")));
        }
        Ok(())
    }
}
