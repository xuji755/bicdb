//! **本地控制/查询的过渡协议**（服务模式用；正式协议在 `bicdb-net`）。
//!
//! ```text
//! 请求： VERB \n <载荷长度> \n <载荷字节>
//! 应答： OK|ERR \n <载荷长度> \n <载荷字节>
//! ```
//!
//! **记档（重要）**：设计里的对外协议是 `bicdb-net` 的**版本化请求协议**
//! （封套/终态/对账，REQ-API-*），那是独立切片。本模块只是**本机服务模式**
//! 的过渡形态：长度前缀 + 文本载荷，够 `bicdbcli`/`bicdb stop|status` 用，
//! **不声称与设计协议同形**——它随 `bicdb-net` 落地即被替换。
//!
//! 动词（闭集）：
//! - `HELLO`：握手（返回协议版本 + 实例路径），客户端据此判断"这是本工具的服务"；
//! - `STATUS`：服务与实例的自述（`key=value` 若干行）；
//! - `SQL`：执行 SQL（载荷 = **SQL 文本 + 参数值**，见 [`encode_sql_request`]；
//!   应答载荷 = 结果集编码）；
//! - `DESCRIBE`：列定义（载荷 = 对象名；应答载荷 = 列清单编码——`DESC` 命令用）；
//! - `SHUTDOWN <fast|immediate>`：请服务收尾退出。
//!
//! **为什么长度前缀**：SQL 与结果里必然有换行——按行读会在第一行就断错。

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

/// 协议版本（过渡形态；正式协议落地后本常量作废）。
pub const WIRE_VERSION: u8 = 1;

/// 单帧上限（防御：坏客户端不该让服务分配无界内存）。
pub const MAX_FRAME: u32 = 64 * 1024 * 1024;

/// 协议错误。
#[derive(Debug)]
pub enum WireError {
    /// I/O。
    Io(std::io::Error),
    /// 帧格式非法（缺行/长度越界/非 UTF-8）。
    BadFrame(String),
    /// 对端回了 `ERR`。
    Remote(String),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Io(e) => write!(f, "连接：{e}"),
            WireError::BadFrame(w) => write!(f, "协议帧非法：{w}"),
            WireError::Remote(w) => f.write_str(w),
        }
    }
}

impl std::error::Error for WireError {}

impl From<std::io::Error> for WireError {
    fn from(e: std::io::Error) -> Self {
        WireError::Io(e)
    }
}

/// 写一帧（`head` = 首行：请求是动词、应答是 `OK`/`ERR`）。
pub fn write_frame(s: &mut UnixStream, head: &str, payload: &str) -> Result<(), WireError> {
    let body = payload.as_bytes();
    let mut out = Vec::with_capacity(body.len() + head.len() + 16);
    out.extend_from_slice(head.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(body.len().to_string().as_bytes());
    out.push(b'\n');
    out.extend_from_slice(body);
    s.write_all(&out)?;
    s.flush()?;
    Ok(())
}

/// 读一帧（首行 + 载荷）。按**字节**读到长度为止（载荷里可以有换行）。
pub fn read_frame(s: &mut UnixStream) -> Result<(String, String), WireError> {
    let mut head = Vec::new();
    read_line(s, &mut head)?;
    let mut len_line = Vec::new();
    read_line(s, &mut len_line)?;
    let len: u32 = std::str::from_utf8(&len_line)
        .map_err(|_| WireError::BadFrame("长度行非 UTF-8".to_owned()))?
        .trim()
        .parse()
        .map_err(|_| WireError::BadFrame("长度行不是数".to_owned()))?;
    if len > MAX_FRAME {
        return Err(WireError::BadFrame(format!("载荷 {len} 字节超过上限")));
    }
    let mut body = vec![0u8; len as usize];
    s.read_exact(&mut body)?;
    Ok((
        String::from_utf8_lossy(&head).trim().to_owned(),
        String::from_utf8_lossy(&body).into_owned(),
    ))
}

fn read_line(s: &mut UnixStream, out: &mut Vec<u8>) -> Result<(), WireError> {
    out.clear();
    let mut byte = [0u8; 1];
    loop {
        s.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            return Ok(());
        }
        out.push(byte[0]);
        if out.len() > 1024 {
            return Err(WireError::BadFrame("首行过长".to_owned()));
        }
    }
}

/// **持久连接的客户端**（`bicdbcli` 用：连接 = 会话，事务跨语句保持）。
pub struct Client {
    stream: UnixStream,
}

impl Client {
    /// 连服务。
    pub fn connect(socket: &std::path::Path) -> Result<Self, WireError> {
        Ok(Self {
            stream: UnixStream::connect(socket)?,
        })
    }

    /// 一次请求-应答（同一连接、同一会话）。
    pub fn call(&mut self, verb: &str, payload: &str) -> Result<String, WireError> {
        write_frame(&mut self.stream, verb, payload)?;
        let (head, body) = read_frame(&mut self.stream)?;
        match head.as_str() {
            "OK" => Ok(body),
            "ERR" => Err(WireError::Remote(body)),
            other => Err(WireError::BadFrame(format!("未知应答首行 `{other}`"))),
        }
    }
}

/// **SQL 请求的载荷编码**：`参数序列` + `SQL 文本`。
///
/// ```text
/// <参数数> \n
/// 每参数： <名>\n<类型码>\n<值长度>\n<值>\n              （类型码 n/b/o/-）
/// <SQL 字节长度> \n <SQL 文本>
/// ```
///
/// **为什么参数要随请求走**：直连形态下 `--param` 由会话层摆位；经服务时若只送
/// SQL 文本，参数就丢在客户端了——同一句 SQL 在"服务在跑/不在跑"两种形态下
/// 行为不同，是绝不能有的静默分歧（实测踩到）。
#[must_use]
pub fn encode_sql_request(sql: &str, params: &[(&str, bicdb_exec::Value)]) -> String {
    use bicdb_exec::Value;
    let mut out = format!("{}\n", params.len());
    for (name, v) in params {
        let (kind, text) = match v {
            Value::Null => ('-', String::new()),
            Value::Number(n) => ('n', n.to_string()),
            Value::Bytes(b) => ('b', String::from_utf8_lossy(b).into_owned()),
            Value::Bool(b) => ('o', if *b { "1" } else { "0" }.to_owned()),
        };
        out.push_str(&format!("{name}\n{kind}\n{}\n{text}\n", text.len()));
    }
    out.push_str(&format!("{}\n{sql}", sql.len()));
    out
}

/// 解 SQL 请求载荷 → `(SQL 文本, 参数序列)`。
#[must_use]
pub fn decode_sql_request(payload: &str) -> (String, Vec<(String, bicdb_exec::Value)>) {
    use bicdb_exec::Value;
    let mut it = payload.split_inclusive('\n');
    let Some(n_line) = it.next() else {
        return (String::new(), Vec::new());
    };
    let n: usize = n_line.trim().parse().unwrap_or(0);
    let mut params = Vec::with_capacity(n);
    for _ in 0..n {
        let (Some(name), Some(kind), Some(vlen), Some(val)) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            break;
        };
        let _ = name; // 名字用于诊断；摆位由会话层按清单做
        let name = name.trim_end_matches('\n').to_owned();
        let kind = kind.trim();
        let len: usize = vlen.trim().parse().unwrap_or(0);
        let raw = val.trim_end_matches('\n');
        let text: String = raw.as_bytes().get(..len).map_or_else(
            || raw.to_owned(),
            |b| String::from_utf8_lossy(b).into_owned(),
        );
        let v = match kind {
            "-" => Value::Null,
            "o" => Value::Bool(text == "1"),
            "b" => Value::Bytes(text.into_bytes()),
            _ => match bicdb_types::Number::parse(&text) {
                Ok(num) => Value::Number(num),
                Err(_) => Value::Bytes(text.into_bytes()),
            },
        };
        params.push((name, v));
    }
    let sql = match it.next() {
        Some(len_line) => {
            let len: usize = len_line.trim().parse().unwrap_or(0);
            let rest: String = it.collect();
            rest.as_bytes()
                .get(..len)
                .map_or(rest.clone(), |b| String::from_utf8_lossy(b).into_owned())
        }
        None => String::new(),
    };
    (sql, params)
}

/// **列清单编码**（`DESCRIBE` 的应答；`bicdbcli` 侧解码）。
///
/// 形态：`<列数>\n` 然后每列三行 `名字\n可空(0/1)\n类型名`。
#[must_use]
pub fn encode_columns(cols: &[(String, bool, String)]) -> String {
    let mut out = format!("{}\n", cols.len());
    for (name, nullable, ty) in cols {
        out.push_str(name);
        out.push('\n');
        out.push_str(if *nullable { "1" } else { "0" });
        out.push('\n');
        out.push_str(ty);
        out.push('\n');
    }
    out
}

/// 列清单解码。
#[must_use]
pub fn decode_columns(text: &str) -> Vec<(String, bool, String)> {
    let mut it = text.lines();
    let n: usize = it.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let (Some(name), Some(nul), Some(ty)) = (it.next(), it.next(), it.next()) else {
            break;
        };
        out.push((name.to_owned(), nul.trim() == "1", ty.to_owned()));
    }
    out
}

/// **一次请求-应答**（客户端用）。
pub fn call(socket: &std::path::Path, verb: &str, payload: &str) -> Result<String, WireError> {
    let mut s = UnixStream::connect(socket)?;
    write_frame(&mut s, verb, payload)?;
    let (head, body) = read_frame(&mut s)?;
    match head.as_str() {
        "OK" => Ok(body),
        "ERR" => Err(WireError::Remote(body)),
        other => Err(WireError::BadFrame(format!("未知应答首行 `{other}`"))),
    }
}

/// **一次请求-应答，带超时**（`pg_ctl -w` 式的等待用）。
pub fn call_timeout(
    socket: &std::path::Path,
    verb: &str,
    payload: &str,
    timeout: std::time::Duration,
) -> Result<String, WireError> {
    let mut s = UnixStream::connect(socket)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    write_frame(&mut s, verb, payload)?;
    let (head, body) = read_frame(&mut s)?;
    match head.as_str() {
        "OK" => Ok(body),
        "ERR" => Err(WireError::Remote(body)),
        other => Err(WireError::BadFrame(format!("未知应答首行 `{other}`"))),
    }
}

// ───────────────────────── 结果集的载荷编码 ─────────────────────────

/// 结果集编码（`bicdb-sql` 的 [`bicdb_sql::session::QueryResult`] 的文本形态）。
///
/// 形态（每项一行头 + 长度前缀的行体，见 `decode_results`）：
/// ```text
/// ROWS <列数> <行数>
/// <列名长度>\n<列名> … × 列数
/// <单元长度>\n<单元> … × 行数×列数
/// AFFECTED <n>
/// DDL <长度>\n<文本>
/// TXN <长度>\n<文本>
/// ```
#[must_use]
pub fn encode_results(results: &[bicdb_sql::session::QueryResult]) -> String {
    use bicdb_sql::session::QueryResult as Q;
    let mut out = String::new();
    for kind in results {
        match kind {
            Q::Rows { columns, rows } => {
                out.push_str(&format!("ROWS {} {}\n", columns.len(), rows.len()));
                for c in columns {
                    out.push_str(&format!("{}\n{}\n", c.len(), c));
                }
                for r in rows {
                    for cell in r {
                        out.push_str(&format!("{}\n{}\n", cell.len(), cell));
                    }
                }
            }
            Q::Affected(n) => out.push_str(&format!("AFFECTED {n}\n")),
            Q::Ddl(t) => out.push_str(&format!("DDL {}\n{}\n", t.len(), t)),
            Q::Txn(t) => out.push_str(&format!("TXN {}\n{}\n", t.len(), t)),
        }
    }
    out
}

/// 结果集解码（客户端用）。
#[must_use]
pub fn decode_results(text: &str) -> Vec<bicdb_sql::session::QueryResult> {
    use bicdb_sql::session::QueryResult as Q;
    let mut out = Vec::new();
    let mut it = text.split_inclusive('\n');
    while let Some(line) = it.next() {
        let line = line.trim_end_matches('\n');
        if let Some(rest) = line.strip_prefix("ROWS ") {
            let mut parts = rest.split_whitespace();
            let ncols: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let nrows: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let columns = read_block(&mut it, ncols);
            let cells = read_block(&mut it, ncols * nrows);
            let rows = cells.chunks(ncols.max(1)).map(<[String]>::to_vec).collect();
            out.push(Q::Rows { columns, rows });
        } else if let Some(n) = line.strip_prefix("AFFECTED ") {
            out.push(Q::Affected(n.trim().parse().unwrap_or(0)));
        } else if let Some(t) = line.strip_prefix("DDL ") {
            // 长度在**首行**里（`DDL <字节数>`），随后一行是文本。
            out.push(Q::Ddl(read_body(&mut it, t)));
        } else if let Some(t) = line.strip_prefix("TXN ") {
            out.push(Q::Txn(read_body(&mut it, t)));
        }
    }
    out
}

/// 读 `n` 个"长度前缀块"。
fn read_block<'a>(it: &mut impl Iterator<Item = &'a str>, n: usize) -> Vec<String> {
    (0..n).map(|_| read_one(it)).collect()
}

/// 读一个"长度前缀块"（长度行 + 值行）。
fn read_one<'a>(it: &mut impl Iterator<Item = &'a str>) -> String {
    let Some(len_line) = it.next() else {
        return String::new();
    };
    let len: usize = len_line.trim_end_matches('\n').trim().parse().unwrap_or(0);
    read_n(it, len)
}

/// 读"长度在首行里"的块（`DDL <字节数>` / `TXN <字节数>`）。
fn read_body<'a>(it: &mut impl Iterator<Item = &'a str>, len_text: &str) -> String {
    let len: usize = len_text.trim().parse().unwrap_or(0);
    read_n(it, len)
}

/// 读下一行的前 `len` **字节**（编码侧写的是字节数；中文按字节切才对）。
fn read_n<'a>(it: &mut impl Iterator<Item = &'a str>, len: usize) -> String {
    let Some(val) = it.next() else {
        return String::new();
    };
    let v = val.trim_end_matches('\n');
    match v.as_bytes().get(..len) {
        Some(b) => String::from_utf8_lossy(b).into_owned(),
        None => v.to_owned(),
    }
}
