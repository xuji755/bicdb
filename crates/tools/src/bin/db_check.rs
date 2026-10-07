//! db_check —— 检查器（只读）
//!
//! 用法：`db_check <页镜像文件> [--max-findings N]`
//! 页镜像 = N × 16 KiB 的连续页（P2 尚无工作区文件格式，镜像即校验载体）。
//!
//! **流式读**（一次一页）：镜像可以比内存大——诊断工具的用处正是"库出问题时"。
//! `--max-findings`（默认 1000）只限**报告长度**；判定与计数是全量。
//!
//! 退出码（稳定）：0 = 可用；1 = 有错误（不可恢复）；2 = 用法/IO 错误。

use std::process::ExitCode;

use bicdb_tools::{check_page_image_stream, Verdict};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut path: Option<&String> = None;
    let mut max_findings = 1000usize;
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--max-findings" => {
                let Some(v) = it.next() else {
                    eprintln!("--max-findings 缺数值");
                    return ExitCode::from(2);
                };
                match v.parse::<usize>() {
                    Ok(n) if n > 0 => max_findings = n,
                    _ => {
                        eprintln!("--max-findings 要正整数，给的是 `{v}`");
                        return ExitCode::from(2);
                    }
                }
            }
            other if other.starts_with('-') => {
                eprintln!(
                    "不认识的选项 `{other}`（用法：db_check <页镜像文件> [--max-findings N]）"
                );
                return ExitCode::from(2);
            }
            other => {
                let _ = other;
                path = Some(a);
            }
        }
    }
    let Some(path) = path else {
        eprintln!("用法：db_check <页镜像文件> [--max-findings N]");
        return ExitCode::from(2);
    };
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("读取 {path} 失败：{e}");
            return ExitCode::from(2);
        }
    };
    let report = match check_page_image_stream(&mut file, max_findings) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("读 {path} 失败：{e}");
            return ExitCode::from(2);
        }
    };
    print!("{}", report.render());
    match report.verdict() {
        Verdict::Usable => ExitCode::SUCCESS,
        Verdict::Repairable | Verdict::Unrecoverable => ExitCode::from(1),
    }
}
