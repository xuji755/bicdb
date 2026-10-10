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
use bicdb_cli::proto;
use bicdb_cli::service::{self, ServiceError, StartOptions, StopMode};
use bicdb_exec::Value;
use bicdb_sql::session::{QueryResult, Session, SessionError};

const USAGE: &str = "\
bicdb —— 带撤销/日志的页式数据库（V1.0 单工作区）

用法：
  bicdb init    [<工作区名|根区目录>] [-p 种子参数文件] [-c 键=值]
                                建区（**唯一接受目录的命令**）：建库 + 生成
                                <根区目录>/bicdb.ini（实例参数文件）。
                                **不给参数** ⇒ <BICDB_HOME>/public（初始化部署）；
                                给名字 ⇒ <BICDB_HOME>/<名字>；给路径 ⇒ 原样
  bicdb home                    BICDB_HOME 在哪：程序/数据/日志/备份四条路径
  bicdb list                    <BICDB_HOME> 下的工作区与状态
  bicdb start   [-p 参数文件] [-s 套接字] [-l 日志] [-w 秒] [--force] [-c 键=值]
                                后台起服务（分离进程 + 实例锁 + 控制套接字）
                                --force 仅允许非 PUBLIC 跳过可识别的检查点水位不一致
  bicdb stop    [-p 参数文件] [-m fast|immediate] [-w 秒]  停服务（fast = 完全检查点；
                                -w 等它退出，默认 300 秒）
  bicdb status  [-p 参数文件]    服务/实例状态
  bicdb restart [-p 参数文件]    重启服务
  bicdb params  [-p 参数文件] [-c 键=值]   有效参数表（默认/文件/命令行三来源）
  bicdb recovery verify [-p 参数文件] page <file_id> <block_id>
  bicdb recovery verify [-p 参数文件] object <object_id>
                                实例停止时验证隔离介质并追加同范围 Verified 审计
  bicdb recovery archive [-p 参数文件]
                                停机归档完整恢复审计，并原子保留未验证范围
  bicdb sql     [-p 参数文件] [-U 主体] <SQL>…
                                执行 SQL（服务在跑时经套接字）；`-U` 以某个主体
                                认证（口令取 $BICDB_PASSWORD 或终端提示）
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
            let given = args
                .get(1)
                .filter(|a| !a.starts_with('-'))
                .map(String::as_str);
            let (target, name) = resolve_init_target(given)?;
            let seed = config::parse_ini_arg(&args[1..]);
            let mut overrides =
                config::parse_cli_overrides(&args[1..]).map_err(|e| Exit::Failed(e.to_string()))?;
            // 名字式：**日志统一落 <BICDB_HOME>/log/**（运维不用到处找）；显式
            // 给了 `-c service.log=…` 就听用户的。
            if let Some(name) = &name {
                if let Ok(home) = bicdb_cli::home::Home::locate() {
                    std::fs::create_dir_all(home.log_dir()).map_err(|e| {
                        Exit::Failed(format!("建日志目录 {} 失败：{e}", home.log_dir().display()))
                    })?;
                    std::fs::create_dir_all(home.backup_dir()).map_err(|e| {
                        Exit::Failed(format!(
                            "建备份目录 {} 失败：{e}",
                            home.backup_dir().display()
                        ))
                    })?;
                    if !overrides.iter().any(|(k, _)| k.ends_with("log")) {
                        overrides.push((
                            "service.log".to_owned(),
                            home.workspace_log(name).display().to_string(),
                        ));
                    }
                }
            }
            let params = config::InstanceParams::for_init(&target, seed.as_deref(), &overrides)
                .map_err(|e| Exit::Failed(e.to_string()))?;
            let home = bicdb_cli::home::Home::locate_for_init().ok();
            let mut inst = boot::create_instance(&params, home.as_ref())?;
            inst.shutdown()?;
            println!("已建区：{}", params.db_root.display());
            println!("  参数文件  {}", params.ini_path().display());
            println!(
                "  工作区    ref {}（标识 = SHA-256(工作区号) 前 8 字节）",
                boot::ws_name(inst.ws_ref)
            );
            println!(
                "  日志组    {} 组 × {} 成员 × {} 页",
                params.init.wal_groups, params.init.wal_members, params.init.wal_group_pages
            );
            println!("  日志      {}", params.run.log);
            match (&home, name.as_deref()) {
                (Some(h), _) => {
                    let registered = h.global_ctl_paths().iter().all(|p| p.exists());
                    if registered {
                        println!("  注册表    {}（已登记）", h.control_dir().display());
                    }
                }
                (None, _) => {
                    println!(
                        "  注册表    （无 BICDB_HOME：未登记进实例注册表，`bicdb list` 看不到它）"
                    );
                }
            }
            println!(
                "  下一步    bicdb start -p {}",
                name.as_deref().unwrap_or("public")
            );
            Ok(())
        }
        "start" => {
            let opts = service_opts(&args[1..])?;
            service::start(&opts)?;
            Ok(())
        }
        "recovery" => {
            if args.get(1).map(String::as_str) == Some("archive") {
                let opts = service_opts(&args[2..])?;
                let (path, active) =
                    bicdb_cli::recovery_admin::archive(&opts.params).map_err(Exit::Failed)?;
                println!(
                    "恢复审计已归档：{}（当前审计保留 {} 个未验证范围）",
                    path.display(),
                    active
                );
                return Ok(());
            }
            if args.get(1).map(String::as_str) != Some("verify") {
                return Err(Exit::Usage(
                    "recovery 支持 verify page|object 或 archive".into(),
                ));
            }
            let opts = service_opts(&args[2..])?;
            let (index, kind) = args[2..]
                .iter()
                .enumerate()
                .find(|(_, value)| matches!(value.as_str(), "page" | "object"))
                .ok_or_else(|| Exit::Usage("recovery verify 缺少 page|object 范围".into()))?;
            let index = index + 2;
            let number = |at: usize, what: &str| -> Result<u64, Exit> {
                args.get(at)
                    .ok_or_else(|| Exit::Usage(format!("缺少{what}")))?
                    .parse::<u64>()
                    .map_err(|_| Exit::Usage(format!("{what}必须是无符号整数")))
            };
            let scope = if kind == "page" {
                let file_id = number(index + 1, "文件号")?;
                let block_id = number(index + 2, "块号")?;
                bicdb_cli::recovery_admin::VerifyScope::Page {
                    file_id: u16::try_from(file_id)
                        .map_err(|_| Exit::Usage("文件号超出 u16 范围".into()))?,
                    block_id: u32::try_from(block_id)
                        .map_err(|_| Exit::Usage("块号超出 u32 范围".into()))?,
                }
            } else {
                let object_id = number(index + 1, "对象号")?;
                bicdb_cli::recovery_admin::VerifyScope::Object {
                    object_id: u32::try_from(object_id)
                        .map_err(|_| Exit::Usage("对象号超出 u32 范围".into()))?,
                }
            };
            let detail =
                bicdb_cli::recovery_admin::verify(&opts.params, scope).map_err(Exit::Failed)?;
            println!("恢复范围验证完成：{detail}");
            Ok(())
        }
        "stop" => {
            let opts = service_opts_for(&args[1..], true)?;
            let mode = flag_value(&args[1..], &["-m", "--mode"])
                .map(|v| {
                    StopMode::parse(&v).ok_or_else(|| Exit::Usage(format!("停止模式 `{v}` 不认识")))
                })
                .transpose()?
                .unwrap_or(StopMode::Fast);
            // **`stop` 的等待上限**：默认取 `service.stop_wait_s`（完全检查点在
            // 大库上要几分钟），`-w` 覆盖。
            service::stop(&opts.dir, mode, opts.timeout, &opts.params)?;
            Ok(())
        }
        "home" => {
            // **运维只记这一条命令**：程序/数据/日志/备份四件事的实际路径一次打印。
            match bicdb_cli::home::Home::locate() {
                Ok(home) => {
                    println!("BICDB_HOME = {}", home.root.display());
                    println!("  来源    {}", home.source.as_str());
                    println!(
                        "  程序    {}（版本 {}）",
                        home.app_dir().display(),
                        home.version().unwrap_or_else(|| "未写 VERSION".to_owned())
                    );
                    println!("  数据    {}", home.public_dir().display());
                    println!("  日志    {}", home.log_dir().display());
                    println!("  备份    {}", home.backup_dir().display());
                    println!(
                        "  注册表  {}（全局控制文件双副本）",
                        home.control_dir().display()
                    );
                    println!("  （工作区一览：`bicdb list`；有效参数：`bicdb params -p public`）");
                }
                Err(e) => {
                    println!("{e}");
                    println!("  提示    装到系统里：`scripts/install.sh [--home <目录>]`；");
                    println!(
                        "          或 `export {}=<目录>`；路径式用法不受影响（`bicdb init <目录>`）。",
                        bicdb_cli::home::ENV_HOME
                    );
                }
            }
            Ok(())
        }
        "list" => {
            // `<BICDB_HOME>/` 下的工作区一览（`public` + 额外的；Oracle `oratab` /
            // PG `pg_lsclusters` 的同位物）。
            let home = bicdb_cli::home::Home::locate().map_err(|e| Exit::Failed(e.to_string()))?;
            let list = home
                .workspaces()
                .map_err(|e| Exit::Failed(format!("读 {} 失败：{e}", home.root.display())))?;
            if list.is_empty() {
                println!(
                    "（{} 下还没有工作区——`bicdb init` 建 PUBLIC）",
                    home.root.display()
                );
                return Ok(());
            }
            // **注册表**（全局控制文件）：登记的才算"实例里的工作区"。
            // 扫描只看文件面——两者不一致时如实标出来（崩在这三步中间是已知形态）。
            let registered: Vec<(std::path::PathBuf, u64, u8)> = match boot::open_global_ctl(&home)
            {
                Ok(gcf) => {
                    let recs = gcf.workspaces().unwrap_or_default();
                    let out = recs
                        .iter()
                        .map(|r| {
                            (
                                std::path::PathBuf::from(
                                    String::from_utf8_lossy(&r.root).into_owned(),
                                ),
                                r.workspace_id.as_raw(),
                                r.status,
                            )
                        })
                        .collect();
                    let _ = gcf.close();
                    out
                }
                Err(e) => {
                    println!("（注册表读不了：{e}）");
                    Vec::new()
                }
            };
            println!(
                "{:<20} {:<10} {:<8} {:<10} 工作区目录",
                "工作区", "状态", "工作区号", "注册"
            );
            println!("{:-<20} {:-<10} {:-<8} {:-<10} {:-<40}", "", "", "", "", "");
            for (name, dir) in &list {
                let state = match service::state_of(dir) {
                    service::ServiceState::Serving(_) => "运行中",
                    service::ServiceState::Direct(_) => "直连中",
                    service::ServiceState::Stale(_) => "陈旧锁",
                    service::ServiceState::NotRunning => "已停",
                };
                let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
                let hit = registered.iter().find(|(root, _, _)| {
                    let r = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
                    r == canon
                });
                match hit {
                    Some((_, id, _)) => {
                        println!(
                            "{:<20} {:<10} {:<8} {:<10} {}",
                            name,
                            state,
                            id,
                            "已登记",
                            dir.display()
                        )
                    }
                    None => println!(
                        "{:<20} {:<10} {:<8} {:<10} {}",
                        name,
                        state,
                        "—",
                        "未登记",
                        dir.display()
                    ),
                }
            }
            // 登记了但目录不在（删目录没摘登记 / 搬走了）：**如实报**，不静默。
            let listed: Vec<std::path::PathBuf> = list.iter().map(|(_, d)| d.clone()).collect();
            for (root, id, _) in &registered {
                let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
                if !listed
                    .iter()
                    .any(|d| std::fs::canonicalize(d).unwrap_or_else(|_| d.clone()) == canon)
                    && !canon.join("bicdb.ini").is_file()
                {
                    println!(
                        "{:<20} {:<10} {:<8} {:<10} {}（登记了但目录不在——`bicdb init` 重建或手工摘除）",
                        "(缺失)",
                        "—",
                        id,
                        "已登记",
                        root.display()
                    );
                }
            }
            Ok(())
        }
        "params" => {
            // 有效参数表：**全部可调项**（含未显式给出的 ⇒ "默认"）。
            let ini = config::parse_ini_arg(&args[1..]);
            let overrides =
                config::parse_cli_overrides(&args[1..]).map_err(|e| Exit::Failed(e.to_string()))?;
            let (p, table) = boot::instance_params(ini.as_deref(), &overrides)?;
            println!("实例参数（{}）", p.ini_path().display());
            println!("{:<32} {:<26} {:<8} 类别", "节.键", "取值", "来源");
            println!("{:-<32} {:-<26} {:-<8} {:-<10}", "", "", "", "");
            // 清单与说明都来自 `config::SPECS`（参数的唯一声明表）——
            // 这里不再各写一份 match（此前加一个键要改五处，漏一处就出现
            // "文件里能写但没人读"）。
            for spec in config::InstanceParams::specs() {
                let (section, key) = (spec.section, spec.key);
                let shown = if spec.effect == config::Effect::Creation {
                    // 建区期参数只回显当前实例的事实，不给"可改"的错觉。
                    p.init_fact(key)
                } else {
                    overrides
                        .iter()
                        .rev()
                        .find(|(k, _)| k == &format!("{section}.{key}") || k == key)
                        .map(|(_, v)| (v.clone(), config::Source::Cli))
                        .or_else(|| {
                            table
                                .iter()
                                .rev()
                                .find(|(k, _, _)| k == &format!("{section}.{key}"))
                                .map(|(_, v, src)| (v.clone(), *src))
                        })
                        .unwrap_or_else(|| {
                            (
                                p.value_of(section, key).unwrap_or_default(),
                                config::Source::Default,
                            )
                        })
                };
                let (value, source) = shown;
                let class = spec.effect.as_str();
                println!(
                    "{:<34} {:<14} {:<8} {class}  {}",
                    format!("{section}.{key}"),
                    value,
                    source.as_str(),
                    spec.doc
                );
            }
            println!("  建区期参数由**控制文件**记录（权威）——`bicdb start` 会逐项核对，");
            println!("  不符即拒绝启动（改需重建实例）；运行期参数改完 `bicdb restart` 生效。");
            Ok(())
        }
        "status" => {
            let ini = config::parse_ini_arg(&args[1..]);
            let (params, _) = boot::instance_params(ini.as_deref(), &[])?;
            service::status(&params.db_root, &params)?;
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
            // **认证凭据**（`-U <主体>`；口令从 `$BICDB_PASSWORD` 或终端提示）。
            let creds = bicdb_cli::clientauth::from_args(&args[1..]).map_err(Exit::Usage)?;
            let (inst_params, _) = boot::instance_params(ini.as_deref(), &overrides)?;
            // **服务在跑 ⇒ 走套接字**（同一条 SQL 路径，事务语义一致）。
            if let service::ServiceState::Serving(info) = service::state_of(&inst_params.db_root) {
                let text = sql_text(&sql_args, params.is_empty())?;
                let named: Vec<(&str, Value)> = params
                    .iter()
                    .map(|(n, v)| (n.as_str(), v.clone()))
                    .collect();
                return run_over_socket(
                    &info.socket,
                    &text,
                    &named,
                    &inst_params.run,
                    creds.as_ref(),
                );
            }
            // 直连形态没有"认证"这一步（本机 = OS 身份）——给了 `-U` 要**明说**，
            // 不能悄悄忽略（那会让人以为"以 alice 的身份"跑了语句）。
            if let Some(c) = &creds {
                return Err(Exit::Usage(format!(
                    "`-U {}` 只对**经服务**的连接有效——现在服务没在跑（直连形态）：\
                     本机访问按控制套接字的文件权限（OS 身份）判定",
                    c.user
                )));
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
    session.set_fulltext_defaults(
        inst.params.run.fulltext_interval_ms,
        inst.params.run.fulltext_batch_rows,
    )?;
    session.set_graph_limits(inst.params.run.graph_limits())?;
    attach_dcl(inst.io, inst.params.run.pbkdf2_iterations, &mut session);
    session.set_fixed_table_source(Some(bicdb_cli::fixed::CliFixedTables::new_static(
        &inst.dir, inst.io,
    )));
    f(&mut session)
}

/// **给会话接上管理面上下文**（DCL 的执行落点：`BICDB_HOME` + 全局控制文件的 I/O）。
///
/// 没装（`Home::locate` 失败）也能跑——DCL 会以"要一个部署根"具名拒绝，不猜位置。
fn attach_dcl(
    io: &'static dyn bicdb_workspace::io::FileIo,
    iterations: u32,
    session: &mut Session<'_, '_, '_, '_>,
) {
    let home = bicdb_cli::home::Home::locate().ok().map(|h| h.root);
    session.set_dcl_context(home, Some(io));
    session.set_workspace_provisioner(Some(cli_provisioner()));
    session.set_pbkdf2_iterations(iterations);
}

/// 供给方（无状态）——`'static` 一份就够。
fn cli_provisioner() -> &'static bicdb_cli::provision::CliProvisioner {
    bicdb_cli::provision::CliProvisioner::new_static()
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
/// **`init` 的目标**：名字（不含 `/`）⇒ `<BICDB_HOME>/data/<名字>`；含 `/` 的路径原样。
///
/// 名字式要求**能定出安装根**（`BICDB_HOME` 或二进制位置推导）；定不出就报具名错误
/// ——不偷偷建到当前目录里（那会让"实例在哪儿"变得不可预期）。
/// **`init` 的目标**：
/// - 不给参数 ⇒ `<BICDB_HOME>/public`（**初始化部署**，最常用）
/// - 给名字（不含 `/`、不是保留目录名） ⇒ `<BICDB_HOME>/<名字>`（额外工作区，测试/演练）
/// - 给路径 ⇒ 原样（安装形态**不约束**显式路径）
///
/// 返回 `(工作区根, 名字)`；日志默认落 `<BICDB_HOME>/log/<名字>.log`（名字式才有）。
fn resolve_init_target(given: Option<&str>) -> Result<(std::path::PathBuf, Option<String>), Exit> {
    // 不用 `Option::is_none_or`（stable 1.82；本仓 MSRV 1.80）。
    let name_style = match given {
        None => true,
        Some(g) => !g.is_empty() && !g.contains('/'),
    };
    if !name_style {
        let g = given.expect("上面已判非空");
        return Ok((std::path::PathBuf::from(g), None));
    }
    match bicdb_cli::home::Home::locate_for_init() {
        Ok(home) => {
            let name = given.unwrap_or(bicdb_cli::home::PUBLIC);
            let dir = home
                .workspace_dir(name)
                .unwrap_or_else(|| home.public_dir());
            if dir == home.app_dir() || dir == home.log_dir() || dir == home.backup_dir() {
                return Err(Exit::Usage(format!(
                    "`{name}` 是保留目录名（app/log/backup）——换个名字"
                )));
            }
            Ok((dir, Some(name.to_owned())))
        }
        Err(e) => Err(Exit::Usage(format!(
            "名字式寻址需要先确定 BICDB_HOME：{e}\n  \
             ——或者给路径：`bicdb init ./{}`",
            given.unwrap_or("myws")
        ))),
    }
}

fn service_opts(args: &[String]) -> Result<StartOptions, Exit> {
    service_opts_for(args, false)
}

/// `stop` 形态：等待上限的默认取 `service.stop_wait_s`（其余同 [`service_opts`]）。
fn service_opts_for(args: &[String], stop_side: bool) -> Result<StartOptions, Exit> {
    let ini = config::parse_ini_arg(args);
    let socket = flag_value(args, &["-s", "--socket"]).map(std::path::PathBuf::from);
    let log = flag_value(args, &["-l", "--log"]).map(std::path::PathBuf::from);
    // `-w` 给了就用它；没给则用参数文件里的 `service.start_wait_s`
    // （`stop` 分支用 `stop_wait_s`——见 `service_opts_for`）。
    let wait_s = flag_value(args, &["-w", "--wait"])
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| Exit::Usage(format!("-w 要秒数，给的是 `{v}`")))
        })
        .transpose()?;
    let overrides = config::parse_cli_overrides(args).map_err(|e| Exit::Failed(e.to_string()))?;
    let mut opts =
        StartOptions::load_with_wait(ini.as_deref(), socket, log, wait_s, stop_side, overrides)?;
    opts.force = flag_present(args, "--force");
    Ok(opts)
}

/// **经服务执行**（服务在跑时的 `sql`/`shell` 走这条）：打印与直连同形。
fn run_over_socket(
    socket: &std::path::Path,
    sql: &str,
    params: &[(&str, Value)],
    run: &config::RunParams,
    creds: Option<&bicdb_cli::clientauth::Credentials>,
) -> Result<(), Exit> {
    // **握手超时**（`client.handshake_timeout_ms`）：实例忙时据此报"正忙"
    // 而不是无声挂住。
    let mut client = bicdb_net::Client::connect_with_timeout(
        socket,
        std::time::Duration::from_millis(run.handshake_timeout_ms),
    )
    .map_err(|e| Exit::Failed(format!("经服务执行失败：{e}")))?;
    // **请求超时**（`client.request_timeout_ms`；0 = 不限——长查询是正常的，
    // 但"服务卡住"需要一个能配的兜底）。
    if run.request_timeout_ms > 0 {
        client
            .set_timeout(std::time::Duration::from_millis(run.request_timeout_ms))
            .map_err(|e| Exit::Failed(format!("设置请求超时失败：{e}")))?;
    }
    // **认证**（给了 `-U` 才做）：连上之后、第一条语句之前（服务的准入规则）。
    if let Some(c) = creds {
        // 服务端文案**原文透传**（它自带 `认证失败：…` 前缀——再加一层就是重复）。
        let id = client
            .auth(&c.user, &c.password)
            .map_err(|e| Exit::Failed(e.to_string()))?;
        if id.expired {
            eprintln!(
                "注意：主体 `{}` 的口令已过期（`EXPIRE`）⇒ **受限会话**：\
                 除本人改密（`ALTER USER … IDENTIFIED BY '<新>' REPLACE '<旧>'`）之外一律拒绝",
                id.user
            );
        }
    }
    let sent = proto::wire_params(params);
    let statements = client
        .sql(sql, &sent)
        .map_err(|e| Exit::Failed(format!("经服务执行失败：{e}")))?;
    let results = proto::results(&statements);
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
            "-p" | "--ini" | "--params-file" | "-c" | "--set" | "-U" | "--user"
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
            // 呈现：**列形态**决定右对齐（数值列），值由 `format_value` 格式化。
            let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
            let cells: Vec<Vec<String>> = rows
                .iter()
                .map(|r| r.iter().map(bicdb_sql::session::format_value).collect())
                .collect();
            print_table_with_kinds(out, &names, &cells, columns).map_err(io)?;
        }
        QueryResult::Affected(n) => writeln!(out, "影响 {n} 行").map_err(io)?,
        QueryResult::Ddl(s) => writeln!(out, "{s}").map_err(io)?,
        QueryResult::Txn(s) => writeln!(out, "{s}").map_err(io)?,
    }
    Ok(())
}

/// 表格（列宽按内容取，`NULL` 显式写出）。
fn print_table_with_kinds(
    out: &mut impl Write,
    columns: &[String],
    rows: &[Vec<String>],
    meta: &[bicdb_sql::session::ColumnMeta],
) -> std::io::Result<()> {
    // 数值列右对齐（形态为 NUMBER 的列）——与 SQL*Plus 的口径一致。
    let right: Vec<bool> = columns
        .iter()
        .enumerate()
        .map(|(i, _)| {
            meta.get(i)
                .is_some_and(|m| m.kind == bicdb_exec::ColKind::Number)
        })
        .collect();
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
            if right[i] {
                write!(out, "{cell:>width$}", width = width[i])?;
            } else {
                write!(out, "{cell:<width$}", width = width[i])?;
            }
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
    session.set_fulltext_defaults(
        inst.params.run.fulltext_interval_ms,
        inst.params.run.fulltext_batch_rows,
    )?;
    session.set_graph_limits(inst.params.run.graph_limits())?;
    attach_dcl(inst.io, inst.params.run.pbkdf2_iterations, &mut session);
    session.set_fixed_table_source(Some(bicdb_cli::fixed::CliFixedTables::new_static(
        &inst.dir, inst.io,
    )));
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
