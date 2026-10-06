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
use bicdb_cli::config;
use bicdb_cli::service::{self, ServiceError, StartOptions, StopMode};
use bicdb_cli::wire;
use bicdb_exec::Value;
use bicdb_sql::session::{QueryResult, Session, SessionError};

const USAGE: &str = "\
bicdb —— 带撤销/日志的页式数据库（V1.0 单工作区）

用法：
  bicdb init    <根区目录> [-p 种子参数文件] [-c 键=值]
                                建区（**唯一接受目录的命令**）：建库 + 生成
                                <根区目录>/bicdb.ini（实例参数文件）
  bicdb start   [-p 参数文件] [-s 套接字] [-l 日志] [-w 秒] [-c 键=值]
                                后台起服务（分离进程 + 实例锁 + 控制套接字）
  bicdb stop    [-p 参数文件] [-m fast|immediate]    停服务（fast = 完全检查点）
  bicdb status  [-p 参数文件]    服务/实例状态
  bicdb restart [-p 参数文件]    重启服务
  bicdb params  [-p 参数文件] [-c 键=值]   有效参数表（默认/文件/命令行三来源）
  bicdb sql     [-p 参数文件] <SQL>…       执行 SQL（服务在跑时经套接字）
  bicdb shell   [-p 参数文件]              交互式 shell
  bicdb version | help

**实例寻址（照 Oracle 的口径：不指向某个目录，指向参数文件）**：
  `-p <参数文件|根区目录>` > 环境变量 BICDB_INI > 当前目录的 bicdb.ini
  根区目录**注册在参数文件里**（`[instance] db_root`）——本文件即权威。

示例：
  bicdb init  /data/bicdb                      # 建区（并在其下生成 bicdb.ini）
  bicdb start -p /data/bicdb                   # 起服务（也可 `-p /data/bicdb/bicdb.ini`）
  bicdb sql   -p /data/bicdb \"SELECT * FROM t\"
  bicdb params -p /data/bicdb                  # 看有效参数与来源
  bicdb stop  -p /data/bicdb

SQL*Plus 形态的客户端见 `bicdbcli`（缓冲/斜杠命令/SPOOL/@脚本）。
";

fn main() -> ExitCode {
    // **下游提前关闭**（`… | head`）⇒ 静默收工，别 panic（Broken pipe）。
    std::panic::set_hook(Box::new(|info| {
        if info.to_string().contains("Broken pipe") {
            std::process::exit(0);
        }
        eprintln!("{info}");
    }));
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

impl From<ServiceError> for Exit {
    fn from(e: ServiceError) -> Self {
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
            let root = args
                .get(1)
                .filter(|a| !a.starts_with('-'))
                .ok_or_else(|| Exit::Usage("init 缺根区目录".to_owned()))?;
            let seed = config::parse_ini_arg(&args[1..]);
            let overrides =
                config::parse_cli_overrides(&args[1..]).map_err(|e| Exit::Failed(e.to_string()))?;
            let params = config::InstanceParams::for_init(
                std::path::Path::new(root),
                seed.as_deref(),
                &overrides,
            )
            .map_err(|e| Exit::Failed(e.to_string()))?;
            let mut inst = boot::create_instance(&params)?;
            inst.shutdown()?;
            println!("已建区：{}", params.db_root.display());
            println!("  参数文件  {}", params.ini_path().display());
            println!("  工作区    {}", String::from_utf8_lossy(&boot::WS));
            println!(
                "  日志组    {} 组 × {} 成员 × {} 页",
                params.init.wal_groups, params.init.wal_members, params.init.wal_group_pages
            );
            println!("  下一步    bicdb start -p {}", params.db_root.display());
            Ok(())
        }
        "start" => {
            let opts = service_opts(&args[1..])?;
            service::start(&opts)?;
            Ok(())
        }
        "stop" => {
            let opts = service_opts(&args[1..])?;
            let mode = flag_value(&args[1..], &["-m", "--mode"])
                .map(|v| {
                    StopMode::parse(&v).ok_or_else(|| Exit::Usage(format!("停止模式 `{v}` 不认识")))
                })
                .transpose()?
                .unwrap_or(StopMode::Fast);
            service::stop(&opts.dir, mode)?;
            Ok(())
        }
        "params" => {
            // 有效参数表：**全部可调项**（含未显式给出的 ⇒ "默认"）。
            let ini = config::parse_ini_arg(&args[1..]);
            let overrides =
                config::parse_cli_overrides(&args[1..]).map_err(|e| Exit::Failed(e.to_string()))?;
            let (p, table) = boot::instance_params(ini.as_deref(), &overrides)?;
            let value_of = |section: &str, key: &str| -> String {
                let full = format!("{section}.{key}");
                match (section, key) {
                    ("instance", "db_root") => p.db_root.display().to_string(),
                    ("init", "file0_initial_blocks") => p.init.file0_initial_blocks.to_string(),
                    ("init", "undo_initial_blocks") => p.init.undo_initial_blocks.to_string(),
                    ("init", "wal_groups") => p.init.wal_groups.to_string(),
                    ("init", "wal_members") => p.init.wal_members.to_string(),
                    ("init", "wal_group_pages") => p.init.wal_group_pages.to_string(),
                    ("buffer", "pool_frames") => p.run.pool_frames.to_string(),
                    ("storage", "file_extend_blocks") => p.run.file_extend_blocks.to_string(),
                    ("service", "socket") => p.run.socket.clone(),
                    ("service", "log") => p.run.log.clone(),
                    ("lock", "park_ms") => p.run.park_ms.to_string(),
                    ("lock", "deadlock_threshold_ms") => p.run.deadlock_threshold_ms.to_string(),
                    _ => table
                        .iter()
                        .find(|(tk, _, _)| *tk == full)
                        .map_or_else(String::new, |(_, v, _)| v.clone()),
                }
            };
            println!("实例参数（{}）", p.ini_path().display());
            println!("{:<32} {:<26} {:<8} 类别", "节.键", "取值", "来源");
            println!("{:-<32} {:-<26} {:-<8} {:-<10}", "", "", "", "");
            for (section, key) in config::InstanceParams::all_keys() {
                let full = format!("{section}.{key}");
                let src = table
                    .iter()
                    .find(|(tk, _, _)| *tk == full)
                    .map_or(config::Source::Default, |(_, _, s)| *s);
                let kind = if section == "init" {
                    "建区期"
                } else {
                    "运行期"
                };
                println!(
                    "{full:<32} {:<26} {:<8} {kind}",
                    value_of(section, key),
                    src.as_str()
                );
            }
            println!();
            println!("说明：");
            println!("  建区期参数由**控制文件**记录（权威）——`bicdb start` 会逐项核对，");
            println!("  不符即拒绝启动（改需重建实例）；运行期参数改完 `bicdb restart` 生效。");
            Ok(())
        }
        "status" => {
            let ini = config::parse_ini_arg(&args[1..]);
            let (params, _) = boot::instance_params(ini.as_deref(), &[])?;
            service::status(&params.db_root)?;
            Ok(())
        }
        "restart" => {
            let opts = service_opts(&args[1..])?;
            service::restart(&opts)?;
            Ok(())
        }
        // 内部：守护进程入口（由 `start` 拉起；不写进帮助）。
        "__daemon" => {
            let opts = service_opts(&args[1..])?;
            service::run_daemon(&opts, flag_present(&args[1..], "--foreground"))?;
            Ok(())
        }
        "sql" => {
            let ini = config::parse_ini_arg(&args[1..]);
            let overrides =
                config::parse_cli_overrides(&args[1..]).map_err(|e| Exit::Failed(e.to_string()))?;
            let (params, sql_args) = split_params(&args[1..])?;
            let (inst_params, _) = boot::instance_params(ini.as_deref(), &overrides)?;
            // **服务在跑 ⇒ 走套接字**（同一条 SQL 路径，事务语义一致）。
            if let service::ServiceState::Serving(info) = service::state_of(&inst_params.db_root) {
                let text = sql_text(&sql_args, params.is_empty())?;
                let named: Vec<(&str, Value)> = params
                    .iter()
                    .map(|(n, v)| (n.as_str(), v.clone()))
                    .collect();
                return run_over_socket(&info.socket, &text, &named);
            }
            let mut inst = boot::open_instance(&inst_params)?;
            banner_brief(&inst);
            let text = sql_text(&sql_args, params.is_empty())?;
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
            let ini = config::parse_ini_arg(&args[1..]);
            let overrides =
                config::parse_cli_overrides(&args[1..]).map_err(|e| Exit::Failed(e.to_string()))?;
            let (inst_params, _) = boot::instance_params(ini.as_deref(), &overrides)?;
            let mut inst = boot::open_instance(&inst_params)?;
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

/// 从实参里取 `--flag value`（`names` 里的任一写法）。
fn flag_value(args: &[String], names: &[&str]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if names.contains(&a.as_str()) {
            return it.next().cloned();
        }
    }
    None
}

/// 实参里有没有 `--flag`。
fn flag_present(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// **服务类子命令的参数**：`[-p 参数文件] [-s 套接字] [-l 日志] [-w 秒] [-c 键=值]`。
///
/// **不指向目录**（除 `init`）：实例在哪儿由参数文件注册（照 Oracle 的口径）。
fn service_opts(args: &[String]) -> Result<StartOptions, Exit> {
    let ini = config::parse_ini_arg(args);
    let socket = flag_value(args, &["-s", "--socket"]).map(std::path::PathBuf::from);
    let log = flag_value(args, &["-l", "--log"]).map(std::path::PathBuf::from);
    let timeout = flag_value(args, &["-w", "--wait"])
        .map(|v| {
            v.parse::<u64>()
                .map(std::time::Duration::from_secs)
                .map_err(|_| Exit::Usage(format!("-w 要秒数，给的是 `{v}`")))
        })
        .transpose()?
        .unwrap_or(std::time::Duration::from_secs(30));
    let overrides = config::parse_cli_overrides(args).map_err(|e| Exit::Failed(e.to_string()))?;
    Ok(StartOptions::load(
        ini.as_deref(),
        socket,
        log,
        timeout,
        overrides,
    )?)
}

/// **经服务执行**（服务在跑时的 `sql`/`shell` 走这条）：打印与直连同形。
fn run_over_socket(
    socket: &std::path::Path,
    sql: &str,
    params: &[(&str, Value)],
) -> Result<(), Exit> {
    let payload = wire::encode_sql_request(sql, params);
    let body = wire::call(socket, "SQL", &payload)
        .map_err(|e| Exit::Failed(format!("经服务执行失败：{e}")))?;
    let results = wire::decode_results(&body);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for r in &results {
        print_result(&mut out, r)?;
    }
    out.flush()
        .map_err(|e| Exit::Failed(format!("写输出失败：{e}")))?;
    Ok(())
}

/// `--param` 解析结果：`(参数表, 其余实参 = SQL 文本)`。
type ParamsAndSql = (Vec<(String, Value)>, Vec<String>);

/// **切出 `--param 名=值`**（其余是 SQL 文本）。
fn split_params(args: &[String]) -> Result<ParamsAndSql, Exit> {
    let mut params = Vec::new();
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--param" || a == "--sql-param" {
            let kv = it
                .next()
                .ok_or_else(|| Exit::Usage("--param 缺 `名=值`".to_owned()))?;
            params.push(parse_param(kv)?);
        } else if matches!(
            a.as_str(),
            "-p" | "--ini" | "--params-file" | "-c" | "--set"
        ) {
            let _ = it.next(); // 这两个旗标的值不是 SQL 文本
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
fn sql_text(rest: &[String], _any: bool) -> Result<String, Exit> {
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
