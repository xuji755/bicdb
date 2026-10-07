//! **bicdbcli 的会话壳**：缓冲 → 命令分派 → 执行 → 输出。
//!
//! ```text
//! 一行输入
//!   ├─ 缓冲为空 且 是斜杠命令（SET/LIST/@/SPOOL/EXIT…） ⇒ 立即执行命令
//!   ├─ 缓冲为空 且 是 SQL                             ⇒ 进缓冲（替换旧的）
//!   ├─ 缓冲非空                                       ⇒ 续行（行号提示）
//!   │     └─ 以 `;` 结尾 ⇒ 执行（缓冲保留，供 `/` 重跑）
//!   └─ 空行 ⇒ 结束输入（**不执行**；缓冲保留）
//! ```
//!
//! 命令集与 SQL*Plus 的对应见 `HELP`（`help.rs`）。**取法**：只收有落点的
//! 命令——收下不做的命令就是空壳（本项目审计口径）。

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::buffer::{parse_change, parse_range, SqlBuffer};
use crate::conn::{Conn, ConnError};
use crate::help;
use crate::output::{format_elapsed, Output};
use crate::settings::{SetOutcome, Settings};

/// 一行的处理结果。
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    /// 继续。
    Continue,
    /// 退出（带退出码）。
    Quit(i32),
}

/// **会话壳**。
pub struct Shell {
    conn: Conn,
    buf: SqlBuffer,
    /// 正在续行输入（缓冲里已有未完成的行）。
    filling: bool,
    /// 缓冲里的**当前行**（`LIST` 的 `*` 与 `APPEND`/`INPUT` 的落点）。
    current: usize,
    settings: Settings,
    out: Output,
    /// 替换变量（`DEFINE` / `&&name` 记住的那些）。
    defines: Vec<(String, String)>,
    /// 交互模式（决定提示符与 TERMOUT 的行为）。
    interactive: bool,
    /// 当前脚本目录（`@@` 相对它解析）。
    script_dir: Option<PathBuf>,
    /// **脚本嵌套深度**（`@`/`@@`/`START` 自引用会无限递归 ⇒ 栈溢出——
    /// 这里按上限具名拒绝，与仓内"环即报错"的口径一致）。
    script_depth: usize,
}

/// 脚本嵌套深度上限（自引用脚本报错而不是栈溢出）。
pub const MAX_SCRIPT_DEPTH: usize = 32;

impl Shell {
    /// 建壳（连接已就绪）。
    #[must_use]
    pub fn new(conn: Conn, interactive: bool) -> Self {
        Self {
            conn,
            buf: SqlBuffer::new(),
            filling: false,
            current: 0,
            settings: Settings::default(),
            out: Output::new(),
            defines: Vec::new(),
            interactive,
            script_dir: None,
            script_depth: 0,
        }
    }

    /// 收尾（直连 ⇒ 完全检查点）。
    pub fn close(&mut self) -> Result<(), ConnError> {
        self.out.spool_off();
        self.conn.close()
    }

    /// 连接（诊断）。
    #[must_use]
    pub fn conn(&self) -> &Conn {
        &self.conn
    }

    /// 设置（测试与横幅读）。
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// **跑一个脚本文件**（`@file` / `START file` / 命令行 `@file`）。
    pub fn run_script(&mut self, path: &Path) -> Result<i32, ConnError> {
        if self.script_depth >= MAX_SCRIPT_DEPTH {
            return Err(ConnError::State(format!(
                "脚本嵌套超过 {MAX_SCRIPT_DEPTH} 层（{}）——自引用的脚本会无限递归",
                path.display()
            )));
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConnError::State(format!("读脚本 {}：{e}", path.display())))?;
        let saved = self.interactive;
        self.interactive = false; // 脚本里不出提示符
        self.script_depth += 1;
        let mut code = 0;
        for line in text.lines() {
            match self.handle_line(line) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Quit(c)) => {
                    code = c;
                    break;
                }
                Err(e) => {
                    // **脚本中断**（`WHENEVER SQLERROR EXIT` 生效或连接坏了）：
                    // 退出码 1——SQL*Plus 同一位置也是"脚本失败即会话失败"。
                    self.out.line(true, &format!("!  {e}"));
                    code = 1;
                    break;
                }
            }
        }
        self.interactive = saved;
        self.script_depth -= 1;
        Ok(code)
    }

    /// **交互主循环**（读到 EOF 为止）。
    pub fn run_interactive(&mut self) -> Result<i32, ConnError> {
        let stdin = std::io::stdin();
        let mut code = 0;
        loop {
            self.prompt();
            let mut line = String::new();
            let n = stdin
                .lock()
                .read_line(&mut line)
                .map_err(|e| ConnError::State(format!("读输入：{e}")))?;
            if n == 0 {
                // EOF：挂着的输入要说出来（不静默丢）。
                if self.filling && !self.buf.is_empty() {
                    eprintln!("!  输入结束，缓冲里还有未执行的语句（缺 `;`）——`/` 可重跑");
                }
                break;
            }
            let line = line.trim_end_matches(['\n', '\r']);
            match self.handle_line(line)? {
                Flow::Continue => {}
                Flow::Quit(c) => {
                    code = c;
                    break;
                }
            }
        }
        Ok(code)
    }

    fn prompt(&self) {
        if !self.interactive {
            return;
        }
        if self.filling {
            print!("{:>3}  ", self.buf.len() + 1);
        } else {
            print!("{}", self.settings.sqlprompt);
        }
        let _ = std::io::stdout().flush();
    }

    /// **处理一行**（缓冲/命令的统一入口）。
    fn handle_line(&mut self, raw: &str) -> Result<Flow, ConnError> {
        let line = self.substitute(raw);
        // 1) 续行中：只可能是 SQL 的下一行。
        if self.filling {
            if line.trim().is_empty() {
                self.filling = false; // 空行 = 结束输入，不执行
                return Ok(Flow::Continue);
            }
            self.buf.push(line.clone());
            self.current = self.buf.len();
            if SqlBuffer::ends_with_semicolon(&self.buf.text()) {
                self.filling = false;
                self.execute_buffer()?;
            }
            return Ok(Flow::Continue);
        }
        // 2) 缓冲为空：命令 or SQL。
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(Flow::Continue);
        }
        if trimmed.starts_with("--") {
            return Ok(Flow::Continue); // 注释不入缓冲（SQL*Plus 会把 `--` 送出去；我们丢弃更省事，且无歧义）
        }
        if trimmed == "/" {
            return self.run_buffer();
        }
        if let Some(flow) = self.try_command(trimmed)? {
            return Ok(flow);
        }
        // 3) 新语句：**替换**缓冲（SQL*Plus：任何新输入行替换当前缓冲）。
        self.buf.clear();
        self.buf.push(line);
        self.current = 1;
        if SqlBuffer::ends_with_semicolon(&self.buf.text()) {
            self.execute_buffer()?;
        } else {
            self.filling = true;
        }
        Ok(Flow::Continue)
    }

    /// **`/` 与 `RUN`**：跑当前缓冲。
    fn run_buffer(&mut self) -> Result<Flow, ConnError> {
        if self.buf.is_empty() {
            self.out.line(true, "!  缓冲为空");
            return Ok(Flow::Continue);
        }
        self.execute_buffer()?;
        Ok(Flow::Continue)
    }

    /// **执行缓冲里的 SQL**（ECHO/TIMING/FEEDBACK 都在这条路上）。
    fn execute_buffer(&mut self) -> Result<(), ConnError> {
        let sql = self.buf.text();
        if self.settings.echo {
            for l in self.buf.lines() {
                self.out.line(true, l);
            }
        }
        let started = Instant::now();
        let to_term = self.settings.termout || self.interactive;
        let result = self.conn.execute(&sql);
        match result {
            Ok(results) => {
                for r in &results {
                    self.out.result(&self.settings, r, to_term);
                }
                if self.settings.timing {
                    self.out.line(to_term, &format_elapsed(started.elapsed()));
                }
                Ok(())
            }
            Err(e) => {
                self.out.line(to_term, &format!("{e}"));
                if self.settings.sqlerror_exit {
                    // `WHENEVER SQLERROR EXIT`：脚本据此中断（退出码非 0）。
                    return Err(ConnError::Sql(format!(
                        "{e}（WHENEVER SQLERROR EXIT 生效）"
                    )));
                }
                Ok(())
            }
        }
    }

    /// **斜杠命令分派**；`None` = 不是命令（当 SQL 处理）。
    fn try_command(&mut self, line: &str) -> Result<Option<Flow>, ConnError> {
        // `@file`/`@@file`/`!cmd` 的**命令名里带着参数**（没有空白分隔）——
        // 先按前缀识别，再进下面的按词分派。
        if line.starts_with("@@") {
            let name = line.trim_start_matches('@').trim();
            return self.run_nested_script(name, true).map(Some);
        }
        if let Some(name) = line.strip_prefix('@') {
            return self.run_nested_script(name.trim(), false).map(Some);
        }
        if let Some(cmd) = line.strip_prefix('!') {
            return self.run_host(cmd.trim()).map(Some);
        }
        let (cmd, arg) = split_command(line);
        let upper = cmd.to_ascii_uppercase();
        match upper.as_str() {
            "SET" => {
                let (name, value) = split_command(arg);
                match self.settings.set(name, value) {
                    SetOutcome::Ok(what) => self
                        .out
                        .line(self.settings.termout || self.interactive, &what),
                    SetOutcome::Unknown(w) | SetOutcome::BadValue(w) => {
                        self.out.line(true, &format!("!  {w}"))
                    }
                }
                Ok(Some(Flow::Continue))
            }
            "SHOW" => {
                let which = (!arg.trim().is_empty()).then(|| arg.trim());
                if arg.trim().eq_ignore_ascii_case("SPOOL") {
                    if self.out.spooling() {
                        self.out
                            .line(true, &format!("spool {}", self.out.spool_path().display()));
                    } else {
                        self.out.line(true, "spool OFF");
                    }
                    return Ok(Some(Flow::Continue));
                }
                for (k, v) in self.settings.show(which) {
                    self.out.line(true, &format!("{k} {v}"));
                }
                Ok(Some(Flow::Continue))
            }
            "DEFINE" => {
                if arg.trim().is_empty() {
                    for (k, v) in &self.defines {
                        self.out.line(true, &format!("DEFINE {k} = \"{v}\""));
                    }
                } else if let Some((k, v)) = arg.split_once('=') {
                    self.define(k.trim(), v.trim().trim_matches('"'));
                } else {
                    let k = arg.trim();
                    match self.lookup_define(k) {
                        Some(v) => self
                            .out
                            .line(true, &format!("DEFINE {k} = \"{v}\"").to_owned()),
                        None => self.out.line(true, &format!("!  未定义：{k}")),
                    }
                }
                Ok(Some(Flow::Continue))
            }
            "UNDEFINE" => {
                let k = arg.trim().to_ascii_lowercase();
                self.defines.retain(|(n, _)| *n != k);
                Ok(Some(Flow::Continue))
            }
            "LIST" | "L" => {
                if self.buf.is_empty() {
                    self.out.line(true, "!  缓冲为空");
                    return Ok(Some(Flow::Continue));
                }
                let Some((from, to)) = parse_range(arg, self.buf.len()) else {
                    self.out.line(
                        true,
                        "!  LIST 的范围不认识（`L`/`L 3`/`L 2 5`/`L *`/`L LAST`）",
                    );
                    return Ok(Some(Flow::Continue));
                };
                let (from, to) = (from.min(self.buf.len()).max(1), to.min(self.buf.len()));
                for n in from..=to {
                    let mark = if n == self.current { "*" } else { " " };
                    let text = self.buf.get(n).unwrap_or("").to_owned();
                    self.out.line(true, &format!("{n:>3}{mark}  {text}"));
                }
                Ok(Some(Flow::Continue))
            }
            "RUN" | "R" => {
                if self.buf.is_empty() {
                    self.out.line(true, "!  缓冲为空");
                    return Ok(Some(Flow::Continue));
                }
                for n in 1..=self.buf.len() {
                    let text = self.buf.get(n).unwrap_or("").to_owned();
                    self.out.line(true, &format!("{n:>3}  {text}"));
                }
                self.execute_buffer()?;
                Ok(Some(Flow::Continue))
            }
            "DEL" | "DELETE" => {
                let Some((from, to)) = parse_range(arg, self.buf.len()) else {
                    self.out.line(true, "!  DEL 的范围不认识");
                    return Ok(Some(Flow::Continue));
                };
                let (from, to) = (from.max(1), to.min(self.buf.len()));
                if from <= to {
                    for _ in from..=to {
                        self.buf.delete(from);
                    }
                    self.current = from.min(self.buf.len()).max(1);
                }
                Ok(Some(Flow::Continue))
            }
            "APPEND" | "A" => {
                if self.buf.is_empty() {
                    self.out
                        .line(true, "!  缓冲为空（`APPEND` 要有一行当前行）");
                } else {
                    let n = self.current.max(1);
                    let text = if arg.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", arg)
                    };
                    self.buf.append_to(n, &text);
                    let t = self.buf.get(n).unwrap_or("").to_owned();
                    self.out.line(true, &format!("{n:>3}* {t}"));
                }
                Ok(Some(Flow::Continue))
            }
            "INPUT" | "I" => {
                let n = self.current.max(1);
                self.buf.insert_after(n, arg.to_owned());
                self.current = n + 1;
                let t = self.buf.get(self.current).unwrap_or("").to_owned();
                let n = self.current;
                self.out.line(true, &format!("{n:>3}* {t}"));
                Ok(Some(Flow::Continue))
            }
            "CHANGE" | "C" => {
                if self.buf.is_empty() {
                    self.out.line(true, "!  缓冲为空");
                    return Ok(Some(Flow::Continue));
                }
                let Some((old, new, _tail)) = parse_change(arg) else {
                    self.out.line(true, "!  CHANGE 形态：`C /旧/新/`");
                    return Ok(Some(Flow::Continue));
                };
                let n = self.current.max(1);
                let text = self.buf.get(n).unwrap_or("").to_owned();
                if !text.contains(&old) {
                    self.out.line(true, &format!("!  第 {n} 行没有 `{old}`"));
                    return Ok(Some(Flow::Continue));
                }
                let replaced = text.replacen(&old, &new, 1);
                self.buf.set(n, replaced.clone());
                self.out.line(true, &format!("{n:>3}* {replaced}"));
                Ok(Some(Flow::Continue))
            }
            "CLEAR" => {
                let what = arg.trim().to_ascii_uppercase();
                if what.starts_with("BUFF") || what.is_empty() {
                    self.buf.clear();
                    self.filling = false;
                    self.current = 0;
                } else {
                    self.out
                        .line(true, &format!("!  CLEAR 只支持 BUFFER（给的是 `{arg}`）"));
                }
                Ok(Some(Flow::Continue))
            }
            "SPOOL" => {
                let a = arg.trim();
                if a.eq_ignore_ascii_case("OFF") {
                    self.out.spool_off();
                    self.out.line(true, "spool 已关闭");
                } else if a.eq_ignore_ascii_case("OUT") {
                    if self.out.spooling() {
                        let p = self.out.spool_path().display().to_string();
                        self.out.spool_off();
                        self.out.line(true, &format!("spool 已关闭：{p}"));
                    } else {
                        self.out.line(true, "!  没在 SPOOL");
                    }
                } else if a.is_empty() {
                    self.out.line(
                        true,
                        if self.out.spooling() {
                            "spool 开着"
                        } else {
                            "spool OFF"
                        },
                    );
                } else {
                    let path = self.resolve_path(a);
                    match self.out.spool_on(&path) {
                        Ok(()) => self.out.line(true, &format!("spool {}", path.display())),
                        Err(e) => self.out.line(true, &format!("!  开 SPOOL 失败：{e}")),
                    }
                }
                Ok(Some(Flow::Continue))
            }
            "START" => self.run_nested_script(arg.trim(), false).map(Some),
            "HOST" => self.run_host(arg.trim()).map(Some),
            "DESCRIBE" | "DESC" => {
                if arg.trim().is_empty() {
                    self.out.line(true, "!  DESCRIBE 缺对象名");
                    return Ok(Some(Flow::Continue));
                }
                match self.conn.describe(arg.trim()) {
                    Ok(cols) => {
                        // SQL*Plus 的 DESCRIBE 版面（Name / Null? / Type）。
                        self.out.line(true, "");
                        self.out
                            .line(true, &format!("{:<40} {:<8} {}", "Name", "Null?", "Type"));
                        self.out.line(
                            true,
                            &format!(
                                "{:<40} {:<8} {}",
                                "-".repeat(40),
                                "-".repeat(8),
                                "-".repeat(28)
                            ),
                        );
                        for c in cols {
                            self.out.line(
                                true,
                                &format!(
                                    "{:<40} {:<8} {}",
                                    c.name,
                                    if c.nullable { "" } else { "NOT NULL" },
                                    c.type_name
                                ),
                            );
                        }
                        self.out.line(true, "");
                    }
                    Err(e) => self.out.line(true, &format!("!  {e}")),
                }
                Ok(Some(Flow::Continue))
            }
            "PROMPT" => {
                self.out
                    .line(self.settings.termout || self.interactive, arg);
                Ok(Some(Flow::Continue))
            }
            "REM" | "REMARK" => Ok(Some(Flow::Continue)),
            "HELP" => {
                for l in help::topics(arg.trim()) {
                    self.out.line(true, &l);
                }
                Ok(Some(Flow::Continue))
            }
            "WHENEVER" => {
                let a = arg.trim().to_ascii_uppercase();
                if let Some(rest) = a.strip_prefix("SQLERROR") {
                    match rest.trim() {
                        "EXIT" | "EXIT FAILURE" => {
                            self.settings.sqlerror_exit = true;
                            self.out.line(true, "WHENEVER SQLERROR EXIT");
                        }
                        "CONTINUE" => {
                            self.settings.sqlerror_exit = false;
                            self.out.line(true, "WHENEVER SQLERROR CONTINUE");
                        }
                        _ => self
                            .out
                            .line(true, "!  用法：WHENEVER SQLERROR EXIT|CONTINUE"),
                    }
                } else {
                    self.out
                        .line(true, "!  只支持 WHENEVER SQLERROR EXIT|CONTINUE");
                }
                Ok(Some(Flow::Continue))
            }
            "TIMING" => {
                let a = arg.trim();
                if a.is_empty() {
                    self.settings.timing = true;
                } else {
                    let _ = self.settings.set("timing", a);
                }
                self.out.line(
                    true,
                    &format!("timing {}", if self.settings.timing { "ON" } else { "OFF" }),
                );
                Ok(Some(Flow::Continue))
            }
            "EXIT" | "QUIT" => {
                let code = self.exit_code(arg);
                Ok(Some(Flow::Quit(code)))
            }
            _ => Ok(None),
        }
    }

    /// **`@file` / `START file`**（`relative` = `@@` 语义：相对当前脚本目录）。
    fn run_nested_script(&mut self, name: &str, relative: bool) -> Result<Flow, ConnError> {
        if name.is_empty() {
            self.out.line(true, "!  START/@ 缺脚本路径");
            return Ok(Flow::Continue);
        }
        let path = if relative {
            self.script_dir
                .clone()
                .map_or_else(|| PathBuf::from(name), |d| d.join(name))
        } else {
            self.resolve_path(name)
        };
        // 嵌套脚本期间：把脚本目录换成它的（`@@` 用），退出时还原。
        let saved_dir = self.script_dir.clone();
        self.script_dir = path.parent().map(Path::to_path_buf);
        let outcome = self.run_script(&path);
        self.script_dir = saved_dir;
        match outcome {
            Ok(0) => Ok(Flow::Continue),
            Ok(code) => Ok(Flow::Quit(code)),
            Err(e) => {
                self.out.line(true, &format!("!  {e}"));
                Ok(Flow::Continue)
            }
        }
    }

    /// **`HOST` / `!cmd`**：跑一条宿主命令。
    fn run_host(&mut self, cmd: &str) -> Result<Flow, ConnError> {
        if cmd.trim().is_empty() {
            self.out.line(true, "!  HOST 缺命令");
            return Ok(Flow::Continue);
        }
        match std::process::Command::new("sh").arg("-c").arg(cmd).status() {
            Ok(st) => self.out.line(true, &format!("宿主命令退出码：{st}")),
            Err(e) => self.out.line(true, &format!("!  跑宿主命令失败：{e}")),
        }
        Ok(Flow::Continue)
    }

    fn exit_code(&self, arg: &str) -> i32 {
        let a = arg.trim();
        if a.is_empty() {
            return 0;
        }
        if a.eq_ignore_ascii_case("success") {
            return 0;
        }
        if a.eq_ignore_ascii_case("failure") {
            return 1;
        }
        a.parse().unwrap_or(0)
    }

    /// 脚本/`@` 的相对路径：**相对脚本目录**（`@@` 语义）或当前目录。
    fn resolve_path(&self, name: &str) -> PathBuf {
        let p = Path::new(name);
        if p.is_absolute() {
            return p.to_path_buf();
        }
        if let Some(dir) = self.script_dir.clone() {
            let cand = dir.join(name);
            if cand.exists() {
                return cand;
            }
        }
        p.to_path_buf()
    }

    /// 定义替换变量（`&&` 记住）。
    fn define(&mut self, name: &str, value: &str) {
        let key = name.to_ascii_lowercase();
        if let Some(slot) = self.defines.iter_mut().find(|(n, _)| *n == key) {
            slot.1 = value.to_owned();
        } else {
            self.defines.push((key, value.to_owned()));
        }
    }

    fn lookup_define(&self, name: &str) -> Option<String> {
        let key = name.to_ascii_lowercase();
        self.defines
            .iter()
            .find(|(n, _)| *n == key)
            .map(|(_, v)| v.clone())
    }

    /// **替换变量**：`&name` / `&&name`（`&&` 会记住值）。
    fn substitute(&mut self, line: &str) -> String {
        let concat = self.settings.concat;
        if !line.contains(concat) {
            return line.to_owned();
        }
        let mut out = String::with_capacity(line.len());
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] != concat {
                out.push(chars[i]);
                i += 1;
                continue;
            }
            let remember = chars.get(i + 1) == Some(&concat);
            let start = if remember { i + 2 } else { i + 1 };
            let mut j = start;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            if j == start {
                out.push(chars[i]); // 光秃秃的 `&`
                i += 1;
                continue;
            }
            let name: String = chars[start..j].iter().collect();
            match self.lookup_define(&name) {
                Some(v) => {
                    if remember {
                        self.define(&name, &v);
                    }
                    out.push_str(&v);
                }
                None => {
                    out.push_str(&format!("!  替换变量 {name} 未定义（`DEFINE {name}=值`）"));
                }
            }
            i = j;
        }
        if self.settings.verify && out != line {
            self.out.line(true, &out);
        }
        out
    }
}

/// **切出命令与参数**（首个空白分界；`@file` / `!cmd` 这类整体当命令名）。
fn split_command(line: &str) -> (&str, &str) {
    let t = line.trim_start();
    match t.find(char::is_whitespace) {
        Some(i) => (&t[..i], t[i..].trim_start()),
        None => (t, ""),
    }
}

/// 供 `main` 用的便捷：跑脚本路径时记住脚本目录（`@@` 语义）。
impl Shell {
    /// 设定"当前脚本目录"（`@` 里再 `@` 时相对它解析）。
    pub fn set_script_dir(&mut self, dir: Option<PathBuf>) {
        self.script_dir = dir;
    }
}
