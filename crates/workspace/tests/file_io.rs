//! FileIO 替换接口的契约一致性用例（P1 测试底座）。
//!
//! 同一份用例跑在两个实现上（`OsFileIo` 与 `MemFileIo`）——接口可替换的
//! 含义就是**契约等价**。OS 实现专属的用例（符号链接）单列在末尾。
//!
//! P1 验收对齐："测试工作者无法打开他人目录，线程任务与**句柄不串区**"——
//! 句柄串区/复用在这里被直接检验。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bicdb_workspace::io::{
    FaultInjecting, FaultOp, FaultRule, FileHandle, FileIo, MemFileIo, OpenOptions, OsFileIo,
};

fn unique_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟应晚于 UNIX_EPOCH")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "bicdb-io-test-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

/// 两个实现共同遵守的契约。
fn conformance<Io: FileIo>(io: &Io, root: &Path, ensure_dir: impl Fn(&Path)) {
    ensure_dir(root);
    let a = root.join("a.dat");
    let missing = root.join("missing.dat");

    // -- 创建、全量写、读回 -------------------------------------------------
    let h = io
        .open(
            &a,
            OpenOptions::new().read(true).write(true).create_new(true),
        )
        .expect("create_new 应成功");
    io.write_at(h, b"hello world", 0).unwrap();
    io.sync_data(h).unwrap();
    io.sync_all(h).unwrap();
    assert_eq!(io.size(h).unwrap(), 11);

    let mut buf = [0u8; 5];
    assert_eq!(io.read_at(h, &mut buf, 6).unwrap(), 5, "从偏移 6 读 5 字节");
    assert_eq!(&buf, b"world");

    // 短读是正常现象：只读 3 字节。
    let mut small = [0u8; 3];
    assert_eq!(io.read_at(h, &mut small, 0).unwrap(), 3);
    assert_eq!(&small, b"hel");

    // 越过末尾读 → 0。
    assert_eq!(io.read_at(h, &mut buf, 11).unwrap(), 0);
    assert_eq!(io.read_at(h, &mut buf, 9999).unwrap(), 0);

    // read_exact_at：足量成功；不足报 UnexpectedEof。
    let mut all = [0u8; 11];
    io.read_exact_at(h, &mut all, 0).unwrap();
    assert_eq!(&all, b"hello world");
    let mut too_much = [0u8; 12];
    assert_eq!(
        io.read_exact_at(h, &mut too_much, 0).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
    io.close(h).unwrap();

    // -- 关闭之后：句柄失效、且不复用 ---------------------------------------
    assert_eq!(
        io.size(h).unwrap_err().kind(),
        io::ErrorKind::NotFound,
        "已关闭句柄不可用"
    );
    assert_eq!(io.close(h).unwrap_err().kind(), io::ErrorKind::NotFound);

    let h2 = io.open(&a, OpenOptions::new().read(true)).unwrap();
    assert_ne!(h2, h, "句柄号不复用");
    io.close(h2).unwrap();

    // -- 越过末尾写以 0 扩展（OS 稀疏语义） ---------------------------------
    let h = io
        .open(&a, OpenOptions::new().read(true).write(true))
        .unwrap();
    io.write_at(h, b"X", 20).unwrap();
    assert_eq!(io.size(h).unwrap(), 21);
    let mut one = [1u8; 1];
    assert_eq!(io.read_at(h, &mut one, 11).unwrap(), 1);
    assert_eq!(one[0], 0, "空洞读为 0");
    io.close(h).unwrap();

    // -- set_len：收缩与扩展 -------------------------------------------------
    let h = io
        .open(&a, OpenOptions::new().read(true).write(true))
        .unwrap();
    io.set_len(h, 5).unwrap();
    assert_eq!(io.size(h).unwrap(), 5);
    io.set_len(h, 7).unwrap();
    assert_eq!(io.size(h).unwrap(), 7);
    let mut tail = [1u8; 2];
    io.read_exact_at(h, &mut tail, 5).unwrap();
    assert_eq!(tail, [0, 0], "扩展部分读为 0");

    // -- 权限：未以写方式打开 -------------------------------------------------
    let ro = io.open(&a, OpenOptions::new().read(true)).unwrap();
    assert!(io.write_at(ro, b"x", 0).is_err(), "只读句柄不可写");
    assert!(io.set_len(ro, 1).is_err(), "只读句柄不可截断");
    io.close(ro).unwrap();

    let wo = io.open(&a, OpenOptions::new().write(true)).unwrap();
    assert!(io.read_at(wo, &mut one, 0).is_err(), "只写句柄不可读");
    io.close(wo).unwrap();
    io.close(h).unwrap();

    // -- 已存在 / 不存在 -----------------------------------------------------
    assert_eq!(
        io.open(&a, OpenOptions::new().write(true).create_new(true))
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(
        io.open(&missing, OpenOptions::new().read(true))
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );

    // -- truncate ------------------------------------------------------------
    let h = io
        .open(&a, OpenOptions::new().write(true).truncate(true))
        .unwrap();
    assert_eq!(io.size(h).unwrap(), 0);
    io.close(h).unwrap();

    // -- 目录句柄 ------------------------------------------------------------
    assert_eq!(
        io.open_dir(&missing).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    let d = io.open_dir(root).unwrap();
    io.sync_dir(d).unwrap();
    assert!(io.read_at(d, &mut one, 0).is_err(), "目录句柄不可读");
    assert!(io.write_at(d, b"x", 0).is_err(), "目录句柄不可写");
    assert!(io.size(d).is_err(), "目录句柄无 size");
    assert!(io.sync_data(d).is_err(), "目录用 sync_dir");
    io.close(d).unwrap();

    // 用 open() 打开目录、用 open_dir() 打开文件：都要拒绝。
    assert!(io.open(root, OpenOptions::new().read(true)).is_err());
    assert!(io.open_dir(&a).is_err());

    // -- 打开选项校验 --------------------------------------------------------
    assert!(
        io.open(&a, OpenOptions::new()).is_err(),
        "必须至少 read/write"
    );
    assert!(
        io.open(&a, OpenOptions::new().read(true).truncate(true))
            .is_err(),
        "truncate 需要 write"
    );
}

#[test]
fn os_and_mem_implementations_share_the_contract() {
    let root = unique_root("os");
    let os = OsFileIo::new();
    conformance(&os, &root, |p| {
        std::fs::create_dir_all(p).expect("创建测试目录");
    });
    std::fs::remove_dir_all(&root).expect("清理测试目录");

    let root = unique_root("mem");
    let mem = MemFileIo::new();
    conformance(&mem, &root, |p| mem.add_dir(p));
}

#[test]
fn fault_injection_is_deterministic() {
    let root = PathBuf::from("/mem");
    let io = FaultInjecting::new(MemFileIo::new());
    io.inner().add_dir(root.clone());
    let a = root.join("f.dat");

    // 第 2 次写失败一次，第 3 次恢复。
    io.add_rule(FaultRule::once(FaultOp::Write, 2, io::ErrorKind::Other));
    let h = io
        .open(&a, OpenOptions::new().write(true).create(true))
        .unwrap();
    io.write_at(h, b"one", 0).unwrap();
    assert_eq!(
        io.write_at(h, b"two", 3).unwrap_err().kind(),
        io::ErrorKind::Other,
        "第 2 次写被注入失败"
    );
    io.write_at(h, b"three", 3).unwrap();
    assert_eq!(io.inner().contents(&a).unwrap(), b"onethree".to_vec());
    io.close(h).unwrap();

    // 短读：第 2 次读只返回 2 字节，但成功。
    io.add_rule(FaultRule::short_read(2, 2));
    let mut buf = [0u8; 8];
    let h = io.open(&a, OpenOptions::new().read(true)).unwrap();
    assert_eq!(io.read_at(h, &mut buf, 0).unwrap(), 8, "第 1 次读正常");
    assert_eq!(io.read_at(h, &mut buf, 0).unwrap(), 2, "第 2 次读短读");
    assert_eq!(&buf[..2], b"on");
    io.close(h).unwrap();

    // 计数可重置：重置后第 1 次读又正常。
    io.reset_counters();
    let h = io.open(&a, OpenOptions::new().read(true)).unwrap();
    assert_eq!(io.read_at(h, &mut buf, 0).unwrap(), 8);
    io.close(h).unwrap();

    // sync 注入失败：数据仍在（说明失败发生在"落盘"环节）。
    io.add_rule(FaultRule::fail_from(
        FaultOp::SyncData,
        1,
        io::ErrorKind::Other,
    ));
    let h = io
        .open(&a, OpenOptions::new().read(true).write(true))
        .unwrap();
    io.write_at(h, b"data", 0).unwrap();
    assert!(io.sync_data(h).is_err());
    assert_eq!(&io.inner().contents(&a).unwrap()[..4], b"data");
    io.close(h).unwrap();

    // open 注入失败：文件不存在时不产生任何副作用。
    io.add_rule(FaultRule::fail_from(
        FaultOp::Open,
        1,
        io::ErrorKind::OutOfMemory,
    ));
    assert!(io.open(&a, OpenOptions::new().read(true)).is_err());
}

#[test]
fn handles_do_not_leak_across_threads() {
    let root = unique_root("threads");
    std::fs::create_dir_all(&root).expect("创建测试目录");

    let io: Arc<dyn FileIo> = Arc::new(OsFileIo::new());
    let (tx, rx) = std::sync::mpsc::channel::<FileHandle>();

    let t1 = {
        let io = Arc::clone(&io);
        let path = root.join("w1.dat");
        std::thread::spawn(move || {
            let h = io
                .open(&path, OpenOptions::new().write(true).create_new(true))
                .unwrap();
            io.write_at(h, b"thread-1", 0).unwrap();
            tx.send(h).unwrap();
            io.close(h).unwrap();
        })
    };

    // 另一个线程全程不受影响，并且**无法使用** t1 已关闭的句柄。
    let t2 = {
        let io = Arc::clone(&io);
        let path = root.join("w2.dat");
        std::thread::spawn(move || {
            let foreign = rx.recv().unwrap();
            let h = io
                .open(&path, OpenOptions::new().write(true).create_new(true))
                .unwrap();
            io.write_at(h, b"thread-2", 0).unwrap();
            let mut buf = [0u8; 8];
            assert!(io.read_at(h, &mut buf, 0).is_err(), "只写句柄不可读");
            io.close(h).unwrap();

            // 跨线程使用他人句柄：该句柄在发送方已被关闭 → 一律 NotFound，
            // 不会命中任何其他文件（句柄不串区）。
            assert_eq!(
                io.size(foreign).unwrap_err().kind(),
                io::ErrorKind::NotFound
            );
        })
    };

    t1.join().unwrap();
    t2.join().unwrap();
    std::fs::remove_dir_all(&root).expect("清理测试目录");
}
