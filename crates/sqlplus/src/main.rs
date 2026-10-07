//! **bicdbcli** —— bicdb 的 SQL\*Plus 形态客户端（独立命令行工具）。
//!
//! ```text
//! bicdbcli [选项] <实例目录> [@脚本]
//!   -S              静默（不出横幅）
//!   -s <套接字>     指定控制套接字（默认 <实例目录>/bicdb.sock）
//!   -U <主体>       以某个主体认证（口令取 `$BICDB_PASSWORD` 或终端提示；
//!                   **只对经服务的连接有效**——本机直连 = OS 身份）
//!   --direct        强制直连（不经服务）
//!   -?|-h|--help    本帮助
//! ```
//!
//! **连接怎么选**（单写者纪律）：
//! - 服务在跑 ⇒ **经套接字**（一个连接 = 服务端一个会话，`BEGIN … COMMIT` 跨语句成立）；
//! - 没人占 ⇒ **直连**（本进程打开实例，与 `bicdb shell` 同一条路径）；
//! - 别的进程直连着 ⇒ 拒绝（不许两个写者）。
//!
//! **与 `bicdb`（管理命令）的分工**：`bicdb` 管实例生命周期（`init`/`start`/
//! `stop`/`status`）与一次性 `sql`；`bicdbcli` 是**会话式**客户端——缓冲区、
//! 斜杠命令、`SPOOL`、`@` 脚本。SQL\*Plus 的对应见 `HELP`。

#![forbid(unsafe_code)]

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use bicdb_sqlplus::{conn, shell};

const USAGE: &str = "\
bicdbcli —— bicdb 的 SQL*Plus 形态客户端

用法：
  bicdbcli [选项] <实例目录> [@脚本]

选项：
  -S              静默（不出横幅与提示符由 SET SQLPROMPT 控制）
  -s <套接字>      控制套接字（默认 <实例目录>/bicdb.sock）
  -U <主体>       以某个主体认证（口令取 $BICDB_PASSWORD 或终端提示；只对经服务有效）
  --direct        强制直连（服务在跑时会被实例锁挡住）
  -? -h --help    本帮助

会话里常用：
  SELECT … ;      语句以 `;` 结尾执行（续行提示符是行号）
  /               重跑当前缓冲区
  LIST / RUN / DEL / APPEND / CHANGE / CLEAR BUFFER   缓冲编辑
  SET / SHOW      会话参数（ECHO/FEEDBACK/TIMING/PAGESIZE/LINESIZE/NULL/…）
  SPOOL <文件|OFF> 把输出同时写进文件
  @<文件>         跑脚本；DESCRIBE <对象>；HOST !<命令>；EXIT
  HELP            全部命令
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
        Ok(code) => ExitCode::from(code as u8),
        Err(msg) => {
            eprintln!("bicdbcli：{msg}");
            ExitCode::from(2)
        }
    }
}

/// 命令行解析结果。
struct Options {
    script: Option<PathBuf>,
    silent: bool,
    direct: bool,
    /// 显式控制套接字（`-s`；`None` = 按参数文件/实例状态选路）。
    socket: Option<PathBuf>,
}

fn run(args: &[String]) -> Result<i32, String> {
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "-?" | "-h" | "--help"))
    {
        print!("{USAGE}");
        return Ok(0);
    }
    let mut ini: Option<PathBuf> = None;
    let mut script: Option<PathBuf> = None;
    let mut silent = false;
    let mut direct = false;
    let mut socket: Option<PathBuf> = None;
    // **认证凭据**（`-U <主体>` + 口令；`bicdb sql` 与这里共用一份取法）。
    let creds = bicdb_cli::clientauth::from_args(args)?;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-S" => silent = true,
            "--direct" => direct = true,
            "-s" | "--socket" => {
                socket = Some(PathBuf::from(it.next().ok_or("-s 缺套接字路径")?));
            }
            "-p" | "--ini" | "--params-file" => {
                ini = Some(PathBuf::from(it.next().ok_or("-p 缺参数文件路径")?));
            }
            other if other.starts_with('@') => script = Some(PathBuf::from(&other[1..])),
            other if other.starts_with('-') => {
                return Err(format!("不认识的选项 `{other}`（`--help` 看用法）"));
            }
            other => return Err(format!("不认识的参数 `{other}`（`--help` 看用法）")),
        }
    }
    // **实例寻址**：`-p` > `$BICDB_INI` > `./bicdb.ini`（照 Oracle 的口径，
    // 不指向目录——根区目录注册在参数文件里）。
    // **`-s <套接字>` 明确指定了连接目标**：参数文件找不到也不挡路——它这时
    // 只剩两个用途（`[client]` 超时、直连形态的 `db_root`），取默认即可。
    let (params, _) = match bicdb_cli::config::InstanceParams::locate(ini.as_deref()) {
        Ok(v) => v,
        Err(e) if socket.is_some() => {
            let _ = e;
            (bicdb_cli::config::InstanceParams::default(), Vec::new())
        }
        Err(e) => return Err(e.to_string()),
    };
    let opts = Options {
        script,
        silent,
        direct,
        socket,
    };

    // 连接（`-s` 显式套接字 > 服务在跑 ⇒ 套接字 > 直连）。
    let mut conn = conn::Conn::open_with(&params, opts.direct, opts.socket.as_deref())
        .map_err(|e| e.to_string())?;
    // **认证**（给了 `-U` 才做）：连上之后、第一条语句之前（服务的准入规则）。
    if let Some(c) = &creds {
        let id = conn.authenticate(c).map_err(|e| e.to_string())?;
        if !opts.silent {
            if id.expired {
                println!(
                    "已认证：主体 `{}`（{}）—— **口令已过期 ⇒ 受限会话**（只许本人改密：\
                     `ALTER USER {} IDENTIFIED BY '<新口令>' REPLACE '<旧口令>'`）",
                    id.user, id.user_id, id.user
                );
            } else {
                println!("已认证：主体 `{}`（{}）", id.user, id.user_id);
            }
        }
    }
    if !opts.silent {
        println!("bicdbcli —— bicdb {}", env!("CARGO_PKG_VERSION"));
        println!("连接：{}", conn.kind());
        println!("输入 SQL（`;` 结尾执行，`/` 重跑，`HELP` 看命令）");
    }
    // `-S`（SQL*Plus 静默模式）：**不出横幅、不出提示符**，但结果照打。
    let mut sh = shell::Shell::new(conn, opts.script.is_none() && !opts.silent);
    let mut code = 0;
    if let Some(path) = &opts.script {
        sh.set_script_dir(path.parent().map(std::path::Path::to_path_buf));
        code = sh.run_script(path).map_err(|e| e.to_string())?;
    }
    if opts.script.is_none() {
        code = sh.run_interactive().map_err(|e| e.to_string())?;
    }
    // 收尾：直连 ⇒ 完全检查点；经服务 ⇒ 断开（服务继续跑）。
    let _ = std::io::stdout().flush();
    sh.close().map_err(|e| e.to_string())?;
    Ok(code)
}
