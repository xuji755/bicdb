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
use bicdb_exec::Value;
use bicdb_sql::session::{QueryResult, Session, SessionError};

const USAGE: &str = "\
bicdb —— 带撤销/日志的页式数据库（V1.0 单工作区）

用法：
  bicdb init  <dir>               建区
  bicdb sql   <dir> <SQL>…       执行 SQL（多条用 `;` 分隔；`-` = 读 stdin）
              [--param 名=值 …]   给语句里的 `:名` 传值（可重复）
  bicdb shell <dir>               交互式 shell
  bicdb version                   版本
  bicdb help                      本帮助

示例：
  bicdb init ./demo
  bicdb sql ./demo \"CREATE TABLE t (id NUMBER NOT NULL, name VARCHAR2(32))\"
  bicdb sql ./demo \"INSERT INTO t VALUES (1, 'a')\"
  bicdb sql ./demo --param id=1 \"SELECT name FROM t WHERE id = :id\"
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

impl Exit {
    /// 供"两条错一起报"的场合取文本。
    fn message(&self) -> &str {
        match self {
            Exit::Usage(m) | Exit::Failed(m) => m,
        }
    }
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
            let mut inst = boot::create_instance(std::path::Path::new(dir))?;
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
            let (params, sql_args) = split_params(&args[2..])?;
            let mut inst = boot::open_instance(std::path::Path::new(dir))?;
            banner_brief(&inst);
            let text = sql_text(&sql_args)?;
            let named: Vec<(&str, Value)> = params
                .iter()
                .map(|(n, v)| (n.as_str(), v.clone()))
                .collect();
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let r = with_session(&mut inst, |s| {
                for r in s.execute_with_params(&text, &named)? {
                    print_result(&mut out, &r)?;
                }
                Ok(())
            });
            let closed = inst.shutdown();
            // 两条错都要报（会话错在前也不能把关闭失败吞掉）。
            match (r, closed) {
                (Err(e), Ok(())) => return Err(e),
                (Ok(()), Err(e)) => return Err(e.into()),
                (Err(e), Err(c)) => {
                    return Err(Exit::Failed(format!("{}；关闭时又出错：{c}", e.message())))
                }
                (Ok(()), Ok(())) => {}
            }
            out.flush()
                .map_err(|e| Exit::Failed(format!("写输出失败：{e}")))?;
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
            match (r, closed) {
                (Err(e), Ok(())) => return Err(e),
                (Ok(()), Err(e)) => return Err(e.into()),
                (Err(e), Err(c)) => {
                    return Err(Exit::Failed(format!("{}；关闭时又出错：{c}", e.message())))
                }
                (Ok(()), Ok(())) => {}
            }
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

/// **一句话完了吗**（`;` 结尾，**行注释不算**）。
///
/// `SELECT 1; -- 说明` 这种输入此前永远等不到 `;` 结尾（行尾是注释）——
/// 语句挂起、不执行、不报错。这里按"剥掉行注释再看"的口径判定。
fn ends_statement(text: &str) -> bool {
    let mut last_sig = String::new();
    for line in text.lines() {
        let code = match line.find("--") {
            Some(at) => &line[..at],
            None => line,
        };
        if !code.trim().is_empty() {
            last_sig = code.trim_end().to_owned();
        }
    }
    last_sig.ends_with(';')
}

/// `--param` 解析结果：`(参数表, 其余实参 = SQL 文本)`。
type ParamsAndSql = (Vec<(String, Value)>, Vec<String>);

/// **切出 `--param 名=值`**（其余是 SQL 文本）。
fn split_params(args: &[String]) -> Result<ParamsAndSql, Exit> {
    let mut params = Vec::new();
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--param" || a == "-p" {
            let kv = it
                .next()
                .ok_or_else(|| Exit::Usage("--param 缺 `名=值`".to_owned()))?;
            params.push(parse_param(kv)?);
        } else {
            rest.push(a.clone());
        }
    }
    Ok((params, rest))
}

/// `名=值` 的值文本 → [`Value`]：`NULL` / `TRUE|FALSE` / `'文本'` / 其余按数值。
fn parse_param(kv: &str) -> Result<(String, Value), Exit> {
    let (name, text) = kv
        .split_once('=')
        .ok_or_else(|| Exit::Usage(format!("--param 要 `名=值`，给的是 `{kv}`")))?;
    if name.is_empty() {
        return Err(Exit::Usage("--param 的名字为空".to_owned()));
    }
    let v = if text.eq_ignore_ascii_case("NULL") {
        Value::Null
    } else if text.eq_ignore_ascii_case("TRUE") {
        Value::Bool(true)
    } else if text.eq_ignore_ascii_case("FALSE") {
        Value::Bool(false)
    } else if let Some(inner) = text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
        Value::Bytes(inner.as_bytes().to_vec())
    } else {
        match bicdb_types::Number::parse(text) {
            Ok(n) => Value::Number(n),
            Err(e) => {
                return Err(Exit::Usage(format!(
                    "--param {name} 的值 `{text}` 既不是数值也不是 '文本'：{e}"
                )))
            }
        }
    };
    Ok((name.to_owned(), v))
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
            // **EOF 不静默丢语句**：挂着没执行完的东西要说出来。
            if !pending.trim().is_empty() {
                eprintln!(
                    "错误：输入结束，但还有未执行的语句（缺 `;`）：\n  {}",
                    pending.trim()
                );
            }
            return Ok(());
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
        if !ends_statement(&pending) {
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
