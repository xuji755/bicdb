//! page_dump —— 页转储（只读）
//!
//! 用法：`page_dump <页镜像文件> [页序号]`
//! 页镜像 = N × 16 KiB 的连续页（P2 尚无工作区文件格式，镜像即校验载体）。
//!
//! **流式读**：全量转储一次一页地读、边转边写，不在内存里放整个镜像
//! （诊断工具的用处正是"库出问题时"，镜像可以比内存大）；指定页序号时按
//! 偏移定位读一页，不碰其余部分。
//!
//! 退出码：0 成功；2 用法/IO 错误。

use std::io::{Read, Seek, SeekFrom, Write};
use std::process::ExitCode;

use bicdb_storage::page::PAGE_SIZE;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args.len() > 3 {
        eprintln!("用法：page_dump <页镜像文件> [页序号]");
        return ExitCode::from(2);
    }
    let mut file = match std::fs::File::open(&args[1]) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("读取 {} 失败：{e}", args[1]);
            return ExitCode::from(2);
        }
    };
    let Some(idx) = args.get(2) else {
        // 全量转储（流式）。
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        if let Err(e) = bicdb_tools::page_image_dump_stream(&mut file, &mut lock) {
            eprintln!("读 {} 失败：{e}", args[1]);
            return ExitCode::from(2);
        }
        let _ = lock.flush();
        return ExitCode::SUCCESS;
    };
    // 单页转储：定位读一页（不读整个文件）。
    let Ok(index) = idx.parse::<usize>() else {
        eprintln!("页序号非法：{idx}");
        return ExitCode::from(2);
    };
    let size = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            eprintln!("读 {} 失败：{e}", args[1]);
            return ExitCode::from(2);
        }
    };
    let pages = size as usize / PAGE_SIZE;
    // **溢出即超范围**（不是 panic）：序号是用户输入，`usize::MAX` 乘以页大小
    // 会溢出——原先用 `checked_mul`，重写时丢了，测试当场抓住。
    let Some(offset) = index.checked_mul(PAGE_SIZE).filter(|off| {
        off.checked_add(PAGE_SIZE)
            .is_some_and(|end| end <= size as usize)
    }) else {
        eprintln!("页序号 {index} 超出镜像范围（共 {pages} 页）");
        return ExitCode::from(2);
    };
    let mut buf = Box::new([0u8; PAGE_SIZE]);
    if let Err(e) = file
        .seek(SeekFrom::Start(offset as u64))
        .and_then(|_| file.read_exact(&mut buf[..]))
    {
        eprintln!("读 {} 失败：{e}", args[1]);
        return ExitCode::from(2);
    }
    print!(
        "{}",
        bicdb_tools::page_dump(&bicdb_storage::page::Page::from_bytes(buf))
    );
    ExitCode::SUCCESS
}
