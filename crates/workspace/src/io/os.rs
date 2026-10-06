//! 真实文件系统实现：可移植的"不跟随符号链接"打开 + 打开后复核。
//!
//! `ISO` REQ-ISO-007 / REQ-ISO-008：引擎打开受管理文件时**不跟随符号链接**，
//! 且**校验的是已打开的描述符**（fstat 语义），不是路径——"先检查路径、
//! 再按路径打开"永远有窗口；这里靠**打开后的复核**关闭它。
//!
//! # 为什么不硬编码 `O_NOFOLLOW`
//!
//! `REQ-PRT-003` 禁止依赖单一环境假设，而 `fcntl` 标志位恰恰是**随架构不同的**：
//! **arm64 与 x86_64 对同一语义给出不同的数值**——arm64 的资源头
//! （`arch/arm64/include/uapi/asm/fcntl.h`，注释写明"AArch32 (compat) support
//! 用自己的定义"）自带四个定义：`O_DIRECTORY = 040000`、`O_NOFOLLOW = 0100000`、
//! `O_DIRECT = 0200000`、`O_LARGEFILE = 0400000`；x86_64 没有该头，
//! 走 `asm-generic` 的经典值（`O_NOFOLLOW = 0040000`、`O_DIRECTORY = 0020000`）。
//!
//! **同一数值在另一架构上语义不同**（实测，2026-10-04）：
//!
//! | 平台 | 该架构的 `O_NOFOLLOW` | 用另一架构的数值打开符号链接 |
//! | --- | --- | --- |
//! | x86_64（RHEL 7.6 / 内核 3.10，192.168.30.33 实测） | `0o400000` | `0o100000` **静默跟随**（当地是 `O_LARGEFILE`） |
//! | aarch64（Ubuntu 24.04 / 内核 7.0，开发机实测） | `0o100000` | `0o400000` **静默跟随**（当地是 `O_LARGEFILE`） |
//!
//! 与 OS / 内核版本无关（上表两机年代相隔近十年），**只随架构**。
//! 硬编码任一个都会在另一架构上**静默失效**——这正是 `REQ-PRT-003`
//! 点名的失败形状。
//!
//! 因此这里用**与内核常量无关的等价语义**，顺序如下（每步都有明确意图）：
//!
//! 1. `lstat(path)`：路径本身是符号链接 → **拒绝**（快速路径，覆盖常见情形）；
//! 2. `open`：**不传 `O_TRUNC`**——打开不得有破坏性副作用；
//! 3. `fstat(已打开句柄)`：必须是普通文件，否则拒绝；
//! 4. 再次 `lstat(path)`：路径当前指向的实际对象必须与**句柄**同源
//!    （`dev`/`ino` 一致），否则拒绝——这一步关闭"检查与使用之间被换掉"
//!    的窗口（TOCTOU）；
//! 5. 只有全部通过，才执行 `truncate`（用 `set_len`，此时句柄已可信）。
//!
//! 整个过程中**没有任何读取发生**；破坏性动作一律在校验之后。
//!
//! 句柄表把 `File` 留在实现内部（**不使用 `unsafe`**：不手搓裸 fd）；
//! 句柄号单调分配、不复用。热路径优化（免查表直用 fd）后置，接口不变。

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::sync::Mutex;

use super::{handle_not_found, invalid_input, FileHandle, FileIo, HandleKind, OpenOptions};

struct Entry {
    file: File,
    kind: HandleKind,
    read: bool,
    write: bool,
}

#[derive(Default)]
struct State {
    next_id: u64,
    handles: HashMap<u64, Entry>,
}

/// 真实文件系统实现（生产用）。
#[derive(Default)]
pub struct OsFileIo {
    state: Mutex<State>,
}

impl OsFileIo {
    /// 新建（句柄表为空）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&self, file: File, kind: HandleKind, read: bool, write: bool) -> FileHandle {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.next_id += 1;
        let id = state.next_id;
        state.handles.insert(
            id,
            Entry {
                file,
                kind,
                read,
                write,
            },
        );
        FileHandle::from_raw(id)
    }

    fn with_entry<T>(
        &self,
        handle: FileHandle,
        f: impl FnOnce(&Entry) -> io::Result<T>,
    ) -> io::Result<T> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let entry = state
            .handles
            .get(&handle.as_raw())
            .ok_or_else(|| handle_not_found(handle))?;
        f(entry)
    }
}

fn require_file(entry: &Entry, what: &str) -> io::Result<()> {
    if entry.kind != HandleKind::File {
        return Err(invalid_input(format!("目录句柄不支持 {what}")));
    }
    Ok(())
}

fn require_write(entry: &Entry) -> io::Result<()> {
    if !entry.write {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "句柄未以写方式打开",
        ));
    }
    Ok(())
}

/// 拒绝符号链接的统一错误（`ISO` REQ-ISO-007："按打开失败处理"）。
fn symlink_refused(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{path:?} 拒绝打开：{why}"),
    )
}

/// 快速路径：路径本身就存在且是符号链接时直接拒绝（不产生任何打开副作用）。
///
/// 路径**不存在**时放行——是否创建由打开选项决定，创建出来的新对象由
/// 随后的复核负责（新建对象不可能是符号链接）。
fn reject_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(symlink_refused(path, "符号链接")),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// 复核：路径**当前**指向的对象必须与已打开句柄同源（同 `dev`/`ino`），
/// 且路径本身不是符号链接。不一致 = 检查与使用之间被换掉（TOCTOU）→ 拒绝。
fn verify_open_identity(path: &Path, opened: &fs::Metadata) -> io::Result<()> {
    let now = fs::symlink_metadata(path)?;
    if now.file_type().is_symlink() {
        return Err(symlink_refused(path, "复核时路径已变为符号链接"));
    }
    if now.dev() != opened.dev() || now.ino() != opened.ino() {
        return Err(symlink_refused(
            path,
            "复核时路径指向的对象与已打开句柄不一致",
        ));
    }
    Ok(())
}

impl FileIo for OsFileIo {
    fn open(&self, path: &Path, opts: OpenOptions) -> io::Result<FileHandle> {
        opts.validate()?;
        reject_symlink(path)?;
        let mut oo = std::fs::OpenOptions::new();
        // 注意：不传 truncate —— O_TRUNC 在**打开时**即产生副作用，
        // 而破坏性动作必须在复核之后（见模块文档的第 2、5 步）。
        oo.read(opts.is_read())
            .write(opts.is_write())
            .create(opts.is_create())
            .create_new(opts.is_create_new());
        let file = oo.open(path)?;
        // 以句柄为准：校验已打开对象。
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(invalid_input(format!("{path:?} 不是普通文件")));
        }
        verify_open_identity(path, &meta)?;
        // 复核之后才允许破坏性动作。
        if opts.is_truncate() {
            file.set_len(0)?;
        }
        Ok(self.insert(file, HandleKind::File, opts.is_read(), opts.is_write()))
    }

    fn open_dir(&self, path: &Path) -> io::Result<FileHandle> {
        reject_symlink(path)?;
        let file = File::open(path)?;
        let meta = file.metadata()?;
        if !meta.is_dir() {
            return Err(invalid_input(format!("{path:?} 不是目录")));
        }
        verify_open_identity(path, &meta)?;
        Ok(self.insert(file, HandleKind::Dir, true, false))
    }

    fn read_at(&self, handle: FileHandle, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.with_entry(handle, |entry| {
            require_file(entry, "read_at")?;
            if !entry.read {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "句柄未以读方式打开",
                ));
            }
            entry.file.read_at(buf, offset)
        })
    }

    fn write_at(&self, handle: FileHandle, buf: &[u8], offset: u64) -> io::Result<()> {
        self.with_entry(handle, |entry| {
            require_file(entry, "write_at")?;
            require_write(entry)?;
            // 全量或错误：循环处理短写。
            let mut done = 0usize;
            while done < buf.len() {
                match entry.file.write_at(&buf[done..], offset + done as u64) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "write_at 写入 0 字节",
                        ));
                    }
                    Ok(n) => done += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })
    }

    fn size(&self, handle: FileHandle) -> io::Result<u64> {
        self.with_entry(handle, |entry| {
            require_file(entry, "size")?;
            Ok(entry.file.metadata()?.len())
        })
    }

    fn set_len(&self, handle: FileHandle, len: u64) -> io::Result<()> {
        self.with_entry(handle, |entry| {
            require_file(entry, "set_len")?;
            require_write(entry)?;
            entry.file.set_len(len)
        })
    }

    fn sync_data(&self, handle: FileHandle) -> io::Result<()> {
        self.with_entry(handle, |entry| {
            require_file(entry, "sync_data")?;
            entry.file.sync_data()
        })
    }

    fn sync_all(&self, handle: FileHandle) -> io::Result<()> {
        self.with_entry(handle, |entry| {
            require_file(entry, "sync_all")?;
            entry.file.sync_all()
        })
    }

    fn sync_dir(&self, handle: FileHandle) -> io::Result<()> {
        self.with_entry(handle, |entry| {
            if entry.kind != HandleKind::Dir {
                return Err(invalid_input("sync_dir 需要目录句柄"));
            }
            entry.file.sync_all()
        })
    }

    fn close(&self, handle: FileHandle) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.handles.remove(&handle.as_raw()) {
            Some(_) => Ok(()),
            None => Err(handle_not_found(handle)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_base(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时钟应晚于 UNIX_EPOCH")
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("bicdb-io-os-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&base).expect("创建测试目录");
        base
    }

    #[test]
    fn symlink_target_is_not_followed() {
        let base = temp_base("nofollow");
        let real = base.join("real.dat");
        fs::write(&real, b"payload").unwrap();
        let link = base.join("link.dat");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let io = OsFileIo::new();
        let err = io
            .open(&link, OpenOptions::new().read(true))
            .expect_err("符号链接不得被跟随");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        // 真实文件本身可以打开。
        let h = io.open(&real, OpenOptions::new().read(true)).unwrap();
        let mut buf = [0u8; 7];
        assert_eq!(io.read_at(h, &mut buf, 0).unwrap(), 7);
        assert_eq!(&buf, b"payload");
        io.close(h).unwrap();

        fs::remove_dir_all(&base).expect("清理测试目录");
    }

    #[test]
    fn symlinked_dir_is_not_followed() {
        let base = temp_base("dirnofollow");
        let real_dir = base.join("realdir");
        fs::create_dir_all(&real_dir).unwrap();
        let link = base.join("linkdir");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();

        let io = OsFileIo::new();
        let err = io.open_dir(&link).expect_err("符号链接目录不得被跟随");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        let h = io.open_dir(&real_dir).unwrap();
        io.sync_dir(h).unwrap();
        io.close(h).unwrap();

        fs::remove_dir_all(&base).expect("清理测试目录");
    }

    #[test]
    fn symlink_with_truncate_does_not_touch_target() {
        let base = temp_base("truncguard");
        let real = base.join("real.dat");
        fs::write(&real, b"precious").unwrap();
        let link = base.join("link.dat");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let io = OsFileIo::new();
        assert!(
            io.open(&link, OpenOptions::new().write(true).truncate(true))
                .is_err(),
            "符号链接 + 截断必须拒绝"
        );
        assert_eq!(
            fs::read(&real).unwrap(),
            b"precious".to_vec(),
            "拒绝发生在任何破坏性动作之前：目标内容必须原样"
        );

        // create_new 经由 O_CREAT|O_EXCL 天然拒绝符号链接（即使目标不存在）。
        let dangling = base.join("dangling.dat");
        std::os::unix::fs::symlink(base.join("nowhere.dat"), &dangling).unwrap();
        assert!(
            io.open(&dangling, OpenOptions::new().write(true).create_new(true))
                .is_err(),
            "create_new 遇符号链接必须失败"
        );

        fs::remove_dir_all(&base).expect("清理测试目录");
    }
}
