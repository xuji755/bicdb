//! db_check —— 检查器（只读）
//!
//! 用法：`db_check <页镜像文件>`
//! 页镜像 = N × 16 KiB 的连续页（P2 尚无工作区文件格式，镜像即校验载体）。
//!
//! 退出码（稳定）：0 = 可用；1 = 有错误（不可恢复）；2 = 用法/IO 错误。

use std::process::ExitCode;

use bicdb_tools::{check_page_image, Verdict};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("用法：db_check <页镜像文件>");
        return ExitCode::from(2);
    }
    let bytes = match std::fs::read(&args[1]) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("读取 {} 失败：{e}", args[1]);
            return ExitCode::from(2);
        }
    };
    let report = check_page_image(&bytes);
    print!("{}", report.render());
    match report.verdict() {
        Verdict::Usable => ExitCode::SUCCESS,
        Verdict::Repairable | Verdict::Unrecoverable => ExitCode::from(1),
    }
}
