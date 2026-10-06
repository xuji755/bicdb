//! **bicdb 命令行**：建区 / 执行 SQL / 交互式 shell。
//!
//! ```text
//! bicdb init  <dir>              建区（字典 + 撤销段 + 控制文件 + 日志组）
//! bicdb sql   <dir> "<SQL>" …    执行（多条用 `;` 分隔；`-` = 从 stdin 读）
//! bicdb shell <dir>              交互式（读一条跑一条）
//! bicdb version
//! ```
//!
//! **退出码**：0 = 成功；1 = 语句/建区错误；2 = 用法错误。
//!
//! **收尾纪律**：正常退出前跑**完全检查点**（[`boot::Instance::shutdown`]），
//! 让下次打开的恢复范围归零；异常退出（崩溃）由 `open` 的三阶段恢复兜底。

#![forbid(unsafe_code)]

use std::io::{BufRead, Read, Write};
use std::process::ExitCode;

use bicdb_cli::boot;
use bicdb_sql::session::{QueryResult, Session, SessionError};

const USAGE: &str = "\
bicdb —— 带撤销/日志的页式数据库（V1.0 单工作区）

用法：
  bicdb init  <dir>               建区
  bicdb sql   <dir> <SQL>…        执行 SQL（多条用 `;` 分隔；`-` = 读 stdin）
  bicdb shell <dir>               交互式 shell
  bicdb version                   版本
  bicdb help                      本帮助

示例：
  bicdb init ./demo
  bicdb sql ./demo \"CREATE TABLE t (id NUMBER NOT NULL, name VARCHAR2(32))\"
  bicdb sql ./demo \"INSERT INTO t VALUES (1, 'a')\"
  bicdb sql ./demo \"SELECT * FROM t\"
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Exit::Usage(msg)) => {
            eprintln!("用法错误：{msg}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Exit::Failed(msg)) => {
            eprintln!("bicdb：{msg}");
            ExitCode::from(1)
        }
    }
}

enum Exit {
    Usage(String),
    Failed(String),
}

impl From<SessionError> for Exit {
    fn from(e: SessionError) -> Self {
        Exit::Failed(e.to_string())
    }
}

impl From<boot::BootError> for Exit {
    fn from(e: boot::BootError) -> Self {
        Exit::Failed(e.to_string())
    }
}

fn run(args: &[String]) -> Result<(), Exit> {
    let Some(cmd) = args.first().map(String::as_str) else {
        return Err(Exit::Usage("缺少子命令".to_owned()));
    };
    match cmd {
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        "version" | "--version" | "-V" => {
            println!("bicdb {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "init" => {
            let dir = args
                .get(1)
                .ok_or_else(|| Exit::Usage("init 缺目录".to_owned()))?;
            let inst = boot::create_instance(std::path::Path::new(dir))?;
            inst.shutdown()?;
            println!("已建区：{dir}");
            println!("  工作区    {}", String::from_utf8_lossy(&boot::WS));
            println!("  日志组    {}", boot::group_spec().group_count);
            println!("  下一步    bicdb sql {dir} \"SELECT * FROM t\"");
            Ok(())
        }
        "sql" => {
            let dir = args
                .get(1)
                .ok_or_else(|| Exit::Usage("sql 缺目录".to_owned()))?;
            let mut inst = boot::open_instance(std::path::Path::new(dir))?;
            banner_brief(&inst);
            let text = sql_text(&args[2..])?;
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let r = with_session(&mut inst, |s| {
                for r in s.execute(&text)? {
                    print_result(&mut out, &r)?;
                }
                Ok(())
            });
            let closed = inst.shutdown();
            r?;
            closed?;
            out.flush().ok();
            Ok(())
        }
        "shell" => {
            let dir = args
                .get(1)
                .ok_or_else(|| Exit::Usage("shell 缺目录".to_owned()))?;
            let mut inst = boot::open_instance(std::path::Path::new(dir))?;
            banner(&inst);
            let r = repl(&mut inst);
            let closed = inst.shutdown();
            r?;
            closed?;
            Ok(())
        }
        other => Err(Exit::Usage(format!("未知子命令 `{other}`"))),
    }
}

/// 会话一开一关（`sql` 子命令；语句文本可含多条）。
fn with_session<R>(
    inst: &mut boot::Instance,
    f: impl FnOnce(&mut Session<'_, '_, '_, '_>) -> Result<R, Exit>,
) -> Result<R, Exit> {
    let seq = inst.seq();
    let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
    f(&mut session)
}

/// `sql` 子命令的文本来源：给了参数就拼接；`-` 或缺参数 ⇒ 读 stdin。
fn sql_text(rest: &[String]) -> Result<String, Exit> {
    let joined = rest.join(" ");
    if !joined.trim().is_empty() && joined.trim() != "-" {
        return Ok(joined);
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| Exit::Failed(format!("读 stdin 失败：{e}")))?;
    Ok(buf)
}

// ───────────────────────────── 呈现 ─────────────────────────────

fn banner(inst: &boot::Instance) {
    println!(
        "bicdb {} —— 实例 {}（提交序号 {}）",
        env!("CARGO_PKG_VERSION"),
        inst.dir.display(),
        inst.seq()
    );
    if let Some(r) = inst.recovery {
        println!(
            "恢复：起点 LSN {}，重放 {} 块，回滚 {} 个事务，续写位 {}",
            r.start_lsn, r.applied_blocks, r.txns_rolled_back, r.log_end
        );
    }
    println!("输入 SQL（`;` 结尾执行；`.quit` 退出，`.help` 帮助）");
}

fn banner_brief(inst: &boot::Instance) {
    if let Some(r) = inst.recovery {
        if r.applied_blocks > 0 || r.txns_rolled_back > 0 {
            eprintln!(
                "（恢复：重放 {} 块，回滚 {} 个事务）",
                r.applied_blocks, r.txns_rolled_back
            );
        }
    }
}

fn print_result(out: &mut impl Write, r: &QueryResult) -> Result<(), Exit> {
    let io = |e: std::io::Error| Exit::Failed(format!("写输出失败：{e}"));
    match r {
        QueryResult::Rows { columns, rows } => {
            print_table(out, columns, rows).map_err(io)?;
        }
        QueryResult::Affected(n) => writeln!(out, "影响 {n} 行").map_err(io)?,
        QueryResult::Ddl(s) => writeln!(out, "{s}").map_err(io)?,
        QueryResult::Txn(s) => writeln!(out, "{s}").map_err(io)?,
    }
    Ok(())
}

/// 表格（列宽按内容取，`NULL` 显式写出）。
fn print_table(
    out: &mut impl Write,
    columns: &[String],
    rows: &[Vec<String>],
) -> std::io::Result<()> {
    let mut width: Vec<usize> = columns.iter().map(|c| c.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < width.len() {
                width[i] = width[i].max(cell.chars().count());
            }
        }
    }
    let line = |out: &mut dyn Write| -> std::io::Result<()> {
        for (i, w) in width.iter().enumerate() {
            if i > 0 {
                write!(out, "-+-")?;
            }
            write!(out, "{}", "-".repeat(*w))?;
        }
        writeln!(out)
    };
    // 表头
    for (i, c) in columns.iter().enumerate() {
        if i > 0 {
            write!(out, " | ")?;
        }
        write!(out, "{c:<width$}", width = width[i])?;
    }
    writeln!(out)?;
    line(out)?;
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                write!(out, " | ")?;
            }
            write!(out, "{cell:<width$}", width = width[i])?;
        }
        writeln!(out)?;
    }
    writeln!(out, "（{} 行）", rows.len())
}

// ───────────────────────────── 交互式 ─────────────────────────────

fn repl(inst: &mut boot::Instance) -> Result<(), Exit> {
    // 会话常驻整场（`Session` 借住实例；收尾在 `Drop` —— 未提交的显式事务回滚）。
    let seq = inst.seq();
    let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
    let stdin = std::io::stdin();
    let mut buf = String::new();
    let mut pending = String::new();
    loop {
        let prompt = if pending.trim().is_empty() {
            "bicdb> "
        } else {
            "   ...> "
        };
        print!("{prompt}");
        std::io::stdout().flush().ok();
        buf.clear();
        let n = stdin
            .lock()
            .read_line(&mut buf)
            .map_err(|e| Exit::Failed(format!("读 stdin 失败：{e}")))?;
        if n == 0 {
            println!();
            return Ok(()); // EOF
        }
        let line = buf.trim_end();
        if pending.trim().is_empty() {
            match line.trim() {
                "" => continue,
                ".quit" | ".exit" => return Ok(()),
                ".help" => {
                    println!("`;` 结尾执行；多条语句一次执行；`.quit` 退出");
                    continue;
                }
                _ => {}
            }
        }
        pending.push_str(line);
        pending.push('\n');
        if !pending.trim_end().ends_with(';') {
            continue;
        }
        let text = std::mem::take(&mut pending);
        match session.execute(&text) {
            Ok(results) => {
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                for r in results {
                    print_result(&mut out, &r)?;
                }
                out.flush().ok();
            }
            Err(e) => eprintln!("错误：{e}"),
        }
    }
}
