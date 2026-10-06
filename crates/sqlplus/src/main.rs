//! **bicdbcli** —— bicdb 的 SQL\*Plus 形态客户端（独立命令行工具）。
//!
//! ```text
//! bicdbcli [选项] <实例目录> [@脚本]
//!   -S              静默（不出横幅）
//!   -s <套接字>     指定控制套接字（默认 <实例目录>/bicdb.sock）
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
    dir: PathBuf,
    script: Option<PathBuf>,
    silent: bool,
    direct: bool,
}

fn run(args: &[String]) -> Result<i32, String> {
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "-?" | "-h" | "--help"))
    {
        print!("{USAGE}");
        return Ok(0);
    }
    let mut dir: Option<PathBuf> = None;
    let mut script: Option<PathBuf> = None;
    let mut silent = false;
    let mut direct = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-S" => silent = true,
            "--direct" => direct = true,
            "-s" | "--socket" => {
                let _ = it.next().ok_or("-s 缺套接字路径")?; // 套接字由实例状态给出；显式覆盖留白
                return Err("-s 覆盖套接字暂不支持（套接字由实例目录决定）".to_owned());
            }
            other if other.starts_with('@') => script = Some(PathBuf::from(&other[1..])),
            other if other.starts_with('-') => {
                return Err(format!("不认识的选项 `{other}`（`--help` 看用法）"));
            }
            other => dir = Some(PathBuf::from(other)),
        }
    }
    let opts = Options {
        dir: dir.ok_or("缺实例目录（`bicdbcli <实例目录>`）")?,
        script,
        silent,
        direct,
    };

    // 连接（服务在跑 ⇒ 套接字；否则直连）。
    let conn = conn::Conn::open(&opts.dir, opts.direct).map_err(|e| e.to_string())?;
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
