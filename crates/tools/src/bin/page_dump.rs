//! page_dump —— 页转储（只读）
//!
//! 用法：`page_dump <页镜像文件> [页序号]`
//! 页镜像 = N × 16 KiB 的连续页（P2 尚无工作区文件格式，镜像即校验载体）。
//!
//! 退出码：0 成功；2 用法/IO 错误。

use std::process::ExitCode;

use bicdb_storage::page::PAGE_SIZE;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args.len() > 3 {
        eprintln!("用法：page_dump <页镜像文件> [页序号]");
        return ExitCode::from(2);
    }
    let bytes = match std::fs::read(&args[1]) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("读取 {} 失败：{e}", args[1]);
            return ExitCode::from(2);
        }
    };
    match args.get(2) {
        None => print!("{}", bicdb_tools::page_image_dump(&bytes)),
        Some(idx) => {
            let Ok(index) = idx.parse::<usize>() else {
                eprintln!("页序号非法：{idx}");
                return ExitCode::from(2);
            };
            let Some(span) = index
                .checked_mul(PAGE_SIZE)
                .and_then(|start| Some(start..start.checked_add(PAGE_SIZE)?))
            else {
                eprintln!(
                    "页序号 {index} 超出镜像范围（共 {} 页）",
                    bytes.len() / PAGE_SIZE
                );
                return ExitCode::from(2);
            };
            let Some(chunk) = bytes.get(span) else {
                eprintln!(
                    "页序号 {index} 超出镜像范围（共 {} 页）",
                    bytes.len() / PAGE_SIZE
                );
                return ExitCode::from(2);
            };
            let mut buf = Box::new([0u8; PAGE_SIZE]);
            buf.copy_from_slice(chunk);
            print!(
                "{}",
                bicdb_tools::page_dump(&bicdb_storage::page::Page::from_bytes(buf))
            );
        }
    }
    ExitCode::SUCCESS
}
