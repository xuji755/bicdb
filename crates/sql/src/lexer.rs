//! **词法**（设计 `doc/SQL前端设计_v0.1.md` §3.2；切片 S1）——规则对齐
//! **PostgreSQL 的扫描器**（`scan.l`，简化子集；取证 `12-pg-parser-source.txt`）。
//!
//! 三条纪律：
//! 1. **零依赖**——不查目录、不解析字面量值（`NUMBER` 文本留给 ② 走 `TYP` 内核）；
//! 2. **位置齐全**——每个 token 带**字节偏移区间**（设计记档：PG 用字符偏移，
//!    我们用字节——诊断更强，且与行格式的字节语义一致）；
//! 3. **标识符折叠照 PG**（⚠️ 见下"折叠位置"）。
//!
//! **折叠位置（照 PG，记档差异）**：PG 在**扫描器**里把未引号标识符折叠为
//! 小写、引号标识符原样保留；本词法器同样处理。设计规格 REQ-SQL-002 写的是
//! "折叠规则在 ② 应用"——**本实现按 PG 提前到 ①**（对齐 PG 的直接后果，
//! 待评审；见设计 §3.2 的记档）。

use std::fmt;

/// 源码位置（字节偏移区间，半开 `[start, end)`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// 起始字节偏移。
    pub start: usize,
    /// 结束字节偏移（不含）。
    pub end: usize,
}

impl Span {
    /// 新建。
    #[must_use]
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// 合并两个区间。
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// **关键字闭集**（REQ-SQL-005 正面清单所需；对齐 PG 的"保留/非保留"思想但
/// 只留我们用得到的——清单外构造在语法层没有产生式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    /// `SHOW` (`SHOW TABLES` metadata statement).
    Show,
    // 语句起始
    /// `SELECT`
    Select,
    /// `INSERT`
    Insert,
    /// `UPDATE`
    Update,
    /// `DELETE`
    Delete,
    /// `CREATE`
    Create,
    /// `DROP`
    Drop,
    /// `ALTER`
    Alter,
    /// `OPEN`
    Open,
    /// `BEGIN`
    Begin,
    /// `COMMIT`
    Commit,
    /// `ROLLBACK`
    Rollback,
    // 子句
    /// `DISTINCT`
    Distinct,
    /// `FROM`
    From,
    /// `WHERE`
    Where,
    /// `GROUP`
    Group,
    /// `BY`
    By,
    /// `HAVING`
    Having,
    /// `ORDER`
    Order,
    /// `LIMIT`
    Limit,
    /// `OFFSET`
    Offset,
    /// `AS`
    As,
    /// `ASC`
    Asc,
    /// `DESC`
    Desc,
    /// `VALUES`
    Values,
    /// `SET`
    Set,
    /// `INTO`
    Into,
    /// `ON`
    On,
    /// `JOIN`
    Join,
    /// `INNER`
    Inner,
    /// `LEFT`
    Left,
    /// `OUTER`
    Outer,
    /// `UNION`
    Union,
    /// `INTERSECT`
    Intersect,
    /// `EXCEPT`
    Except,
    /// `ALL`
    All,
    /// `AND`
    And,
    /// `OR`
    Or,
    /// `NOT`
    Not,
    /// `IS`
    Is,
    /// `NULL`
    Null,
    /// `IN`
    In,
    /// `BETWEEN`
    Between,
    /// `CASE`
    Case,
    /// `WHEN`
    When,
    /// `THEN`
    Then,
    /// `ELSE`
    Else,
    /// `END`
    End,
    /// `CAST`
    Cast,
    /// `COALESCE`
    Coalesce,
    /// `TRUE`
    True,
    /// `FALSE`
    False,
    // DDL
    /// `TABLE`
    Table,
    /// `INDEX`
    Index,
    /// `UNIQUE`
    Unique,
    /// `GRAPH`
    Graph,
    /// `VERTEX`
    Vertex,
    /// `EDGE`
    Edge,
    /// `WORKSPACE`
    Workspace,
    /// `FOR`
    For,
    /// `USER`
    User,
    /// `NAME`
    Name,
    /// `CLONE`
    Clone,
    /// `OF`
    Of,
    /// `QUOTA`
    Quota,
    /// `WITH`
    With,
    /// `READ`
    Read,
    /// `ONLY`
    Only,
    /// `WRITE`
    Write,
    /// `FORCE`
    Force,
    /// `VERIFY`
    Verify,
    /// `RECOVERY`
    Recovery,
    /// `PAGE`
    Page,
    /// `OBJECT`
    Object,
    // 仅用于"响亮拒绝"（闭集外构造；词法保留使错误文案更准）
    /// `LIKE`
    Like,
    /// `RIGHT`
    Right,
    /// `FULL`
    Full,
    /// `NATURAL`
    Natural,
    /// `USING`
    Using,
    /// `OVER`
    Over,
    /// `EXISTS`
    Exists,
    /// `RETURNING`
    Returning,
    /// `DEFAULT`
    Default,
    /// `PRIMARY`
    Primary,
    /// `KEY`
    Key,
    /// `REFERENCES`
    References,
    /// `CHECK`
    Check,
    /// `NULLS`（`NULLS FIRST/LAST`——语法暂不开，词典保留）
    Nulls,
    /// `FIRST`
    First,
    /// `LAST`
    Last,
    /// `SAVEPOINT`
    Savepoint,
    /// `TRUNCATE`
    Truncate,
    /// `ADD`（`ALTER TABLE ADD`）
    Add,
    // ── DCL（`doc/DCL语句设计_v0.1.md` §2.1；语法冻入 D1）──────────
    /// `SYSTEM`
    System,
    /// `DATABASE`
    Database,
    /// `SESSION`
    Session,
    /// `TEMPLATE`
    Template,
    /// `FILESYSTEM`
    Filesystem,
    /// `ALLOCATE`
    Allocate,
    /// `OFF`
    Off,
    /// `CLEAR`
    Clear,
    /// `TO`（`ALTER WORKSPACE … TO TEMPLATE`）
    To,
    /// `PAUSE`（`ALTER USER … PAUSE`——≈ Oracle `ACCOUNT LOCK`）
    Pause,
    /// `RESUME`
    Resume,
    /// `EXPIRE`（`IDENTIFIED BY '…' EXPIRE`）
    Expire,
    /// `REPLACE`（`IDENTIFIED BY '<新>' REPLACE '<旧>'`）
    Replace,
    /// `CASCADE`（`DROP USER … CASCADE`）
    Cascade,
    /// `UNLIMITED`（配额无上限）
    Unlimited,
    /// `IDENTIFIED`（`IDENTIFIED BY '<口令>'`）
    Identified,
}

impl Keyword {
    /// 关键字查表（大小写不敏感；闭集）。
    fn lookup(text: &str) -> Option<Self> {
        let up = text.to_ascii_uppercase();
        Some(match up.as_str() {
            "SHOW" => Self::Show,
            "SELECT" => Self::Select,
            "INSERT" => Self::Insert,
            "UPDATE" => Self::Update,
            "DELETE" => Self::Delete,
            "CREATE" => Self::Create,
            "DROP" => Self::Drop,
            "ALTER" => Self::Alter,
            "OPEN" => Self::Open,
            "BEGIN" => Self::Begin,
            "COMMIT" => Self::Commit,
            "ROLLBACK" => Self::Rollback,
            "DISTINCT" => Self::Distinct,
            "FROM" => Self::From,
            "WHERE" => Self::Where,
            "GROUP" => Self::Group,
            "BY" => Self::By,
            "HAVING" => Self::Having,
            "ORDER" => Self::Order,
            "LIMIT" => Self::Limit,
            "OFFSET" => Self::Offset,
            "AS" => Self::As,
            "ASC" => Self::Asc,
            "DESC" => Self::Desc,
            "VALUES" => Self::Values,
            "SET" => Self::Set,
            "INTO" => Self::Into,
            "ON" => Self::On,
            "JOIN" => Self::Join,
            "INNER" => Self::Inner,
            "LEFT" => Self::Left,
            "OUTER" => Self::Outer,
            "UNION" => Self::Union,
            "INTERSECT" => Self::Intersect,
            "EXCEPT" => Self::Except,
            "ALL" => Self::All,
            "AND" => Self::And,
            "OR" => Self::Or,
            "NOT" => Self::Not,
            "IS" => Self::Is,
            "NULL" => Self::Null,
            "IN" => Self::In,
            "BETWEEN" => Self::Between,
            "CASE" => Self::Case,
            "WHEN" => Self::When,
            "THEN" => Self::Then,
            "ELSE" => Self::Else,
            "END" => Self::End,
            "CAST" => Self::Cast,
            "COALESCE" => Self::Coalesce,
            "TRUE" => Self::True,
            "FALSE" => Self::False,
            "TABLE" => Self::Table,
            "INDEX" => Self::Index,
            "UNIQUE" => Self::Unique,
            "GRAPH" => Self::Graph,
            "VERTEX" => Self::Vertex,
            "EDGE" => Self::Edge,
            "WORKSPACE" => Self::Workspace,
            "FOR" => Self::For,
            "USER" => Self::User,
            "NAME" => Self::Name,
            "CLONE" => Self::Clone,
            "OF" => Self::Of,
            "QUOTA" => Self::Quota,
            "WITH" => Self::With,
            "READ" => Self::Read,
            "ONLY" => Self::Only,
            "WRITE" => Self::Write,
            "FORCE" => Self::Force,
            "VERIFY" => Self::Verify,
            "RECOVERY" => Self::Recovery,
            "PAGE" => Self::Page,
            "OBJECT" => Self::Object,
            "LIKE" => Self::Like,
            "RIGHT" => Self::Right,
            "FULL" => Self::Full,
            "NATURAL" => Self::Natural,
            "USING" => Self::Using,
            "OVER" => Self::Over,
            "EXISTS" => Self::Exists,
            "RETURNING" => Self::Returning,
            "DEFAULT" => Self::Default,
            "PRIMARY" => Self::Primary,
            "KEY" => Self::Key,
            "REFERENCES" => Self::References,
            "CHECK" => Self::Check,
            "NULLS" => Self::Nulls,
            "FIRST" => Self::First,
            "LAST" => Self::Last,
            "SAVEPOINT" => Self::Savepoint,
            "TRUNCATE" => Self::Truncate,
            "ADD" => Self::Add,
            // ── DCL ──
            "SYSTEM" => Self::System,
            "DATABASE" => Self::Database,
            "SESSION" => Self::Session,
            "TEMPLATE" => Self::Template,
            "FILESYSTEM" => Self::Filesystem,
            "ALLOCATE" => Self::Allocate,
            "OFF" => Self::Off,
            "CLEAR" => Self::Clear,
            "TO" => Self::To,
            "PAUSE" => Self::Pause,
            "RESUME" => Self::Resume,
            "EXPIRE" => Self::Expire,
            "REPLACE" => Self::Replace,
            "CASCADE" => Self::Cascade,
            "UNLIMITED" => Self::Unlimited,
            "IDENTIFIED" => Self::Identified,
            _ => return None,
        })
    }

    /// 原词（错误文案用）。
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::Show => "SHOW",
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
            Self::Create => "CREATE",
            Self::Drop => "DROP",
            Self::Alter => "ALTER",
            Self::Open => "OPEN",
            Self::Begin => "BEGIN",
            Self::Commit => "COMMIT",
            Self::Rollback => "ROLLBACK",
            Self::Distinct => "DISTINCT",
            Self::From => "FROM",
            Self::Where => "WHERE",
            Self::Group => "GROUP",
            Self::By => "BY",
            Self::Having => "HAVING",
            Self::Order => "ORDER",
            Self::Limit => "LIMIT",
            Self::Offset => "OFFSET",
            Self::As => "AS",
            Self::Asc => "ASC",
            Self::Desc => "DESC",
            Self::Values => "VALUES",
            Self::Set => "SET",
            Self::Into => "INTO",
            Self::On => "ON",
            Self::Join => "JOIN",
            Self::Inner => "INNER",
            Self::Left => "LEFT",
            Self::Outer => "OUTER",
            Self::Union => "UNION",
            Self::Intersect => "INTERSECT",
            Self::Except => "EXCEPT",
            Self::All => "ALL",
            Self::And => "AND",
            Self::Or => "OR",
            Self::Not => "NOT",
            Self::Is => "IS",
            Self::Null => "NULL",
            Self::In => "IN",
            Self::Between => "BETWEEN",
            Self::Case => "CASE",
            Self::When => "WHEN",
            Self::Then => "THEN",
            Self::Else => "ELSE",
            Self::End => "END",
            Self::Cast => "CAST",
            Self::Coalesce => "COALESCE",
            Self::True => "TRUE",
            Self::False => "FALSE",
            Self::Table => "TABLE",
            Self::Index => "INDEX",
            Self::Unique => "UNIQUE",
            Self::Graph => "GRAPH",
            Self::Vertex => "VERTEX",
            Self::Edge => "EDGE",
            Self::Workspace => "WORKSPACE",
            Self::For => "FOR",
            Self::User => "USER",
            Self::Name => "NAME",
            Self::Clone => "CLONE",
            Self::Of => "OF",
            Self::Quota => "QUOTA",
            Self::With => "WITH",
            Self::Read => "READ",
            Self::Only => "ONLY",
            Self::Write => "WRITE",
            Self::Force => "FORCE",
            Self::Verify => "VERIFY",
            Self::Recovery => "RECOVERY",
            Self::Page => "PAGE",
            Self::Object => "OBJECT",
            Self::Like => "LIKE",
            Self::Right => "RIGHT",
            Self::Full => "FULL",
            Self::Natural => "NATURAL",
            Self::Using => "USING",
            Self::Over => "OVER",
            Self::Exists => "EXISTS",
            Self::Returning => "RETURNING",
            Self::Default => "DEFAULT",
            Self::Primary => "PRIMARY",
            Self::Key => "KEY",
            Self::References => "REFERENCES",
            Self::Check => "CHECK",
            Self::Nulls => "NULLS",
            Self::First => "FIRST",
            Self::Last => "LAST",
            Self::Savepoint => "SAVEPOINT",
            Self::Truncate => "TRUNCATE",
            Self::Add => "ADD",
            Self::System => "SYSTEM",
            Self::Database => "DATABASE",
            Self::Session => "SESSION",
            Self::Template => "TEMPLATE",
            Self::Filesystem => "FILESYSTEM",
            Self::Allocate => "ALLOCATE",
            Self::Off => "OFF",
            Self::Clear => "CLEAR",
            Self::To => "TO",
            Self::Pause => "PAUSE",
            Self::Resume => "RESUME",
            Self::Expire => "EXPIRE",
            Self::Replace => "REPLACE",
            Self::Cascade => "CASCADE",
            Self::Unlimited => "UNLIMITED",
            Self::Identified => "IDENTIFIED",
        }
    }
}

/// 标点与操作符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Punct {
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[` (fixed full-text JSON path only).
    LBracket,
    /// `]` (fixed full-text JSON path only).
    RBracket,
    /// `,`
    Comma,
    /// `.`
    Dot,
    /// `;`
    Semi,
    /// `*`
    Star,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `/`
    Slash,
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `::`（转型——PG 的 `TYPECAST`）
    Cast,
    /// `<->`（L2 距离；向量接口）
    L2,
    /// `<=>`（余弦距离）
    Cosine,
    /// `<#>`（负内积）
    NegInner,
}

/// 词素。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    /// 关键字（闭集）。
    Keyword(Keyword),
    /// **未引号标识符**（已按 PG 折叠为小写）。
    Ident(String),
    /// **引号标识符**（`"…"`；原样保留、大小写敏感）。
    QIdent(String),
    /// 数字字面量（**原文本**——值的解析在 ② 走 TYP 内核）。
    Number(String),
    /// 字符串字面量（已解转义的字节内容：SQL 的 `''` ⇒ `'`）。
    Str(Vec<u8>),
    /// 参数 `:name`（PG 用 `$n`；本库规格用 `:name`——设计记档）。
    Param(String),
    /// 标点 / 操作符。
    Punct(Punct),
    /// 语句结束（文本末尾也算一个）。
    Eof,
}

/// 一个词素 + 位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// 词素。
    pub kind: TokenKind,
    /// 字节偏移区间。
    pub span: Span,
}

/// 词法错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    /// 文案。
    pub message: String,
    /// 位置。
    pub span: Span,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "词法错误（字节 {}..{}）：{}",
            self.span.start, self.span.end, self.message
        )
    }
}

impl std::error::Error for LexError {}

/// 词法分析：文本 → token 流（末尾恒有一个 `Eof`）。
pub fn tokenize(text: &str) -> Result<Vec<Token>, LexError> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            // 行注释 `-- …`（PG 同名）
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            // 块注释 `/* … */`（不嵌套）
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let start = i;
                i += 2;
                loop {
                    if i + 1 >= bytes.len() {
                        return Err(LexError {
                            message: "块注释未闭合".to_owned(),
                            span: Span::new(start, bytes.len()),
                        });
                    }
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            // 字符串字面量（`''` 转义；PG 的普通字符串）
            b'\'' => {
                let start = i;
                i += 1;
                let mut value = Vec::new();
                loop {
                    if i >= bytes.len() {
                        return Err(LexError {
                            message: "字符串未闭合".to_owned(),
                            span: Span::new(start, i),
                        });
                    }
                    if bytes[i] == b'\'' {
                        if bytes.get(i + 1) == Some(&b'\'') {
                            value.push(b'\'');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    value.push(bytes[i]);
                    i += 1;
                }
                out.push(Token {
                    kind: TokenKind::Str(value),
                    span: Span::new(start, i),
                });
            }
            // **引号标识符**（`""` 转义；原样保留——PG 的 `"Foo"`）
            b'"' => {
                let start = i;
                i += 1;
                let mut name = Vec::new();
                loop {
                    if i >= bytes.len() {
                        return Err(LexError {
                            message: "引号标识符未闭合".to_owned(),
                            span: Span::new(start, i),
                        });
                    }
                    if bytes[i] == b'"' {
                        if bytes.get(i + 1) == Some(&b'"') {
                            name.push(b'"');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    name.push(bytes[i]);
                    i += 1;
                }
                let name = String::from_utf8(name).map_err(|_| LexError {
                    message: "引号标识符不是合法 UTF-8".to_owned(),
                    span: Span::new(start, i),
                })?;
                if name.is_empty() {
                    return Err(LexError {
                        message: "引号标识符不得为空".to_owned(),
                        span: Span::new(start, i),
                    });
                }
                out.push(Token {
                    kind: TokenKind::QIdent(name),
                    span: Span::new(start, i),
                });
            }
            b':' if bytes.get(i + 1) != Some(&b':') => {
                let start = i;
                i += 1;
                let name_start = i;
                if i >= bytes.len() || !is_ident_start(bytes[i]) {
                    return Err(LexError {
                        message: "`:` 之后必须是参数名".to_owned(),
                        span: Span::new(start, i),
                    });
                }
                while i < bytes.len() && is_ident_cont(bytes[i]) {
                    i += 1;
                }
                // 未引号参数名折叠照 PG 的标识符规则（小写）。
                out.push(Token {
                    kind: TokenKind::Param(text[name_start..i].to_ascii_lowercase()),
                    span: Span::new(start, i),
                });
            }
            // 数字：`digits[.digits][e[+-]digits]`（含 `.5` 形态；照 PG；原文本）
            b'.' if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if matches!(bytes.get(i), Some(b'e' | b'E')) {
                    let mut j = i + 1;
                    if matches!(bytes.get(j), Some(b'+' | b'-')) {
                        j += 1;
                    }
                    if bytes.get(j).is_some_and(u8::is_ascii_digit) {
                        i = j;
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                }
                out.push(Token {
                    kind: TokenKind::Number(text[start..i].to_owned()),
                    span: Span::new(start, i),
                });
            }
            b'0'..=b'9' => {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if bytes.get(i) == Some(&b'.') && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                if matches!(bytes.get(i), Some(b'e' | b'E')) {
                    let mut j = i + 1;
                    if matches!(bytes.get(j), Some(b'+' | b'-')) {
                        j += 1;
                    }
                    if bytes.get(j).is_some_and(u8::is_ascii_digit) {
                        i = j;
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                }
                out.push(Token {
                    kind: TokenKind::Number(text[start..i].to_owned()),
                    span: Span::new(start, i),
                });
            }
            _ if is_ident_start(b) => {
                let start = i;
                while i < bytes.len() && is_ident_cont(bytes[i]) {
                    i += 1;
                }
                let word = &text[start..i];
                let kind = match Keyword::lookup(word) {
                    Some(k) => TokenKind::Keyword(k),
                    // 未引号标识符：**照 PG 折叠为小写**（引号内的不折）。
                    None => TokenKind::Ident(word.to_ascii_lowercase()),
                };
                out.push(Token {
                    kind,
                    span: Span::new(start, i),
                });
            }
            _ => {
                let (punct, len) = read_punct(bytes, i).ok_or_else(|| LexError {
                    message: format!("无法识别的字符 `{}`", char::from(b)),
                    span: Span::new(i, i + 1),
                })?;
                out.push(Token {
                    kind: TokenKind::Punct(punct),
                    span: Span::new(i, i + len),
                });
                i += len;
            }
        }
    }
    out.push(Token {
        kind: TokenKind::Eof,
        span: Span::new(bytes.len(), bytes.len()),
    });
    Ok(out)
}

/// 标识符起始字节。
///
/// **非 ASCII 字节（≥ 0x80）算字母**——照 PG 的扫描器（`scanner.l` 的
/// `ident_start` 把高位置 1 的字节当字母）：这让 `名称`/`café` 这类标识符
/// 可用（未引号名只做 **ASCII** 折叠，非 ASCII 原样保留，与 PG 的
/// `downcase_identifier` 一致）。
fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

/// 标识符续接字节（同 PG 口径 + 我们的 `$`）。
fn is_ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// 读一个标点/操作符（**先长后短**：`<->` 必须先于 `<=` 与 `<` 试）。
fn read_punct(bytes: &[u8], i: usize) -> Option<(Punct, usize)> {
    let two = |k: u8| bytes.get(i + 1) == Some(&k);
    let three = |a: u8, b: u8| bytes.get(i + 1) == Some(&a) && bytes.get(i + 2) == Some(&b);
    if bytes[i] == b'<' {
        if three(b'-', b'>') {
            return Some((Punct::L2, 3));
        }
        if three(b'=', b'>') {
            return Some((Punct::Cosine, 3));
        }
        if three(b'#', b'>') {
            return Some((Punct::NegInner, 3));
        }
    }
    Some(match bytes[i] {
        b'(' => (Punct::LParen, 1),
        b')' => (Punct::RParen, 1),
        b'[' => (Punct::LBracket, 1),
        b']' => (Punct::RBracket, 1),
        b',' => (Punct::Comma, 1),
        b'.' => (Punct::Dot, 1),
        b';' => (Punct::Semi, 1),
        b'*' => (Punct::Star, 1),
        b'+' => (Punct::Plus, 1),
        b'-' => (Punct::Minus, 1),
        b'/' => (Punct::Slash, 1),
        b'=' => (Punct::Eq, 1),
        b':' if two(b':') => (Punct::Cast, 2),
        b'<' if two(b'>') => (Punct::Ne, 2),
        b'<' if two(b'=') => (Punct::Le, 2),
        b'<' => (Punct::Lt, 1),
        b'>' if two(b'=') => (Punct::Ge, 2),
        b'>' => (Punct::Gt, 1),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<TokenKind> {
        tokenize(text)
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    #[test]
    fn keywords_are_case_insensitive_and_unquoted_idents_fold_lower() {
        // 未引号标识符**照 PG 折叠为小写**；关键字闭集判定大小写不敏感。
        assert_eq!(
            kinds("select Foo from T"),
            vec![
                TokenKind::Keyword(Keyword::Select),
                TokenKind::Ident("foo".to_owned()),
                TokenKind::Keyword(Keyword::From),
                TokenKind::Ident("t".to_owned()),
                TokenKind::Eof,
            ]
        );
        // 闭集外的词就是标识符（`window`）。
        assert_eq!(
            kinds("window"),
            vec![TokenKind::Ident("window".to_owned()), TokenKind::Eof]
        );
    }

    #[test]
    fn quoted_identifiers_preserve_case() {
        assert_eq!(
            kinds("\"MyCol\" \"Sel\"\"ect\""),
            vec![
                TokenKind::QIdent("MyCol".to_owned()),
                TokenKind::QIdent("Sel\"ect".to_owned()), // `""` 转义
                TokenKind::Eof,
            ]
        );
        assert!(tokenize("\"\"").is_err(), "空引号标识符拒绝");
        assert!(tokenize("\"abc").is_err(), "未闭合拒绝");
    }

    #[test]
    fn numbers_follow_pg_shape_and_keep_raw_text() {
        assert_eq!(
            kinds("1 2.5 .5x 1e3 2.5E-2"),
            vec![
                TokenKind::Number("1".to_owned()),
                TokenKind::Number("2.5".to_owned()),
                TokenKind::Number(".5".to_owned()), // PG 允许 `.5`
                TokenKind::Ident("x".to_owned()),
                TokenKind::Number("1e3".to_owned()),
                TokenKind::Number("2.5E-2".to_owned()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn strings_params_operators_and_cast() {
        assert_eq!(
            kinds(":p 'a''b' <> <= >= :: <-> <=> <#>"),
            vec![
                TokenKind::Param("p".to_owned()),
                TokenKind::Str(b"a'b".to_vec()),
                TokenKind::Punct(Punct::Ne),
                TokenKind::Punct(Punct::Le),
                TokenKind::Punct(Punct::Ge),
                TokenKind::Punct(Punct::Cast),
                TokenKind::Punct(Punct::L2),
                TokenKind::Punct(Punct::Cosine),
                TokenKind::Punct(Punct::NegInner),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn comments_and_spans() {
        let toks = tokenize("a -- 注释\n/* 块 */ b").unwrap();
        assert_eq!(toks.len(), 3);
        assert_eq!(toks[0].span, Span::new(0, 1));
        assert_eq!(toks[1].span, Span::new(22, 23)); // 字节区间（含多字节注释）
        assert!(tokenize("/* never closed").is_err());
        assert!(tokenize("x 'unterminated").is_err());
    }
}
