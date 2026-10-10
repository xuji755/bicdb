//! **动词与载荷编解码**（协议的第二层：帧之上，谁说什么）。
//!
//! 规格全文见 `docs/客户端协议_v0.1.md`；本模块是它的**唯一实现**（Rust 侧）。
//!
//! ```text
//! HELLO                    握手：协议版本 + 服务/引擎版本 + 实例（客户端据此判断能否说话）
//! AUTH                     认证：载荷 = 主体名 + 口令；应答 = 身份（谁在连）
//! STATUS                   服务自述（key=value 若干行）
//! SQL                      执行：载荷 = 参数序列 + SQL 文本；应答 = 语句结果序列
//! DESCRIBE <对象>           列定义：应答 = 列清单（名/形态/可空/类型码/长度/类型名）
//! SHUTDOWN <fast|immediate> 请服务收尾
//! ```
//!
//! **口令的过线纪律**：`AUTH` 的载荷里**只有**主体名与口令，**没有 `user_id`**
//! （`REQ-ISO-002`：身份只来自认证结果，绝不接受请求体里的用户号）；
//! 口令走的是**文件权限保护的本机套接字**，服务端读它只为了跑一次 PBKDF2 比对，
//! **不进日志、不进回执、不进诊断**（`doc/evidence/auth-20261007/evidence.md` §4）。
//!
//! **解析纪律（照 MySQL 包序号错乱的教训：错位要"看得出"）**：
//! - 每个变长字段**自带字节数**，解析按**字节游标**读满（`take_line` / `take_exact`），
//!   **不按行切分**——名字/文本里出现换行不该让后续字段整体错位；
//! - 定长字段（标记、长度行）**认不出就报错**，不做"尽力解释"；
//! - [`WIRE_VERSION`] 随**不能兼容的**载荷变化 +1，`HELLO` 双方互换版本，
//!   客户端不认识的版本**拒绝连**。

use crate::value::Value;

/// 协议版本（**不兼容变更才 +1**；本版 = 1）。
pub const WIRE_VERSION: u8 = 1;

/// 握手应答（`HELLO`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// 协议版本。
    pub wire: u8,
    /// 服务/引擎版本（`0.3.0` 形态）。
    pub version: String,
    /// 实例根区目录（诊断）。
    pub instance: String,
}

impl Hello {
    /// 编码。
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "wire={}\nversion={}\ninstance={}\n",
            self.wire, self.version, self.instance
        )
    }

    /// 解码（缺项按空——握手失败由版本核对兜底）。
    #[must_use]
    pub fn decode(text: &str) -> Self {
        let get = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(k))
                .unwrap_or("")
                .trim()
                .to_owned()
        };
        Self {
            wire: get("wire=").parse().unwrap_or(0),
            version: get("version="),
            instance: get("instance="),
        }
    }
}

/// **认证请求**（`AUTH` 的载荷）：主体名 + 口令。
///
/// ```text
/// U <用户名字节数> \n <用户名字节> \n
/// S <口令字节数> \n <口令字节> \n
/// ```
///
/// 两段都**自带字节数**（照本模块的解析纪律：按字节游标读满，名字/口令里
/// 出现换行也不该错位）；读完必须**到头**（多一个字节就是错位）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthRequest {
    /// 主体名（实例内唯一的登录名）。
    pub user: String,
    /// 口令（明文——**只应走本机套接字**；网络承载属"对外协议"切片）。
    pub password: String,
}

impl AuthRequest {
    /// 编码。
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.byte_prefixed(b'U', self.user.as_bytes());
        w.byte_prefixed(b'S', self.password.as_bytes());
        w.out
    }

    /// 解码。
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut r = Reader::new(bytes);
        let user = r.byte_prefixed(b'U', "主体名")?;
        let password = r.byte_prefixed(b'S', "口令")?;
        r.finish()?;
        Ok(Self {
            user: String::from_utf8_lossy(&user).into_owned(),
            password: String::from_utf8_lossy(&password).into_owned(),
        })
    }
}

/// **认证应答**（`AUTH` 的 OK 载荷）——`key=value` 若干行（与 [`Hello`] 同规）。
///
/// ```text
/// user=<主体名>
/// user_id=<主体号>
/// status=active|expired
/// ```
///
/// `status=expired`（口令已过期）时**连接已建立但受限**：除本人改密
/// （`ALTER USER … REPLACE`）外一律拒绝——照 MySQL "新客户端密码过期后受限登录模式"
/// （`raw/02-mysql-locked-expired.md`）。改密成功后服务端解除限制。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthOk {
    /// 主体名（**以服务端目录为准回显**——客户端拿到的是权威拼写）。
    pub user: String,
    /// 主体号（服务端从目录解析，**不是客户端给的**）。
    pub user_id: u64,
    /// 口令是否已过期（受限会话）。
    pub expired: bool,
}

impl AuthOk {
    /// 编码。
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "user={}\nuser_id={}\nstatus={}\n",
            self.user,
            self.user_id,
            if self.expired { "expired" } else { "active" }
        )
    }

    /// 解码（缺项按空/0——认证应答的形状由响应码兜底）。
    #[must_use]
    pub fn decode(text: &str) -> Self {
        let get = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(k))
                .unwrap_or("")
                .trim()
                .to_owned()
        };
        Self {
            user: get("user="),
            user_id: get("user_id=").parse().unwrap_or(0),
            expired: get("status=") == "expired",
        }
    }
}

/// Additive ROUTE response. Strings are hex UTF-8 to preserve embedded newlines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedWorkspace {
    /// Authenticated principal, never supplied by the request.
    pub user_id: u64,
    /// Owned workspace ID.
    pub workspace_id: u64,
    /// Workspace label.
    pub name: String,
    /// Registered absolute directory.
    pub root: String,
}
impl OwnedWorkspace {
    /// Encode server response.
    pub fn encode(&self) -> String {
        format!(
            "user_id={}\nworkspace_id={}\nname_hex={}\nroot_hex={}\n",
            self.user_id,
            self.workspace_id,
            crate::value::hex_encode(self.name.as_bytes()),
            crate::value::hex_encode(self.root.as_bytes())
        )
    }
    /// Strictly decode identity and path; never guess an incomplete route.
    pub fn decode(text: &str) -> Result<Self, String> {
        let get = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key))
                .ok_or_else(|| "ROUTE 应答缺字段".to_owned())
        };
        let decode = |key: &str| -> Result<String, String> {
            let s = get(key)?;
            if s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("ROUTE 非法编码".into());
            }
            String::from_utf8(crate::value::hex_decode(s)).map_err(|_| "ROUTE 非 UTF-8".into())
        };
        let result = Self {
            user_id: get("user_id=")?.parse().map_err(|_| "ROUTE 非法主体号")?,
            workspace_id: get("workspace_id=")?
                .parse()
                .map_err(|_| "ROUTE 非法工作区号")?,
            name: decode("name_hex=")?,
            root: decode("root_hex=")?,
        };
        if result.user_id == 0 || result.workspace_id == 0 || result.root.is_empty() {
            return Err("ROUTE 非法绑定".into());
        }
        Ok(result)
    }
}

/// 结果集的一列（名 + 形态 + 类型信息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// 列名。
    pub name: String,
    /// **形态**（驱动据此把线上文本/字节转成原生类型）：`n` 数值 / `b` 字节串 / `o` 布尔。
    pub kind: char,
    /// 是否可空（结果集里恒 `true`——引擎按值给；`DESCRIBE` 有真值）。
    pub nullable: bool,
    /// 引擎类型码（`DESCRIBE` 有；结果集里为 0）。
    pub type_code: u32,
    /// 声明长度（`DESCRIBE` 有；结果集里为 0）。
    pub length: u32,
    /// 类型名（`DESCRIBE` 的显示形态；结果集里为空）。
    pub type_name: String,
}

/// 一条语句的结果（与 `bicdb-sql` 的 `QueryResult` 同形，但**值走线上模型**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    /// 结果集。
    Rows {
        /// 列。
        columns: Vec<Column>,
        /// 行 × 列。
        rows: Vec<Vec<Value>>,
    },
    /// 影响行数。
    Affected(u64),
    /// DDL 回执。
    Ddl(String),
    /// 事务回执。
    Txn(String),
}

/// **一条 SQL 请求**：参数序列 + SQL 文本。
///
/// ```text
/// P <参数数> \n
///   N <名字节数> \n <名字节> \n      × 参数数
///   V <值单元>                        （值单元自带长度，自成一行）
/// Q <SQL 字节数> \n <SQL 字节>
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SqlRequest {
    /// SQL 文本（可含多条语句，`;` 分隔）。
    pub sql: String,
    /// 参数（名 → 值）。
    pub params: Vec<(String, Value)>,
}

impl SqlRequest {
    /// 编码。
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.line(&format!("P {}", self.params.len()));
        for (name, v) in &self.params {
            w.byte_prefixed(b'N', name.as_bytes());
            w.cell(v);
        }
        w.byte_prefixed(b'Q', self.sql.as_bytes());
        w.out
    }

    /// 解码。
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut r = Reader::new(bytes);
        let n = r.head_u64(b'P', "参数数")?;
        let mut params = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let name = r.byte_prefixed(b'N', "参数名")?;
            let v = r.cell()?;
            params.push((String::from_utf8_lossy(&name).into_owned(), v));
        }
        let sql = r.byte_prefixed(b'Q', "SQL 文本")?;
        r.finish()?;
        Ok(Self {
            sql: String::from_utf8_lossy(&sql).into_owned(),
            params,
        })
    }
}

/// **结果序列编码**（服务端 → 客户端）。
#[must_use]
pub fn encode_statements(list: &[Statement]) -> Vec<u8> {
    let mut w = Writer::new();
    for s in list {
        match s {
            Statement::Rows { columns, rows } => {
                w.line(&format!("R {} {}", columns.len(), rows.len()));
                for c in columns {
                    w.column(c, false);
                }
                for r in rows {
                    for v in r {
                        w.cell(v);
                    }
                }
            }
            Statement::Affected(n) => w.line(&format!("A {n}")),
            Statement::Ddl(t) => w.byte_prefixed(b'D', t.as_bytes()),
            Statement::Txn(t) => w.byte_prefixed(b'T', t.as_bytes()),
        }
    }
    w.out
}

/// **结果序列解码**（客户端）。
pub fn decode_statements(bytes: &[u8]) -> Result<Vec<Statement>, String> {
    let mut r = Reader::new(bytes);
    let mut out = Vec::new();
    while !r.done() {
        match r.peek()? {
            b'R' => {
                let (ncols, nrows) = r.rows_head()?;
                let mut columns = Vec::with_capacity(ncols);
                for _ in 0..ncols {
                    columns.push(r.column()?);
                }
                let mut rows = Vec::with_capacity(nrows);
                for _ in 0..nrows {
                    let mut row = Vec::with_capacity(ncols);
                    for _ in 0..ncols {
                        row.push(r.cell()?);
                    }
                    rows.push(row);
                }
                out.push(Statement::Rows { columns, rows });
            }
            b'A' => {
                let n = r.head_u64(b'A', "影响行数")?;
                out.push(Statement::Affected(n));
            }
            b'D' => {
                let t = r.byte_prefixed(b'D', "DDL 回执")?;
                out.push(Statement::Ddl(String::from_utf8_lossy(&t).into_owned()));
            }
            b'T' => {
                let t = r.byte_prefixed(b'T', "事务回执")?;
                out.push(Statement::Txn(String::from_utf8_lossy(&t).into_owned()));
            }
            other => {
                return Err(format!(
                    "未知语句标记 `{}`（第 {} 字节）",
                    other as char, r.pos
                ))
            }
        }
    }
    Ok(out)
}

/// **列清单编码**（`DESCRIBE` 应答）。
#[must_use]
pub fn encode_columns(cols: &[Column]) -> Vec<u8> {
    let mut w = Writer::new();
    w.line(&format!("C {}", cols.len()));
    for c in cols {
        w.column(c, true);
    }
    w.out
}

/// 列清单解码。
pub fn decode_columns(bytes: &[u8]) -> Result<Vec<Column>, String> {
    let mut r = Reader::new(bytes);
    let n = r.head_u64(b'C', "列数")?;
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        out.push(r.column()?);
    }
    r.finish()?;
    Ok(out)
}

// ───────────────────────── 线（写） ─────────────────────────

/// 写侧：字节缓冲 + 几个格式原语。
struct Writer {
    out: Vec<u8>,
}

impl Writer {
    fn new() -> Self {
        Self { out: Vec::new() }
    }

    /// 一个头行（含换行）。
    fn line(&mut self, text: &str) {
        self.out.extend_from_slice(text.as_bytes());
        self.out.push(b'\n');
    }

    /// **字节前缀字段**：`<标记> <字节数>\n<字节>\n`。
    fn byte_prefixed(&mut self, tag: u8, body: &[u8]) {
        self.out.push(tag);
        self.out.push(b' ');
        self.out
            .extend_from_slice(body.len().to_string().as_bytes());
        self.out.push(b'\n');
        self.out.extend_from_slice(body);
        self.out.push(b'\n');
    }

    /// 一个值单元（自成一行：`<标记><字节数>\n<载荷>`）。
    fn cell(&mut self, v: &Value) {
        v.encode(&mut self.out);
        self.out.push(b'\n');
    }

    /// 一条列记录。
    ///
    /// ```text
    /// C <形态> <可空> <类型码> <长度> <名字节数> <类型名字节数> \n
    /// <名字节> \n <类型名字节> \n
    /// ```
    fn column(&mut self, c: &Column, full: bool) {
        let (nullable, type_code, length, type_name) = if full {
            (
                u8::from(c.nullable),
                c.type_code,
                c.length,
                c.type_name.as_str(),
            )
        } else {
            (1, 0, 0, "")
        };
        self.line(&format!(
            "C {} {} {} {} {} {}",
            c.kind,
            nullable,
            type_code,
            length,
            c.name.len(),
            type_name.len()
        ));
        self.out.extend_from_slice(c.name.as_bytes());
        self.out.push(b'\n');
        self.out.extend_from_slice(type_name.as_bytes());
        self.out.push(b'\n');
    }
}

// ───────────────────────── 读（字节游标） ─────────────────────────

/// 读侧：**字节游标**（按声明的字节数读满——不按行切分）。
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn done(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn peek(&self) -> Result<u8, String> {
        self.buf
            .get(self.pos)
            .copied()
            .ok_or_else(|| format!("载荷在 {} 字节处就到头了", self.pos))
    }

    /// 读一行（不含换行；到载荷末尾也算一行）。
    fn line(&mut self) -> Result<&'a [u8], String> {
        let start = self.pos;
        let end = self.buf[start..]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(self.buf.len(), |i| start + i);
        self.pos = (end + 1).min(self.buf.len());
        Ok(&self.buf[start..end])
    }

    /// 读一个 **`<标记> <字节数>` 头行**并返回载荷字节数。
    fn head_u64(&mut self, tag: u8, what: &str) -> Result<u64, String> {
        let line = self.line()?;
        let (t, rest) = line.split_first().ok_or_else(|| format!("{what}：空行"))?;
        if *t != tag {
            return Err(format!(
                "{what}：期待标记 `{}`，读到 `{}`",
                tag as char, *t as char
            ));
        }
        std::str::from_utf8(rest)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| {
                format!(
                    "{what}：头行 `{}{}` 里不是数",
                    tag as char,
                    String::from_utf8_lossy(rest)
                )
            })
    }

    fn take_exact(&mut self, n: usize, what: &str) -> Result<&'a [u8], String> {
        let end = self.pos + n;
        if end > self.buf.len() {
            return Err(format!(
                "{what}：要 {n} 字节，载荷只剩 {} 字节",
                self.buf.len() - self.pos
            ));
        }
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn expect_nl(&mut self, what: &str) -> Result<(), String> {
        match self.buf.get(self.pos) {
            Some(b'\n') => {
                self.pos += 1;
                Ok(())
            }
            other => Err(format!(
                "{what}：字段后应是换行，读到 {}",
                other.map_or("载荷末尾".to_owned(), |b| format!("`{}`", *b as char))
            )),
        }
    }

    /// **`<标记> <字节数>\n<字节>\n`** —— 读满声明的字节数，再吃掉分隔换行。
    fn byte_prefixed(&mut self, tag: u8, what: &str) -> Result<Vec<u8>, String> {
        let n = self.head_u64(tag, what)? as usize;
        let body = self.take_exact(n, what)?.to_vec();
        self.expect_nl(what)?;
        Ok(body)
    }

    /// 一个值单元（*不做**跨行**扫描*：长度说了算）。
    fn cell(&mut self) -> Result<Value, String> {
        let line = self.line()?;
        let (tag, rest) = line.split_first().ok_or("值单元：空行")?;
        let len: usize = std::str::from_utf8(rest)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| {
                format!(
                    "值单元：头行 `{}` 里不是字节数",
                    String::from_utf8_lossy(line)
                )
            })?;
        // 载荷**按字节读满**（十六进制字节串、十进制数值文本都不会含换行，
        // 但读法不依赖这一点——长度是权威）。单元形态：`<标记><字节数>\n<载荷>\n`。
        let body = self.take_exact(len, "值单元")?;
        let text = String::from_utf8_lossy(body).into_owned();
        self.expect_nl("值单元")?;
        Ok(Value::decode(*tag, &text))
    }

    /// `R <列数> <行数>` 头行。
    fn rows_head(&mut self) -> Result<(usize, usize), String> {
        let line = self.line()?;
        let (tag, rest) = line.split_first().ok_or("结果集：空行")?;
        if *tag != b'R' {
            return Err(format!("结果集：期待 `R`，读到 `{}`", *tag as char));
        }
        let text = String::from_utf8_lossy(rest);
        let mut it = text.split_whitespace();
        let ncols = it.next().and_then(|s| s.parse().ok());
        let nrows = it.next().and_then(|s| s.parse().ok());
        match (ncols, nrows) {
            (Some(c), Some(r)) => Ok((c, r)),
            _ => Err(format!("结果集：头行 `{}` 里不是两个数", text.trim())),
        }
    }

    /// 一条列记录（与 [`Writer::column`] 对称）。
    fn column(&mut self) -> Result<Column, String> {
        let line = self.line()?;
        let text = String::from_utf8_lossy(line).into_owned();
        let mut it = text.split_whitespace();
        let (Some(tag), Some(kind), Some(nul), Some(code), Some(len), Some(nlen), Some(tlen)) = (
            it.next(),
            it.next(),
            it.next(),
            it.next(),
            it.next(),
            it.next(),
            it.next(),
        ) else {
            return Err(format!("列记录字段不全：`{text}`"));
        };
        if tag != "C" {
            return Err(format!("列记录：期待 `C`，读到 `{tag}`"));
        }
        let nlen: usize = nlen.parse().map_err(|_| format!("列名长度：`{nlen}`"))?;
        let tlen: usize = tlen.parse().map_err(|_| format!("类型名长度：`{tlen}`"))?;
        let name = self.take_exact(nlen, "列名")?.to_vec();
        self.expect_nl("列名")?;
        let type_name = self.take_exact(tlen, "类型名")?.to_vec();
        self.expect_nl("类型名")?;
        Ok(Column {
            name: String::from_utf8_lossy(&name).into_owned(),
            kind: kind.chars().next().unwrap_or('b'),
            nullable: nul == "1",
            type_code: code.parse().unwrap_or(0),
            length: len.parse().unwrap_or(0),
            type_name: String::from_utf8_lossy(&type_name).into_owned(),
        })
    }

    /// 载荷**必须读到头**（多一个字节都算错位）。
    fn finish(&self) -> Result<(), String> {
        if self.done() {
            Ok(())
        } else {
            Err(format!("载荷还有 {} 字节没读完", self.buf.len() - self.pos))
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn owned_route_roundtrip_and_invalid_fields() {
        let route = super::OwnedWorkspace {
            user_id: 2,
            workspace_id: 3,
            name: "私有区\n=test".into(),
            root: "/tmp/私有\n=root".into(),
        };
        let decoded = super::OwnedWorkspace::decode(&route.encode()).unwrap();
        assert_eq!(decoded.user_id, route.user_id);
        assert_eq!(decoded.workspace_id, route.workspace_id);
        assert_eq!(decoded.name, route.name);
        assert_eq!(decoded.root, route.root);
        assert!(super::OwnedWorkspace::decode(
            "user_id=0\nworkspace_id=1\nname_hex=61\nroot_hex=2f61\n"
        )
        .is_err());
        assert!(super::OwnedWorkspace::decode(
            "user_id=1\nworkspace_id=2\nname_hex=zz\nroot_hex=2f61\n"
        )
        .is_err());
        assert!(super::OwnedWorkspace::decode(
            "user_id=1\nworkspace_id=2\nname_hex=61\nroot_hex=ff\n"
        )
        .is_err());
    }

    use super::*;

    #[test]
    fn hello_round_trips() {
        let h = Hello {
            wire: WIRE_VERSION,
            version: "0.2.0".to_owned(),
            instance: "/data/bicdb".to_owned(),
        };
        assert_eq!(Hello::decode(&h.encode()), h);
    }

    #[test]
    fn auth_request_and_reply_round_trip() {
        // **口令里有换行/非 ASCII**：长度是权威，不该被换行带偏。
        let req = AuthRequest {
            user: "alice".to_owned(),
            password: "p@ss\n口令 with spaces".to_owned(),
        };
        assert_eq!(AuthRequest::decode(&req.encode()).expect("解"), req);
        // 空口令也解得出来（拒绝它的是认证层，不是编解码层）。
        let empty = AuthRequest {
            user: "bob".to_owned(),
            password: String::new(),
        };
        assert_eq!(AuthRequest::decode(&empty.encode()).expect("解"), empty);

        let ok = AuthOk {
            user: "alice".to_owned(),
            user_id: 7,
            expired: true,
        };
        assert_eq!(AuthOk::decode(&ok.encode()), ok);
        assert!(ok.encode().contains("status=expired"));
        assert!(AuthOk::decode(&AuthOk::default().encode()).user.is_empty());
    }

    #[test]
    fn a_malformed_auth_payload_is_named_not_guessed() {
        let good = AuthRequest {
            user: "alice".to_owned(),
            password: "secret".to_owned(),
        }
        .encode();
        // 截断：报错，不是"尽力解出半条"。
        for cut in 1..good.len() {
            assert!(
                AuthRequest::decode(&good[..good.len() - cut]).is_err(),
                "截断 {cut} 字节应报错"
            );
        }
        // 多一段：同样报错（读到头才算解对）。
        let mut extra = good.clone();
        extra.extend_from_slice(b"X 1\na\n");
        assert!(AuthRequest::decode(&extra).is_err());
        // 段序错（口令在前）：认得出的标记就该认，认不出就报错。
        let mut swapped = Vec::new();
        swapped.extend_from_slice(b"S 3\nabc\n");
        swapped.extend_from_slice(b"U 5\nalice\n");
        let err = AuthRequest::decode(&swapped).expect_err("应报错");
        assert!(err.contains('S'), "{err}");
    }

    #[test]
    fn sql_request_round_trips_with_odd_payloads() {
        // **换行/非 UTF-8/空文本**都过一遍：长度是权威，不该被换行带偏。
        let req = SqlRequest {
            sql: "SELECT 1\nFROM t; -- 中文\n".to_owned(),
            params: vec![
                ("a".to_owned(), Value::Null),
                ("b".to_owned(), Value::Number("123.45".to_owned())),
                ("b2".to_owned(), Value::Number(String::new())),
                ("c".to_owned(), Value::Bytes(b"x\ny".to_vec())),
                ("d".to_owned(), Value::Bytes(vec![0x00, 0xff, 0x0a])),
                ("空".to_owned(), Value::Bool(true)),
            ],
        };
        assert_eq!(SqlRequest::decode(&req.encode()).expect("解"), req);
    }

    #[test]
    fn statements_round_trip_including_newlines_in_names() {
        let list = vec![
            Statement::Rows {
                columns: vec![
                    Column {
                        name: "ID".to_owned(),
                        kind: 'n',
                        nullable: true,
                        type_code: 0,
                        length: 0,
                        type_name: String::new(),
                    },
                    Column {
                        // 名字里带换行：长度前缀说了算，后面不该错位。
                        name: "怪\n名".to_owned(),
                        kind: 'b',
                        nullable: true,
                        type_code: 0,
                        length: 0,
                        type_name: String::new(),
                    },
                ],
                rows: vec![
                    vec![
                        Value::Number("1".to_owned()),
                        Value::Bytes(b"alpha".to_vec()),
                    ],
                    vec![Value::Null, Value::Bytes(vec![0xde, 0xad, 0x0a])],
                ],
            },
            Statement::Affected(7),
            Statement::Ddl("已建表 t\n（含嵌套换行）".to_owned()),
            Statement::Txn("已提交".to_owned()),
        ];
        let bytes = encode_statements(&list);
        assert_eq!(decode_statements(&bytes).expect("解"), list);
    }

    #[test]
    fn columns_round_trip_with_full_metadata() {
        let cols = vec![
            Column {
                name: "ID".to_owned(),
                kind: 'n',
                nullable: false,
                type_code: 1,
                length: 22,
                type_name: "NUMBER".to_owned(),
            },
            Column {
                name: "名 字".to_owned(),
                kind: 'b',
                nullable: true,
                type_code: 3,
                length: 32,
                type_name: "VARCHAR2(32)".to_owned(),
            },
        ];
        let bytes = encode_columns(&cols);
        assert_eq!(decode_columns(&bytes).expect("解"), cols);
        // `DESCRIBE` 的载荷**读到头**才算解对（多余字节是错位）。
        let mut extra = bytes.clone();
        extra.push(b'x');
        assert!(decode_columns(&extra).is_err());
    }

    #[test]
    fn a_truncated_or_misaligned_payload_is_named_not_guessed() {
        let list = vec![Statement::Rows {
            columns: vec![Column {
                name: "A".to_owned(),
                kind: 'n',
                nullable: true,
                type_code: 0,
                length: 0,
                type_name: String::new(),
            }],
            rows: vec![vec![Value::Number("1".to_owned())]],
        }];
        let bytes = encode_statements(&list);
        // 截断：**报错**，不是"尽力解出半条"。
        for cut in 1..bytes.len() {
            assert!(
                decode_statements(&bytes[..bytes.len() - cut]).is_err()
                    || decode_statements(&bytes[..bytes.len() - cut]).is_ok_and(|s| s.is_empty()),
                "截断 {cut} 字节应报错或解出空"
            );
        }
        // 头行被改：认不出标记 ⇒ 具名报错。
        let mut bad = bytes.clone();
        bad[0] = b'X';
        let err = decode_statements(&bad).expect_err("应报错");
        assert!(err.contains('X'), "{err}");
    }
}
