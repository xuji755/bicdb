//! **词法**（设计 `doc/SQL前端设计_v0.1.md` §3.1；切片 S1）。
//!
//! 三条纪律：
//! 1. **零依赖**——不查目录、不做名字折叠、不解析字面量值（`NUMBER` 文本留给
//!    ②阶段走 `TYP` 内核；REQ-SQL-002 的正面清单）；
//! 2. **位置齐全**——每个 token 带**字节偏移区间**（错误可定位；验收要它）；
//! 3. **关键字是闭集**——与标识符同形，按闭集判定；闭集之外的词一律标识符。

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

    /// 合并两个区间（用于把子表达式的区间并成父节点区间）。
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// **关键字闭集**（REQ-SQL-005 正面清单用到的一切；大小写不敏感）。
///
/// 闭集纪律（REQ-SQL-006）：不在本表的词（`WITH`、`EXISTS`、`OVER`、`RETURNING`…）
/// 就是普通标识符——**语法层没有它们的产生式**，用到即语法错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
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
    /// `LIKE`（列表外；词法保留以便报"不支持"）
    Like,
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
    /// `NULLIF`
    Nullif,
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
    /// `NOT_` 之外的：`NULL` 已有；这里放 `DEFAULT`（拒绝用）
    Default,
    /// `PRIMARY`（拒绝用）
    Primary,
    /// `KEY`（拒绝用）
    Key,
    /// `REFERENCES`（拒绝用）
    References,
    /// `CHECK`（拒绝用）
    Check,
    /// `RETURNING`（拒绝用）
    Returning,
    /// `RIGHT`（拒绝用——REQ-SQL-006）
    Right,
    /// `FULL`（拒绝用）
    Full,
    /// `NATURAL`（拒绝用）
    Natural,
    /// `USING`（拒绝用）
    Using,
    /// `OVER`（拒绝用——窗口函数）
    Over,
    /// `EXISTS`（拒绝用——子查询）
    Exists,
}

impl Keyword {
    /// 关键字查表（大小写不敏感；闭集）。
    fn lookup(text: &str) -> Option<Self> {
        let up = text.to_ascii_uppercase();
        Some(match up.as_str() {
            "SELECT" => Self::Select,
            "INSERT" => Self::Insert,
            "UPDATE" => Self::Update,
            "DELETE" => Self::Delete,
            "CREATE" => Self::Create,
            "DROP" => Self::Drop,
            "ALTER" => Self::Alter,
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
            "LIKE" => Self::Like,
            "CASE" => Self::Case,
            "WHEN" => Self::When,
            "THEN" => Self::Then,
            "ELSE" => Self::Else,
            "END" => Self::End,
            "CAST" => Self::Cast,
            "COALESCE" => Self::Coalesce,
            "NULLIF" => Self::Nullif,
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
            "DEFAULT" => Self::Default,
            "PRIMARY" => Self::Primary,
            "KEY" => Self::Key,
            "REFERENCES" => Self::References,
            "CHECK" => Self::Check,
            "RETURNING" => Self::Returning,
            "RIGHT" => Self::Right,
            "FULL" => Self::Full,
            "NATURAL" => Self::Natural,
            "USING" => Self::Using,
            "OVER" => Self::Over,
            "EXISTS" => Self::Exists,
            _ => return None,
        })
    }

    /// 原词（错误文案用）。
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
            Self::Create => "CREATE",
            Self::Drop => "DROP",
            Self::Alter => "ALTER",
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
            Self::Like => "LIKE",
            Self::Case => "CASE",
            Self::When => "WHEN",
            Self::Then => "THEN",
            Self::Else => "ELSE",
            Self::End => "END",
            Self::Cast => "CAST",
            Self::Coalesce => "COALESCE",
            Self::Nullif => "NULLIF",
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
            Self::Default => "DEFAULT",
            Self::Primary => "PRIMARY",
            Self::Key => "KEY",
            Self::References => "REFERENCES",
            Self::Check => "CHECK",
            Self::Returning => "RETURNING",
            Self::Right => "RIGHT",
            Self::Full => "FULL",
            Self::Natural => "NATURAL",
            Self::Using => "USING",
            Self::Over => "OVER",
            Self::Exists => "EXISTS",
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
    /// `<->`（L2 距离；RET 的向量接口）
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
    /// 标识符（**记原文本**——大小写折叠在 ② 阶段）。
    Ident(String),
    /// 数字字面量（**原文本**——解析成 `NUMBER` 在 ② 走 TYP 内核）。
    Number(String),
    /// 字符串字面量（**已解转义**的字节内容：SQL 的 `''` ⇒ `'`）。
    Str(Vec<u8>),
    /// 参数 `:name`（记名字，不记下标——序号在 ② 定型后定）。
    Param(String),
    /// 标点 / 操作符。
    Punct(Punct),
    /// 语句结束（文本末尾也算一个；解析器据它收尾）。
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
    /// 出错位置。
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
            // 行注释 `-- …`
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
            b':' => {
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
                out.push(Token {
                    kind: TokenKind::Param(text[name_start..i].to_owned()),
                    span: Span::new(start, i),
                });
            }
            b'0'..=b'9' => {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'.' {
                    let dot = i;
                    i += 1;
                    if i < bytes.len() && bytes[i].is_ascii_digit() {
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                    } else {
                        i = dot; // `1.` 里的点不是小数点的情形（如 `1 ..`）——回退
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
                    // 标识符原文本（含 `$` 结尾名——保留规则在 ② 判，词法不管）
                    None => TokenKind::Ident(word.to_owned()),
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

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// 读一个标点/操作符（**先长后短**：`<->` 必须先于 `<=` 与 `<` 试）。
fn read_punct(bytes: &[u8], i: usize) -> Option<(Punct, usize)> {
    let two = |k: u8| bytes.get(i + 1) == Some(&k);
    let three = |a: u8, b: u8| bytes.get(i + 1) == Some(&a) && bytes.get(i + 2) == Some(&b);
    // 三字节操作符（向量距离接口）
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
        b',' => (Punct::Comma, 1),
        b'.' => (Punct::Dot, 1),
        b';' => (Punct::Semi, 1),
        b'*' => (Punct::Star, 1),
        b'+' => (Punct::Plus, 1),
        b'-' => (Punct::Minus, 1),
        b'/' => (Punct::Slash, 1),
        b'=' => (Punct::Eq, 1),
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
    fn keywords_are_case_insensitive_and_idents_keep_text() {
        assert_eq!(
            kinds("select Foo from T"),
            vec![
                TokenKind::Keyword(Keyword::Select),
                TokenKind::Ident("Foo".to_owned()), // 原文本——折叠在 ②
                TokenKind::Keyword(Keyword::From),
                TokenKind::Ident("T".to_owned()),
                TokenKind::Eof,
            ]
        );
        // `WITH` 是关键字但**没有产生式**（子查询/CTE 不提供）——词法保留以便报错。
        assert_eq!(
            kinds("with"),
            vec![TokenKind::Keyword(Keyword::With), TokenKind::Eof]
        );
        // 不在闭集里的词就是标识符。
        assert_eq!(
            kinds("window"),
            vec![TokenKind::Ident("window".to_owned()), TokenKind::Eof]
        );
    }

    #[test]
    fn numbers_strings_params_and_operators() {
        assert_eq!(
            kinds("1 2.5 :p 'a''b' <> <= >= <-> <=> <#>"),
            vec![
                TokenKind::Number("1".to_owned()),
                TokenKind::Number("2.5".to_owned()),
                TokenKind::Param("p".to_owned()),
                TokenKind::Str(b"a'b".to_vec()),
                TokenKind::Punct(Punct::Ne),
                TokenKind::Punct(Punct::Le),
                TokenKind::Punct(Punct::Ge),
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
        assert_eq!(toks[1].span, Span::new(22, 23)); // b 的字节区间（含多字节字符的注释）
    }

    #[test]
    fn lex_errors_carry_positions() {
        let e = tokenize("x 'unterminated").unwrap_err();
        assert_eq!(e.span.start, 2);
        let e2 = tokenize("/* never closed").unwrap_err();
        assert_eq!(e2.message, "块注释未闭合");
        let e3 = tokenize(":1").unwrap_err();
        assert_eq!(e3.message, "`:` 之后必须是参数名");
    }

    #[test]
    fn dollar_suffixed_names_are_plain_identifiers_at_lex_time() {
        // 保留规则（`$` 结尾名）是**② 阶段**的事；词法只给文本。
        assert_eq!(
            kinds("file$"),
            vec![TokenKind::Ident("file$".to_owned()), TokenKind::Eof]
        );
    }
}
