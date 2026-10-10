//! **语法**（设计 `doc/SQL前端设计_v0.1.md` §3.3；切片 S1）——**照 PG 的产生式
//! 删减**：手写递归下降，每个 PG 产生式对应一个解析函数；**优先级表照抄 PG**
//! （`gram.y` 的 `%left/%right/%nonassoc` 声明，见设计 §3.3 与取证
//! `12-pg-parser-source.txt` §1）。
//!
//! **闭集纪律**（REQ-SQL-006）：不提供清单里的构造**没有产生式**——用到即是语法
//! 错误（"不支持该构造"），不是"解析后再拒绝"。对齐 PG 后 = **把 PG 的对应
//! 产生式删掉**。

use crate::ast::*;
use crate::lexer::{tokenize, Keyword, LexError, Punct, Span, Token, TokenKind};

/// 语法错误（带字节区间）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// 文案。
    pub message: String,
    /// 位置。
    pub span: Span,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "语法错误（字节 {}..{}）：{}",
            self.span.start, self.span.end, self.message
        )
    }
}

impl std::error::Error for ParseError {}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        Self {
            message: e.message,
            span: e.span,
        }
    }
}

/// 解析一条语句（文本 → Raw AST）。文本末尾可有 `;`。
pub fn parse(text: &str) -> Result<Stmt, ParseError> {
    let tokens = tokenize(text)?;
    let mut p = Parser {
        tokens,
        at: 0,
        src: text,
    };
    let stmt = p.statement()?;
    p.eat_punct(Punct::Semi);
    let tok = p.peek();
    if !matches!(tok.kind, TokenKind::Eof) {
        return Err(p.err_here("语句结束后出现多余内容"));
    }
    Ok(stmt)
}

/// 解析多条语句（按 `;` 分隔；末尾可分号）。
pub fn parse_many(text: &str) -> Result<Vec<Stmt>, ParseError> {
    let tokens = tokenize(text)?;
    let mut p = Parser {
        tokens,
        at: 0,
        src: text,
    };
    let mut out = Vec::new();
    loop {
        while p.eat_punct(Punct::Semi) {}
        if matches!(p.peek().kind, TokenKind::Eof) {
            return Ok(out);
        }
        out.push(p.statement()?);
        if !matches!(p.peek().kind, TokenKind::Eof) && !p.eat_punct(Punct::Semi) {
            return Err(p.err_here("语句之间需要 `;`"));
        }
    }
}

struct Parser<'a> {
    tokens: Vec<Token>,
    at: usize,
    /// 原文（关键字作名字时按 span 取原文本并折叠——与 PG 的扫描器同一规则）。
    src: &'a str,
}

impl Parser<'_> {
    // ── 基础 ──────────────────────────────────────────────

    fn peek(&self) -> &Token {
        &self.tokens[self.at]
    }

    fn peek2(&self) -> &Token {
        self.tokens.get(self.at + 1).unwrap_or_else(|| self.peek())
    }

    fn advance(&mut self) -> Token {
        let t = self.tokens[self.at].clone();
        if self.at + 1 < self.tokens.len() {
            self.at += 1;
        }
        t
    }

    fn err_here(&self, message: &str) -> ParseError {
        ParseError {
            message: message.to_owned(),
            span: self.peek().span,
        }
    }

    fn at_kw(&self, k: Keyword) -> bool {
        matches!(self.peek().kind, TokenKind::Keyword(k2) if k2 == k)
    }

    fn eat_kw(&mut self, k: Keyword) -> bool {
        if self.at_kw(k) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, k: Keyword) -> Result<Token, ParseError> {
        if self.at_kw(k) {
            Ok(self.advance())
        } else {
            Err(self.err_here(&format!("期望 `{}`", k.text())))
        }
    }

    fn at_punct(&self, p: Punct) -> bool {
        matches!(self.peek().kind, TokenKind::Punct(p2) if p2 == p)
    }

    fn eat_punct(&mut self, p: Punct) -> bool {
        if self.at_punct(p) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_punct(&mut self, p: Punct) -> Result<Token, ParseError> {
        if self.at_punct(p) {
            Ok(self.advance())
        } else {
            Err(self.err_here(&format!("期望 `{}`", punct_text(p))))
        }
    }

    /// 名字位置（**照 PG 的 `ColId`**）：标识符 / 引号标识符 / 关键字作名字。
    ///
    /// **折叠口径（照 PG 的扫描器）**：未引号的名字折为小写（词法层已折
    /// `Ident`；关键字形态在这里按 span 取原文本再折）；**引号标识符原样保留**。
    fn col_id(&mut self) -> Result<(String, Span), ParseError> {
        let t = self.advance();
        match t.kind {
            TokenKind::Ident(s) | TokenKind::QIdent(s) => Ok((s, t.span)),
            TokenKind::Keyword(_) => {
                let raw = self
                    .src
                    .get(t.span.start..t.span.end)
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                Ok((raw, t.span))
            }
            _ => Err(ParseError {
                message: "期望名字".to_owned(),
                span: t.span,
            }),
        }
    }

    /// **一个"名字"**（`DCL语句设计` §2.1：`名 := 标识符 | Str`——两者等价，
    /// 照 PG 的 `CREATE DATABASE foo` / `'foo'` 同形）。
    fn name_text(&mut self, what: &str) -> Result<(String, Span), ParseError> {
        let t = self.advance();
        match t.kind {
            TokenKind::Ident(name) => Ok((name, t.span)),
            TokenKind::Str(bytes) => match String::from_utf8(bytes) {
                Ok(name) => Ok((name, t.span)),
                Err(_) => Err(ParseError {
                    message: format!("{what}必须是有效的 UTF-8（标识符或字符串）"),
                    span: t.span,
                }),
            },
            _ => Err(ParseError {
                message: format!("期望{what}：标识符或字符串"),
                span: t.span,
            }),
        }
    }

    /// **工作区引用**（`DCL语句设计` §1.1）：`整数 | 名`（`名 := 标识符 | Str`；
    /// **名字实例内唯一 ⇒ 没有 `FOR USER` 限定**——操作者/属主从会话来，不从引用位来）。
    ///
    /// 解析器**只认形态**：不做名字查找、不判唯一性（绑定期的事）。
    fn work_ref(&mut self) -> Result<WorkRef, ParseError> {
        let t = self.advance();
        match t.kind {
            TokenKind::Number(text) => {
                let id = text.parse::<u64>().map_err(|_| ParseError {
                    message: format!("工作区 id 必须是整数：`{text}`"),
                    span: t.span,
                })?;
                Ok(WorkRef {
                    id: Some(id),
                    name: None,
                    location: t.span,
                })
            }
            TokenKind::Ident(name) => Ok(WorkRef {
                id: None,
                name: Some(name.into_bytes()),
                location: t.span,
            }),
            TokenKind::Str(name) => Ok(WorkRef {
                id: None,
                name: Some(name),
                location: t.span,
            }),
            _ => Err(ParseError {
                message: "期望工作区**引用**：id（整数）或名字（标识符 / 字符串）".to_owned(),
                span: t.span,
            }),
        }
    }

    /// **文件系统引用**（§1.1）：`整数（池槽位） | 名（文件系统名）`。
    ///
    /// **路径不是引用位**：`USING '<路径>'` 只在 `CREATE FILESYSTEM` 出现。
    fn fs_ref(&mut self) -> Result<FsRef, ParseError> {
        let t = self.advance();
        match t.kind {
            TokenKind::Number(text) => {
                let slot = text.parse::<u32>().map_err(|_| ParseError {
                    message: format!("文件系统槽位必须是整数：`{text}`"),
                    span: t.span,
                })?;
                Ok(FsRef {
                    slot: Some(slot),
                    name: None,
                    location: t.span,
                })
            }
            TokenKind::Ident(name) => Ok(FsRef {
                slot: None,
                name: Some(name.into_bytes()),
                location: t.span,
            }),
            TokenKind::Str(name) => Ok(FsRef {
                slot: None,
                name: Some(name),
                location: t.span,
            }),
            _ => Err(ParseError {
                message: "期望文件系统**引用**：槽位（整数）或文件系统名（标识符 / 字符串）"
                    .to_owned(),
                span: t.span,
            }),
        }
    }

    fn expect_string(&mut self) -> Result<(Vec<u8>, Span), ParseError> {
        let t = self.advance();
        match t.kind {
            TokenKind::Str(s) => Ok((s, t.span)),
            _ => Err(ParseError {
                message: "期望字符串字面量".to_owned(),
                span: t.span,
            }),
        }
    }

    fn expect_number_text(&mut self) -> Result<(String, Span), ParseError> {
        let t = self.advance();
        match t.kind {
            TokenKind::Number(s) => Ok((s, t.span)),
            _ => Err(ParseError {
                message: "期望数字".to_owned(),
                span: t.span,
            }),
        }
    }

    // ── 语句（PG 的 stmt 层）──────────────────────────────

    fn statement(&mut self) -> Result<Stmt, ParseError> {
        let t = self.peek().clone();
        match t.kind {
            TokenKind::Keyword(Keyword::Show) => {
                let start = self.advance().span;
                let table = self.advance();
                match &table.kind {
                    TokenKind::Ident(name) if name == "tables" => {
                        Ok(Stmt::ShowTables(start.merge(table.span)))
                    }
                    TokenKind::Ident(name) if name == "graphs" => {
                        Ok(Stmt::ShowGraphs(start.merge(table.span)))
                    }
                    TokenKind::Ident(name) if name == "fulltext" => {
                        self.expect_kw(Keyword::Graph)?;
                        self.expect_graph_word("indexes")?;
                        self.expect_kw(Keyword::On)?;
                        let graph = self.col_id()?.0;
                        Ok(Stmt::GraphIndex(GraphIndexStmt {
                            graph,
                            name: None,
                            action: GraphIndexAction::FulltextShow,
                            location: start,
                        }))
                    }
                    TokenKind::Keyword(Keyword::Graph) => {
                        self.expect_graph_word("indexes")?;
                        self.expect_kw(Keyword::On)?;
                        let graph = self.col_id()?.0;
                        Ok(Stmt::GraphIndex(GraphIndexStmt {
                            graph,
                            name: None,
                            action: GraphIndexAction::Show,
                            location: start,
                        }))
                    }
                    _ => Err(ParseError {
                        message: "SHOW 支持 TABLES、GRAPHS 或 GRAPH INDEXES ON graph".into(),
                        span: table.span,
                    }),
                }
            }
            TokenKind::Ident(ref name) if name == "cypher" => {
                let location = self.advance().span;
                self.cypher_stmt(location, false)
            }
            TokenKind::Ident(ref name) if name == "profile" => {
                let location = self.advance().span;
                self.expect_graph_word("cypher")?;
                self.cypher_stmt(location, true)
            }
            TokenKind::Ident(ref name) if name == "search" => {
                let location = self.advance().span;
                self.expect_graph_word("fulltext")?;
                self.expect_kw(Keyword::Graph)?;
                self.expect_kw(Keyword::Index)?;
                let name = self.col_id()?.0;
                self.expect_kw(Keyword::On)?;
                let graph = self.col_id()?.0;
                self.expect_kw(Keyword::For)?;
                let query = self.utf8_string()?;
                let options = self.fulltext_options()?;
                Ok(Stmt::GraphIndex(GraphIndexStmt {
                    graph,
                    name: Some(name),
                    action: GraphIndexAction::FulltextSearch { query, options },
                    location,
                }))
            }
            TokenKind::Keyword(Keyword::Select) | TokenKind::Keyword(Keyword::Values) => {
                Ok(Stmt::Select(self.select_no_parens()?))
            }
            TokenKind::Keyword(Keyword::Insert) => Ok(Stmt::Insert(self.insert_stmt()?)),
            TokenKind::Keyword(Keyword::Update) => Ok(Stmt::Update(self.update_stmt()?)),
            TokenKind::Keyword(Keyword::Delete) => Ok(Stmt::Delete(self.delete_stmt()?)),
            TokenKind::Keyword(Keyword::Create) => self.create_stmt(),
            TokenKind::Keyword(Keyword::Drop) => self.drop_stmt(),
            TokenKind::Keyword(Keyword::Alter) => self.alter_stmt(),
            TokenKind::Keyword(Keyword::Begin) => {
                let location = self.advance().span;
                Ok(Stmt::Transaction(TransactionStmt {
                    kind: TransactionStmtKind::Begin,
                    location,
                }))
            }
            TokenKind::Keyword(Keyword::Commit) => {
                let location = self.advance().span;
                Ok(Stmt::Transaction(TransactionStmt {
                    kind: TransactionStmtKind::Commit,
                    location,
                }))
            }
            TokenKind::Keyword(Keyword::Rollback) => {
                let location = self.advance().span;
                Ok(Stmt::Transaction(TransactionStmt {
                    kind: TransactionStmtKind::Rollback,
                    location,
                }))
            }
            TokenKind::Keyword(Keyword::With) => Err(ParseError {
                message: "不支持该构造：`WITH` / CTE / 递归（REQ-SQL-006）".to_owned(),
                span: t.span,
            }),
            TokenKind::Keyword(Keyword::Savepoint) => Err(ParseError {
                message: "不支持 `SAVEPOINT`（REQ-SQL-006）".to_owned(),
                span: t.span,
            }),
            TokenKind::Keyword(Keyword::Truncate) => Err(ParseError {
                message: "不支持 `TRUNCATE`（REQ-SQL-006）".to_owned(),
                span: t.span,
            }),
            _ => Err(ParseError {
                message: "不是一条可识别的语句".to_owned(),
                span: t.span,
            }),
        }
    }

    fn expect_graph_word(&mut self, word: &str) -> Result<(), ParseError> {
        let token = self.advance();
        match token.kind {
            TokenKind::Ident(value) if value == word => Ok(()),
            _ => Err(ParseError {
                message: format!("expected {word}"),
                span: token.span,
            }),
        }
    }
    fn cypher_stmt(&mut self, location: Span, profile: bool) -> Result<Stmt, ParseError> {
        let graph = self.col_id()?.0;
        let query = self.expect_string()?.0;
        let parameters = if matches!(&self.peek().kind,TokenKind::Ident(n) if n=="parameters") {
            self.advance();
            self.expect_string()?.0
        } else {
            b"{}".to_vec()
        };
        let budgets = if matches!(&self.peek().kind,TokenKind::Ident(n) if n=="budgets") {
            self.advance();
            self.expect_string()?.0
        } else {
            b"{}".to_vec()
        };
        Ok(Stmt::Cypher(CypherStmt {
            graph,
            profile,
            query: String::from_utf8(query).map_err(|_| self.err_here("Cypher must be UTF-8"))?,
            parameters: String::from_utf8(parameters)
                .map_err(|_| self.err_here("parameters must be UTF-8"))?,
            budgets: String::from_utf8(budgets)
                .map_err(|_| self.err_here("budgets must be UTF-8"))?,
            location,
        }))
    }
    fn create_graph_index(&mut self, location: Span, unique: bool) -> Result<Stmt, ParseError> {
        let name = self.col_id()?.0;
        self.expect_kw(Keyword::On)?;
        let graph = self.col_id()?.0;
        let entity = self.col_id()?.0;
        if entity != "nodes" && entity != "relationships" {
            return Err(self.err_here("graph index needs NODES or RELATIONSHIPS"));
        }
        let label = if matches!(&self.peek().kind,TokenKind::Ident(n) if n=="label" || n=="type") {
            let marker = self.col_id()?.0;
            if (entity == "nodes" && marker != "label")
                || (entity == "relationships" && marker != "type")
            {
                return Err(self.err_here("NODES uses LABEL; RELATIONSHIPS uses TYPE"));
            }
            Some(self.col_id()?.0)
        } else {
            None
        };
        self.expect_punct(Punct::LParen)?;
        let mut fields = vec![];
        if !self.eat_punct(Punct::RParen) {
            loop {
                let mut path = vec![self.col_id()?.0];
                while self.eat_punct(Punct::Dot) {
                    path.push(self.col_id()?.0);
                }
                fields.push(path);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_punct(Punct::RParen)?;
        }
        Ok(Stmt::GraphIndex(GraphIndexStmt {
            graph,
            name: Some(name),
            action: GraphIndexAction::Create {
                entity,
                label,
                fields,
                unique,
            },
            location,
        }))
    }

    fn utf8_string(&mut self) -> Result<String, ParseError> {
        String::from_utf8(self.expect_string()?.0)
            .map_err(|_| self.err_here("full-text string must be UTF-8"))
    }
    fn fulltext_options(&mut self) -> Result<String, ParseError> {
        if matches!(&self.peek().kind,TokenKind::Ident(n) if n=="options") {
            self.advance();
            self.utf8_string()
        } else {
            Ok("{}".into())
        }
    }
    fn create_fulltext_graph_index(&mut self, location: Span) -> Result<Stmt, ParseError> {
        self.expect_kw(Keyword::Graph)?;
        self.expect_kw(Keyword::Index)?;
        let name = self.col_id()?.0;
        self.expect_kw(Keyword::On)?;
        let graph = self.col_id()?.0;
        let entity = self.col_id()?.0;
        if entity != "nodes" && entity != "relationships" {
            return Err(self.err_here("full-text needs NODES or RELATIONSHIPS"));
        }
        let mut labels = vec![];
        if matches!(&self.peek().kind,TokenKind::Ident(n) if n=="label" || n=="type") {
            let marker = self.col_id()?.0;
            if (entity == "nodes" && marker != "label")
                || (entity == "relationships" && marker != "type")
            {
                return Err(self.err_here("NODES uses LABEL; RELATIONSHIPS uses TYPE"));
            }
            if self.eat_punct(Punct::LParen) {
                loop {
                    labels.push(self.col_id()?.0);
                    if !self.eat_punct(Punct::Comma) {
                        break;
                    }
                }
                self.expect_punct(Punct::RParen)?;
            } else {
                labels.push(self.col_id()?.0);
            }
        }
        self.expect_punct(Punct::LParen)?;
        let mut fields = vec![];
        loop {
            let mut path = vec![GraphTextPathPart::Key(self.col_id()?.0)];
            loop {
                if self.eat_punct(Punct::Dot) {
                    path.push(GraphTextPathPart::Key(self.col_id()?.0));
                } else if self.eat_punct(Punct::LBracket) {
                    let value = self.expect_number_text()?.0.parse().map_err(|_| {
                        self.err_here("JSON array position must be an unsigned integer")
                    })?;
                    path.push(GraphTextPathPart::Index(value));
                    self.expect_punct(Punct::RBracket)?;
                } else {
                    break;
                }
            }
            fields.push(path);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        let options = self.fulltext_options()?;
        Ok(Stmt::GraphIndex(GraphIndexStmt {
            graph,
            name: Some(name),
            action: GraphIndexAction::FulltextCreate {
                entity,
                labels,
                fields,
                options,
            },
            location,
        }))
    }

    fn create_stmt(&mut self) -> Result<Stmt, ParseError> {
        let location = self.expect_kw(Keyword::Create)?.span;
        if matches!(&self.peek().kind,TokenKind::Ident(n) if n=="fulltext") {
            self.advance();
            return self.create_fulltext_graph_index(location);
        }
        if self.eat_kw(Keyword::Table) {
            return Ok(Stmt::CreateTable(self.create_table(location)?));
        }
        let unique = self.eat_kw(Keyword::Unique);
        if self.eat_kw(Keyword::Index) {
            return Ok(Stmt::Index(self.index_stmt(location, unique)?));
        }
        if self.eat_kw(Keyword::Graph) {
            if self.eat_kw(Keyword::Index) {
                return self.create_graph_index(location, unique);
            }
            if unique {
                return Err(self.err_here("UNIQUE requires GRAPH INDEX"));
            }
            let (relname, iloc) = self.col_id()?;
            return Ok(Stmt::CreateGraph(CreateGraphStmt {
                graph: RangeVar {
                    relname,
                    alias: None,
                    location: iloc,
                },
                location,
            }));
        }
        if unique {
            return Err(self.err_here("UNIQUE requires INDEX or GRAPH INDEX"));
        }
        if self.eat_kw(Keyword::Workspace) {
            return Ok(Stmt::CreateWorkspace(self.create_workspace(location)?));
        }
        if self.eat_kw(Keyword::Filesystem) {
            let (name, _) = self.name_text("文件系统名")?;
            self.expect_kw(Keyword::Using)?;
            let path = self.expect_string()?.0;
            return Ok(Stmt::CreateFilesystem(CreateFilesystemStmt {
                name,
                path,
                location,
            }));
        }
        if self.eat_kw(Keyword::User) {
            return Ok(Stmt::CreateUser(self.create_user(location)?));
        }
        Err(self.err_here(
            "`CREATE` 之后只能是 TABLE / [UNIQUE] INDEX / GRAPH / WORKSPACE / FILESYSTEM / USER",
        ))
    }

    fn drop_stmt(&mut self) -> Result<Stmt, ParseError> {
        let location = self.expect_kw(Keyword::Drop)?.span;
        if matches!(&self.peek().kind,TokenKind::Ident(n) if n=="fulltext") {
            self.advance();
            self.expect_kw(Keyword::Graph)?;
            self.expect_kw(Keyword::Index)?;
            let name = self.col_id()?.0;
            self.expect_kw(Keyword::On)?;
            let graph = self.col_id()?.0;
            return Ok(Stmt::GraphIndex(GraphIndexStmt {
                graph,
                name: Some(name),
                action: GraphIndexAction::FulltextDrop,
                location,
            }));
        }
        if matches!(self.peek().kind, TokenKind::Keyword(Keyword::Graph))
            && matches!(
                self.tokens.get(self.at + 1).map(|t| &t.kind),
                Some(TokenKind::Keyword(Keyword::Index))
            )
        {
            self.advance();
            self.advance();
            let name = self.col_id()?.0;
            self.expect_kw(Keyword::On)?;
            let graph = self.col_id()?.0;
            return Ok(Stmt::GraphIndex(GraphIndexStmt {
                graph,
                name: Some(name),
                action: GraphIndexAction::Drop,
                location,
            }));
        }
        if self.eat_kw(Keyword::User) {
            let (name, _) = self.name_text("主体名")?;
            let cascade = self.eat_kw(Keyword::Cascade);
            return Ok(Stmt::DropUser(DropUserStmt {
                name,
                cascade,
                location,
            }));
        }
        if self.eat_kw(Keyword::Filesystem) {
            return Ok(Stmt::DropFilesystem(DropFilesystemStmt {
                fs: self.fs_ref()?,
                location,
            }));
        }
        let remove_type = if self.eat_kw(Keyword::Table) {
            ObjectType::Table
        } else if self.eat_kw(Keyword::Index) {
            ObjectType::Index
        } else if self.eat_kw(Keyword::Graph) {
            ObjectType::Graph
        } else if self.eat_kw(Keyword::Workspace) {
            ObjectType::Workspace
        } else {
            return Err(self.err_here(
                "`DROP` 之后只能是 TABLE / INDEX / GRAPH / WORKSPACE / FILESYSTEM / USER",
            ));
        };
        let missing_ok = false;
        let mut objects = Vec::new();
        let mut workspaces = Vec::new();
        loop {
            if remove_type == ObjectType::Workspace {
                workspaces.push(self.work_ref()?);
            } else {
                let (relname, iloc) = self.col_id()?;
                objects.push(RangeVar {
                    relname,
                    alias: None,
                    location: iloc,
                });
            }
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        Ok(Stmt::Drop(DropStmt {
            objects,
            workspaces,
            remove_type,
            missing_ok,
            location,
        }))
    }

    fn alter_stmt(&mut self) -> Result<Stmt, ParseError> {
        let location = self.expect_kw(Keyword::Alter)?.span;
        if matches!(&self.peek().kind,TokenKind::Ident(n) if n=="fulltext") {
            self.advance();
            self.expect_kw(Keyword::Graph)?;
            self.expect_kw(Keyword::Index)?;
            let name = self.col_id()?.0;
            self.expect_kw(Keyword::On)?;
            let graph = self.col_id()?.0;
            let action = self.col_id()?.0;
            if !["sync", "wait", "rebuild", "pause", "resume", "options"].contains(&action.as_str())
            {
                return Err(self.err_here(
                    "full-text maintenance needs SYNC, WAIT, REBUILD, PAUSE, RESUME or OPTIONS",
                ));
            }
            return Ok(Stmt::GraphIndex(GraphIndexStmt {
                graph,
                name: Some(name),
                action: match action.as_str() {
                    "sync" => GraphIndexAction::FulltextSync,
                    "wait" => GraphIndexAction::FulltextWait {
                        options: if matches!(&self.peek().kind, TokenKind::Ident(word) if word == "options")
                        {
                            self.advance();
                            self.utf8_string()?
                        } else {
                            "{}".into()
                        },
                    },
                    "rebuild" => GraphIndexAction::FulltextRebuild,
                    "pause" => GraphIndexAction::FulltextPause,
                    "resume" => GraphIndexAction::FulltextResume,
                    "options" => GraphIndexAction::FulltextConfigure {
                        options: self.utf8_string()?,
                    },
                    _ => unreachable!("validated action"),
                },
                location,
            }));
        }
        if self.eat_kw(Keyword::Graph) {
            if !self.eat_kw(Keyword::Index) {
                let graph = self.col_id()?.0;
                if matches!(&self.peek().kind, TokenKind::Ident(word) if word == "upgrade") {
                    self.advance();
                    self.expect_graph_word("storage")?;
                    return Ok(Stmt::GraphIndex(GraphIndexStmt {
                        graph,
                        name: None,
                        action: GraphIndexAction::UpgradeStorage,
                        location,
                    }));
                }
                self.expect_graph_word("rebuild")?;
                let action = if matches!(&self.peek().kind, TokenKind::Ident(word) if word == "storage")
                {
                    self.advance();
                    GraphIndexAction::RebuildStorage
                } else {
                    self.expect_graph_word("access")?;
                    GraphIndexAction::RebuildAccess
                };
                return Ok(Stmt::GraphIndex(GraphIndexStmt {
                    graph,
                    name: None,
                    action,
                    location,
                }));
            }
            let name = self.col_id()?.0;
            self.expect_kw(Keyword::On)?;
            let graph = self.col_id()?.0;
            self.expect_graph_word("rebuild")?;
            return Ok(Stmt::GraphIndex(GraphIndexStmt {
                graph,
                name: Some(name),
                action: GraphIndexAction::Rebuild,
                location,
            }));
        }
        if self.eat_kw(Keyword::Session) {
            return self.variable_set(location);
        }
        if self.eat_kw(Keyword::System) {
            return Err(self.err_here(
                "`ALTER SYSTEM …` 不再提供——文件系统语句是 `CREATE / ALTER / DROP FILESYSTEM` \
                 三件套（`DCL语句设计` v0.2 的 F 组：与 WORKSPACE / USER 同构）",
            ));
        }
        if self.eat_kw(Keyword::Database) {
            return self.alter_database(location);
        }
        if self.eat_kw(Keyword::Filesystem) {
            let fs = self.fs_ref()?;
            self.expect_kw(Keyword::Set)?;
            self.expect_kw(Keyword::Allocate)?;
            self.expect_punct(Punct::Eq)?;
            let allocate = if self.eat_kw(Keyword::On) {
                true
            } else if self.eat_kw(Keyword::Off) {
                false
            } else {
                return Err(self.err_here("`ALLOCATE` 的值只能是 `ON` 或 `OFF`"));
            };
            return Ok(Stmt::AlterFilesystem(AlterFilesystemStmt {
                fs,
                allocate,
                location,
            }));
        }
        if self.eat_kw(Keyword::User) {
            return self.alter_user(location);
        }
        if !self.eat_kw(Keyword::Workspace) {
            return Err(self.err_here(
                "不支持该构造：`ALTER` 只提供 WORKSPACE / USER / FILESYSTEM / SESSION / DATABASE \
                 五类（`DCL语句设计` v0.2 §1 的闭集）",
            ));
        }
        let workspace = self.work_ref()?;
        let action = if self.eat_kw(Keyword::Open) {
            self.expect_kw(Keyword::Read)?;
            if self.eat_kw(Keyword::Only) {
                AlterWorkspaceAction::Open(crate::ast::WorkspaceOpenMode::ReadOnly)
            } else {
                self.expect_kw(Keyword::Write)?;
                AlterWorkspaceAction::Open(if self.eat_kw(Keyword::Force) {
                    crate::ast::WorkspaceOpenMode::ReadWriteForce
                } else {
                    crate::ast::WorkspaceOpenMode::ReadWrite
                })
            }
        } else if self.eat_kw(Keyword::Verify) {
            self.expect_kw(Keyword::Recovery)?;
            let scope = if self.eat_kw(Keyword::Page) {
                let file_id = self.unsigned_integer("文件号")?;
                let block_id = self.unsigned_integer("块号")?;
                crate::ast::RecoveryVerifyScope::Page {
                    file_id: file_id
                        .try_into()
                        .map_err(|_| self.err_here("恢复验证文件号超出 u16 范围"))?,
                    block_id: block_id
                        .try_into()
                        .map_err(|_| self.err_here("恢复验证块号超出 u32 范围"))?,
                }
            } else if self.eat_kw(Keyword::Object) {
                let object_id = self.unsigned_integer("对象号")?;
                crate::ast::RecoveryVerifyScope::Object {
                    object_id: object_id
                        .try_into()
                        .map_err(|_| self.err_here("恢复验证对象号超出 u32 范围"))?,
                }
            } else {
                return Err(self.err_here("VERIFY RECOVERY 之后只能是 PAGE 或 OBJECT"));
            };
            AlterWorkspaceAction::VerifyRecovery(scope)
        } else if self.eat_kw(Keyword::Add) {
            self.expect_kw(Keyword::Filesystem)?;
            let fs = self.fs_ref()?;
            // `ADD FILESYSTEM … [QUOTA <量> ON FILESYSTEM <fs_ref>]`（W3）
            let quota = if self.at_kw(Keyword::Quota) {
                Some(self.quota_on_fs()?)
            } else {
                None
            };
            AlterWorkspaceAction::AddFilesystem { fs, quota }
        } else if self.eat_kw(Keyword::To) {
            self.expect_kw(Keyword::Template)?;
            let (name, _) = self.name_text("模板名")?;
            AlterWorkspaceAction::ToTemplate {
                name: name.into_bytes(),
            }
        } else {
            self.expect_kw(Keyword::Set)?;
            if self.eat_kw(Keyword::Default) {
                self.expect_kw(Keyword::Filesystem)?;
                AlterWorkspaceAction::SetDefaultFilesystem { fs: self.fs_ref()? }
            } else if self.eat_kw(Keyword::Name) {
                self.expect_punct(Punct::Eq)?;
                let (name, _) = self.name_text("新名字")?;
                AlterWorkspaceAction::SetName(name.into_bytes())
            } else if self.eat_kw(Keyword::Quota) {
                AlterWorkspaceAction::SetQuota(self.quota_list()?)
            } else {
                return Err(self.err_here(
                    "`ALTER WORKSPACE` 只提供 `OPEN READ ONLY|READ WRITE [FORCE]` / \
                     `VERIFY RECOVERY PAGE|OBJECT` / \
                     `ADD FILESYSTEM` / `SET DEFAULT FILESYSTEM` / \
                     `SET NAME = …` / `SET QUOTA (…)` / `TO TEMPLATE …`（W3–W7——闭集）",
                ));
            }
        };
        Ok(Stmt::AlterWorkspace(AlterWorkspaceStmt {
            workspace,
            action,
            location,
        }))
    }

    fn unsigned_integer(&mut self, what: &str) -> Result<u64, ParseError> {
        let token = self.advance();
        match token.kind {
            TokenKind::Number(text) if !text.contains(['.', 'e', 'E']) => {
                text.parse::<u64>().map_err(|_| ParseError {
                    message: format!("{what}必须是无符号整数：`{text}`"),
                    span: token.span,
                })
            }
            _ => Err(ParseError {
                message: format!("{what}必须是无符号整数"),
                span: token.span,
            }),
        }
    }

    /// **`ALTER SESSION SET/CLEAR <参数>`**（S 组；白名单在 ② 判）。
    fn variable_set(&mut self, location: Span) -> Result<Stmt, ParseError> {
        let kind = if self.eat_kw(Keyword::Set) {
            VariableSetKind::Set
        } else if self.eat_kw(Keyword::Clear) {
            VariableSetKind::Clear
        } else {
            return Err(self.err_here("`ALTER SESSION` 之后只能是 `SET` 或 `CLEAR`"));
        };
        let (name, _) = self.col_id()?;
        let mut args = Vec::new();
        if kind == VariableSetKind::Set {
            self.expect_punct(Punct::Eq)?;
            let t = self.advance();
            let value = match t.kind {
                TokenKind::Number(text) => Some(classify_number(&text)),
                TokenKind::Str(text) => Some(ConstValue::Str(text)),
                _ => {
                    return Err(ParseError {
                        message: "会话参数值：整数 / 浮点 / 字符串（内存量照 PG：`'64MB'`）"
                            .to_owned(),
                        span: t.span,
                    })
                }
            };
            args.push(AConst {
                value,
                location: t.span,
            });
        }
        Ok(Stmt::VariableSet(VariableSetStmt {
            kind,
            name,
            args,
            location,
        }))
    }

    /// **`ALTER DATABASE …`**（T 组：模板；**不带库名**——本库实例即一个"库"，记档）。
    ///
    /// **克隆与原地转模板都不在这里**（v0.2）：克隆是 `CREATE WORKSPACE … FROM TEMPLATE`（W2），
    /// 原地固化是 `ALTER WORKSPACE … TO TEMPLATE`（W7）——**一件事不设两个入口**。
    fn alter_database(&mut self, location: Span) -> Result<Stmt, ParseError> {
        let action = if self.eat_kw(Keyword::Add) {
            self.expect_kw(Keyword::Template)?;
            let (name, _) = self.name_text("模板名")?;
            self.expect_kw(Keyword::From)?;
            let from = self.work_ref()?;
            let graph_data = if self.eat_kw(Keyword::With) {
                self.expect_kw(Keyword::Graph)?;
                self.expect_graph_word("data")?;
                true
            } else {
                false
            };
            AlterDatabaseAction::AddTemplate {
                name: name.into_bytes(),
                from,
                graph_data,
            }
        } else if self.eat_kw(Keyword::Drop) {
            self.expect_kw(Keyword::Template)?;
            let (name, _) = self.name_text("模板名")?;
            AlterDatabaseAction::DropTemplate {
                name: name.into_bytes(),
            }
        } else if self.eat_kw(Keyword::Clone) {
            return Err(self.err_here(
                "`ALTER DATABASE CLONE WORKSPACE …` 不再提供——克隆由**目标**发起：\
                 `CREATE WORKSPACE '<新名>' FROM TEMPLATE '<模板名>'`（W2）",
            ));
        } else if self.eat_kw(Keyword::Alter) {
            return Err(self.err_here(
                "`ALTER DATABASE ALTER WORKSPACE … TO TEMPLATE` 不再提供——原地转模板是\
                 `ALTER WORKSPACE <引用> TO TEMPLATE '<名>'`（W7）",
            ));
        } else {
            return Err(self.err_here(
                "`ALTER DATABASE` 只提供 ADD TEMPLATE / DROP TEMPLATE 两个动作（T1–T2——闭集）",
            ));
        };
        Ok(Stmt::AlterDatabase(AlterDatabaseStmt { action, location }))
    }

    /// **盘级配额项**（`QUOTA <量> ON FILESYSTEM <fs_ref>`；F/W/U 三组共用）。
    ///
    /// **`量` 有两种**（BNF）：整数（字节）或 `UNLIMITED`。
    fn quota_on_fs(&mut self) -> Result<FsQuota, ParseError> {
        let location = self.expect_kw(Keyword::Quota)?.span;
        let amount = self.quota_amount()?;
        self.expect_kw(Keyword::On)?;
        self.expect_kw(Keyword::Filesystem)?;
        Ok(FsQuota {
            fs: self.fs_ref()?,
            amount,
            location,
        })
    }

    /// `量 := 整数 | UNLIMITED`。
    fn quota_amount(&mut self) -> Result<QuotaAmount, ParseError> {
        let t = self.advance();
        match t.kind {
            TokenKind::Number(text) => {
                text.parse::<u64>()
                    .map(QuotaAmount::Bytes)
                    .map_err(|_| ParseError {
                        message: format!("配额必须是 0..2^64-1 的整数（字节）：`{text}`"),
                        span: t.span,
                    })
            }
            TokenKind::Keyword(Keyword::Unlimited) => Ok(QuotaAmount::Unlimited),
            _ => Err(ParseError {
                message: "配额只能是整数（字节）或 `UNLIMITED`".to_owned(),
                span: t.span,
            }),
        }
    }

    /// **配额项**（`QuotaItem` 的产生式只有四个键——闭集；见 §2.3）。
    fn quota_list(&mut self) -> Result<Vec<DefElem>, ParseError> {
        self.expect_punct(Punct::LParen)?;
        let mut items = Vec::new();
        loop {
            let (defname, location) = self.col_id()?;
            if !matches!(defname.as_str(), "data" | "undo" | "temp" | "asset") {
                return Err(ParseError {
                    message: format!(
                        "配额键 `{defname}` 不在闭集内（只有 data / undo / temp / asset）"
                    ),
                    span: location,
                });
            }
            self.expect_punct(Punct::Eq)?;
            let (arg, _) = self.def_value()?;
            items.push(DefElem {
                defname,
                arg,
                location,
            });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        Ok(items)
    }

    /// 一个 `DefElem` 的值（`def_elem_list` 与 `quota_list` 共用）。
    fn def_value(&mut self) -> Result<(DefElemArg, Span), ParseError> {
        let t = self.advance();
        let arg = match t.kind {
            TokenKind::Number(s) => DefElemArg::Const(AConst {
                value: Some(classify_number(&s)),
                location: t.span,
            }),
            TokenKind::Str(s) => DefElemArg::Const(AConst {
                value: Some(ConstValue::Str(s)),
                location: t.span,
            }),
            TokenKind::Ident(s) | TokenKind::QIdent(s) => DefElemArg::Ident(s),
            TokenKind::Keyword(_) => {
                let raw = self
                    .src
                    .get(t.span.start..t.span.end)
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                DefElemArg::Ident(raw)
            }
            _ => {
                return Err(ParseError {
                    message: "选项值形态非法".to_owned(),
                    span: t.span,
                })
            }
        };
        Ok((arg, t.span))
    }

    // ── DDL 体（PG 的产生式删减）──────────────────────────

    /// `CREATE TABLE relation '(' table_elts ')' [WITH '(' options ')']`
    /// （PG `CreateStmt`；去 OptInherit/OptPartitionSpec/TableAccessMethod/…）。
    fn create_table(&mut self, location: Span) -> Result<CreateStmt, ParseError> {
        let relation = self.relation_expr()?;
        self.expect_punct(Punct::LParen)?;
        let mut table_elts = Vec::new();
        loop {
            table_elts.push(self.column_def()?);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        let options = if self.eat_kw(Keyword::With) {
            self.def_elem_list()?
        } else {
            Vec::new()
        };
        Ok(CreateStmt {
            relation,
            table_elts,
            options,
            location,
        })
    }

    /// `columnDef`（PG 的裁剪：无约束/默认/存储/压缩）。
    fn column_def(&mut self) -> Result<ColumnDef, ParseError> {
        let (colname, location) = self.col_id()?;
        let type_name = self.type_name()?;
        let is_not_null = if self.eat_kw(Keyword::Not) {
            self.expect_kw(Keyword::Null)?;
            true
        } else {
            false
        };
        // 列约束清单外的一切（DEFAULT/PK/FK/CHECK）在语法层就没有产生式。
        for bad in [
            Keyword::Default,
            Keyword::Primary,
            Keyword::Key,
            Keyword::References,
            Keyword::Check,
            Keyword::Unique,
        ] {
            if self.at_kw(bad) {
                return Err(ParseError {
                    message: format!(
                        "不支持该构造：列约束 `{}`（REQ-SQL-006；主键效果由 NOT NULL + 唯一索引达成）",
                        bad.text()
                    ),
                    span: self.peek().span,
                });
            }
        }
        Ok(ColumnDef {
            colname,
            type_name,
            is_not_null,
            location,
        })
    }

    /// 类型名（PG `GenericType: type_function_name opt_type_modifiers`）。
    fn type_name(&mut self) -> Result<TypeName, ParseError> {
        let (name, location) = self.col_id()?;
        let mut typmods = Vec::new();
        if self.at_punct(Punct::LParen) {
            self.advance();
            loop {
                let (n, _) = self.expect_number_text()?;
                typmods.push(n);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_punct(Punct::RParen)?;
        }
        Ok(TypeName {
            name,
            typmods,
            location,
        })
    }

    /// `'(' def_elem (',' def_elem)* ')'`（PG 的 `reloption_list` 删减）。
    fn def_elem_list(&mut self) -> Result<Vec<DefElem>, ParseError> {
        self.expect_punct(Punct::LParen)?;
        let mut items = Vec::new();
        loop {
            let (defname, location) = self.col_id()?;
            self.expect_punct(Punct::Eq)?;
            let (arg, _) = self.def_value()?;
            items.push(DefElem {
                defname,
                arg,
                location,
            });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        Ok(items)
    }

    /// `CREATE [UNIQUE] INDEX idxname ON relation [VERTEX|EDGE] '(' index_params ')'`
    /// （PG `IndexStmt` 的裁剪 + 本库的图目标扩展）。
    fn index_stmt(&mut self, location: Span, unique: bool) -> Result<IndexStmt, ParseError> {
        let (idxname, _) = self.col_id()?;
        self.expect_kw(Keyword::On)?;
        let relation = self.relation_expr()?;
        let target_kind = if self.eat_kw(Keyword::Vertex) {
            IndexTargetKind::Vertex
        } else if self.eat_kw(Keyword::Edge) {
            IndexTargetKind::Edge
        } else {
            IndexTargetKind::Table
        };
        self.expect_punct(Punct::LParen)?;
        let mut index_params = Vec::new();
        loop {
            index_params.push(self.index_elem()?);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        Ok(IndexStmt {
            idxname,
            relation,
            target_kind,
            index_params,
            unique,
            location,
        })
    }

    /// `index_elem`（PG 的裁剪：只留 `name`/`expr`）。
    fn index_elem(&mut self) -> Result<IndexElem, ParseError> {
        let location = self.peek().span;
        // 单列名形态 ⇒ name（表达式形态才走全解析）。
        if matches!(self.peek().kind, TokenKind::Ident(_) | TokenKind::QIdent(_))
            && (matches!(self.peek2().kind, TokenKind::Punct(Punct::Comma))
                || matches!(self.peek2().kind, TokenKind::Punct(Punct::RParen)))
        {
            let (name, _) = self.col_id()?;
            return Ok(IndexElem {
                name: Some(name),
                expr: None,
                location,
            });
        }
        let expr = self.expr()?;
        // 纯列引用也归到 `name`（PG 的 `index_elem: ColId` 形态）。
        if let Expr::ColumnRef(cr) = &expr {
            if cr.fields.len() == 1 {
                if let ColumnRefField::Name(n) = &cr.fields[0] {
                    return Ok(IndexElem {
                        name: Some(n.clone()),
                        expr: None,
                        location,
                    });
                }
            }
        }
        Ok(IndexElem {
            name: None,
            expr: Some(Box::new(expr)),
            location,
        })
    }

    /// **`CREATE WORKSPACE <名> [DEFAULT FILESYSTEM <fs_ref>] [FROM TEMPLATE '<名>']
    /// [QUOTA <量> ON FILESYSTEM <fs_ref>]…`**（W1/W2）。
    ///
    /// **建的是无主容器**：属主由 `CREATE USER … USING WORKSPACE` 绑定
    /// （依赖顺序 **FS → WORKSPACE → USER**；旧的 `FOR USER` 形式已删除——
    /// 一条语句只做一件事，`DCL语句设计` v0.2 §0）。
    fn create_workspace(&mut self, location: Span) -> Result<CreateWorkspaceStmt, ParseError> {
        let (name, _) = self.name_text("工作区名")?;
        let mut default_fs = None;
        let mut from_template = None;
        let mut quotas = Vec::new();
        // 三个可选项**任意顺序**（各自最多一次——重复即具名拒绝）。
        loop {
            if self.at_kw(Keyword::Default) {
                self.advance();
                self.expect_kw(Keyword::Filesystem)?;
                if default_fs.is_some() {
                    return Err(self.err_here("`DEFAULT FILESYSTEM` 只能给一次"));
                }
                default_fs = Some(self.fs_ref()?);
            } else if self.at_kw(Keyword::From) {
                self.advance();
                self.expect_kw(Keyword::Template)?;
                if from_template.is_some() {
                    return Err(self.err_here("`FROM TEMPLATE` 只能给一次"));
                }
                from_template = Some(self.expect_string()?.0);
            } else if self.at_kw(Keyword::Quota) {
                quotas.push(self.quota_on_fs()?);
            } else {
                break;
            }
        }
        Ok(CreateWorkspaceStmt {
            name,
            default_fs,
            from_template,
            quotas,
            location,
        })
    }

    /// **`CREATE USER <主体> IDENTIFIED BY '<口令>' USING WORKSPACE <work_ref>`**（U1）。
    ///
    /// **`USING WORKSPACE` 是必选**（有了工作区才能建用户——依赖顺序的落点）。
    fn create_user(&mut self, location: Span) -> Result<CreateUserStmt, ParseError> {
        let (name, _) = self.name_text("主体名")?;
        self.expect_kw(Keyword::Identified)?;
        self.expect_kw(Keyword::By)?;
        let password = self.expect_string()?.0;
        self.expect_kw(Keyword::Using)?;
        self.expect_kw(Keyword::Workspace)?;
        Ok(CreateUserStmt {
            name,
            password,
            using_workspace: self.work_ref()?,
            location,
        })
    }

    /// **`ALTER USER <主体> …`**（U2–U6；五种动作）。
    fn alter_user(&mut self, location: Span) -> Result<Stmt, ParseError> {
        let (name, _) = self.name_text("主体名")?;
        let action = if self.eat_kw(Keyword::Identified) {
            self.expect_kw(Keyword::By)?;
            let new = self.expect_string()?.0;
            if self.eat_kw(Keyword::Replace) {
                // 本人改密（U3；要求旧口令）
                AlterUserAction::ReplacePassword {
                    new,
                    old: self.expect_string()?.0,
                }
            } else {
                // admin 重置（U2；可带 EXPIRE）
                let expire = self.eat_kw(Keyword::Expire);
                AlterUserAction::SetPassword { new, expire }
            }
        } else if self.eat_kw(Keyword::Pause) {
            AlterUserAction::SetPaused(true)
        } else if self.eat_kw(Keyword::Resume) {
            AlterUserAction::SetPaused(false)
        } else if self.eat_kw(Keyword::Using) {
            self.expect_kw(Keyword::Workspace)?;
            AlterUserAction::UsingWorkspace(self.work_ref()?)
        } else if self.eat_kw(Keyword::Drop) {
            self.expect_kw(Keyword::Workspace)?;
            AlterUserAction::DropWorkspace(self.work_ref()?)
        } else {
            return Err(self.err_here(
                "`ALTER USER` 只提供 `IDENTIFIED BY '<口令>' [EXPIRE]`（admin 重置）/ \
                 `IDENTIFIED BY '<新>' REPLACE '<旧>'`（本人改密）/ `PAUSE` / `RESUME` / \
                 `USING WORKSPACE` / `DROP WORKSPACE`（U2–U6——闭集）",
            ));
        };
        Ok(Stmt::AlterUser(AlterUserStmt {
            name,
            action,
            location,
        }))
    }

    /// `relation_expr`（PG 的裁剪：单名 + 可选别名）。
    fn relation_expr(&mut self) -> Result<RangeVar, ParseError> {
        let (relname, location) = self.col_id()?;
        let alias = if self.eat_kw(Keyword::As) {
            let (aliasname, _) = self.col_id()?;
            Some(Alias { aliasname })
        } else if matches!(self.peek().kind, TokenKind::Ident(_)) {
            // 裸别名（`FROM t x`）——但 `JOIN`/`LEFT` 等关键字不算别名。
            let (aliasname, _) = self.col_id()?;
            Some(Alias { aliasname })
        } else {
            None
        };
        Ok(RangeVar {
            relname,
            alias,
            location,
        })
    }

    // ── DML ──────────────────────────────────────────────

    /// `INSERT INTO relation_expr opt_column_list select_stmt`
    /// （**`VALUES` 走 `SelectStmt.values_lists`**——照 PG 的 `insert_rest`）。
    fn insert_stmt(&mut self) -> Result<InsertStmt, ParseError> {
        let location = self.expect_kw(Keyword::Insert)?.span;
        self.expect_kw(Keyword::Into)?;
        let relation = self.relation_expr()?;
        let mut cols = Vec::new();
        if self.at_punct(Punct::LParen) {
            self.advance();
            loop {
                let (name, cloc) = self.col_id()?;
                cols.push(ResTarget {
                    name: Some(name),
                    // 照 PG：列清单的 `val` 是只含名字的 `ColumnRef`（占位）。
                    val: Expr::ColumnRef(ColumnRef {
                        fields: Vec::new(),
                        location: cloc,
                    }),
                    location: cloc,
                });
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_punct(Punct::RParen)?;
        }
        // **来源两种**（PG 的 `insert_rest`）：`VALUES` 与 `SELECT`（含集合运算）。
        // `DEFAULT VALUES` 仍不给（本版没有列默认值，记档）。
        if self.at_kw(Keyword::Default) {
            return Err(self.err_here(
                "不支持 `INSERT … DEFAULT VALUES`（本版列没有默认值——每个 NOT NULL 列都要给）",
            ));
        }
        if !self.at_kw(Keyword::Values) && !self.at_kw(Keyword::Select) {
            return Err(self.err_here("`INSERT` 的来源只能是 `VALUES` 或 `SELECT`"));
        }
        let select_stmt = self.select_no_parens()?;
        Ok(InsertStmt {
            relation,
            cols,
            select_stmt: Some(Box::new(select_stmt)),
            location,
        })
    }

    /// `UPDATE relation_expr SET target_list [WHERE a_expr]`
    /// （PG `UpdateStmt` 的裁剪：无 `FROM`）。
    fn update_stmt(&mut self) -> Result<UpdateStmt, ParseError> {
        let location = self.expect_kw(Keyword::Update)?.span;
        let relation = self.relation_expr()?;
        self.expect_kw(Keyword::Set)?;
        let mut target_list = Vec::new();
        loop {
            let (name, tloc) = self.col_id()?;
            self.expect_punct(Punct::Eq)?;
            let val = self.expr()?;
            target_list.push(ResTarget {
                name: Some(name),
                val,
                location: tloc,
            });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        let where_clause = if self.eat_kw(Keyword::Where) {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        Ok(UpdateStmt {
            relation,
            target_list,
            where_clause,
            location,
        })
    }

    /// `DELETE FROM relation_expr [WHERE a_expr]`（PG `DeleteStmt` 的裁剪）。
    fn delete_stmt(&mut self) -> Result<DeleteStmt, ParseError> {
        let location = self.expect_kw(Keyword::Delete)?.span;
        self.expect_kw(Keyword::From)?;
        let relation = self.relation_expr()?;
        let where_clause = if self.eat_kw(Keyword::Where) {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        Ok(DeleteStmt {
            relation,
            where_clause,
            location,
        })
    }

    // ── SELECT（PG：select_no_parens / simple_select / set ops）──

    /// `select_no_parens`：`select_clause [ORDER BY …] [LIMIT …] [OFFSET …]`
    /// （排序/限行挂**整个查询表达式**的节点上——照 PG）。
    fn select_no_parens(&mut self) -> Result<SelectStmt, ParseError> {
        let mut node = self.simple_select(0)?;
        if self.at_kw(Keyword::Order) {
            self.advance();
            self.expect_kw(Keyword::By)?;
            loop {
                node.sort_clause.push(self.sort_by()?);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
        }
        if self.eat_kw(Keyword::Limit) {
            node.limit_count = Some(Box::new(self.expr()?));
        }
        if self.eat_kw(Keyword::Offset) {
            node.limit_offset = Some(Box::new(self.expr()?));
        }
        Ok(node)
    }

    /// `simple_select`：`select_core (UNION|INTERSECT|EXCEPT [ALL] select_core)*`
    /// ——**优先级照 PG**（`UNION`/`EXCEPT` 同级、`INTERSECT` 更紧；左结合），
    /// 结果用 `op/all/larg/rarg` **左深嵌套**。
    fn simple_select(&mut self, min_prec: u8) -> Result<SelectStmt, ParseError> {
        let mut left = self.select_core()?;
        while let Some((op, prec)) = self.peek_set_op() {
            if prec < min_prec {
                break;
            }
            self.advance(); // 运算词
            let all = self.eat_kw(Keyword::All);
            let right = self.simple_select(prec + 1)?;
            let location = left.location.merge(right.location);
            left = SelectStmt {
                distinct: false,
                target_list: Vec::new(),
                from_clause: Vec::new(),
                where_clause: None,
                group_clause: Vec::new(),
                having_clause: None,
                values_lists: None,
                sort_clause: Vec::new(),
                limit_offset: None,
                limit_count: None,
                op: Some(op),
                all,
                larg: Some(Box::new(left)),
                rarg: Some(Box::new(right)),
                location,
            };
        }
        Ok(left)
    }

    fn peek_set_op(&self) -> Option<(SetOperation, u8)> {
        match self.peek().kind {
            TokenKind::Keyword(Keyword::Union) => Some((SetOperation::Union, 1)),
            TokenKind::Keyword(Keyword::Except) => Some((SetOperation::Except, 1)),
            TokenKind::Keyword(Keyword::Intersect) => Some((SetOperation::Intersect, 2)),
            _ => None,
        }
    }

    /// `select_core`：`SELECT …` 或 `VALUES …`。
    fn select_core(&mut self) -> Result<SelectStmt, ParseError> {
        let location = self.peek().span;
        let mut node = SelectStmt {
            distinct: false,
            target_list: Vec::new(),
            from_clause: Vec::new(),
            where_clause: None,
            group_clause: Vec::new(),
            having_clause: None,
            values_lists: None,
            sort_clause: Vec::new(),
            limit_offset: None,
            limit_count: None,
            op: None,
            all: false,
            larg: None,
            rarg: None,
            location,
        };
        if self.at_kw(Keyword::Values) {
            self.advance();
            node.values_lists = Some(self.values_lists()?);
            return Ok(node);
        }
        self.expect_kw(Keyword::Select)?;
        node.distinct = self.eat_kw(Keyword::Distinct);
        node.target_list = self.target_list()?;
        if self.eat_kw(Keyword::From) {
            node.from_clause = self.parse_from_list()?;
        }
        if self.eat_kw(Keyword::Where) {
            node.where_clause = Some(Box::new(self.expr()?));
        }
        if self.at_kw(Keyword::Group) {
            self.advance();
            self.expect_kw(Keyword::By)?;
            loop {
                node.group_clause.push(self.expr()?);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
        }
        if self.eat_kw(Keyword::Having) {
            node.having_clause = Some(Box::new(self.expr()?));
        }
        Ok(node)
    }

    /// `VALUES '(' expr_list ')' (',' '(' expr_list ')')*`（PG `values_clause`）。
    fn values_lists(&mut self) -> Result<Vec<Vec<Expr>>, ParseError> {
        let mut out = Vec::new();
        loop {
            self.expect_punct(Punct::LParen)?;
            let mut row = Vec::new();
            loop {
                row.push(self.expr()?);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_punct(Punct::RParen)?;
            out.push(row);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        Ok(out)
    }

    /// `target_list`（PG `opt_target_list`：`target_el (',' target_el)*`）。
    fn target_list(&mut self) -> Result<Vec<ResTarget>, ParseError> {
        let mut out = Vec::new();
        loop {
            out.push(self.target_el()?);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        Ok(out)
    }

    /// `target_el`：`a_expr [AS label] | '*' | relation '.' '*'`
    /// （`*` 是 `ColumnRef{[AStar]}`——照 PG）。
    fn target_el(&mut self) -> Result<ResTarget, ParseError> {
        let location = self.peek().span;
        if self.at_punct(Punct::Star) {
            self.advance();
            return Ok(ResTarget {
                name: None,
                val: Expr::ColumnRef(ColumnRef {
                    fields: vec![ColumnRefField::AStar],
                    location,
                }),
                location,
            });
        }
        let val = self.expr()?;
        let mut name = None;
        let mut end = val.location();
        // `AS 名` 或裸名（照 PG 的 `opt_as_label`）——两种拼写同一结果。
        let has_label = self.at_kw(Keyword::As) || matches!(self.peek().kind, TokenKind::Ident(_));
        if has_label {
            self.eat_kw(Keyword::As);
            let (n, sp) = self.col_id()?;
            name = Some(n);
            end = sp;
        }
        Ok(ResTarget {
            name,
            val,
            location: location.merge(end),
        })
    }

    /// `from_list: table_ref (',' table_ref)*`——**逗号项仍是列表里的独立元素**
    /// （照 PG 的原始树；连接树的组装是 ② 的事）。
    fn parse_from_list(&mut self) -> Result<Vec<FromItem>, ParseError> {
        let mut items = vec![self.table_ref()?];
        while self.eat_punct(Punct::Comma) {
            items.push(self.table_ref()?);
        }
        Ok(items)
    }

    /// `table_ref`：`relation_expr | joined_table`（左递归——左深 `JoinExpr`）。
    fn table_ref(&mut self) -> Result<FromItem, ParseError> {
        let mut left = self.table_primary()?;
        loop {
            let jointype = if self.eat_kw(Keyword::Inner) {
                self.expect_kw(Keyword::Join)?;
                JoinType::Inner
            } else if self.eat_kw(Keyword::Left) {
                self.eat_kw(Keyword::Outer);
                self.expect_kw(Keyword::Join)?;
                JoinType::Left
            } else if self.eat_kw(Keyword::Join) {
                JoinType::Inner
            } else if self.at_kw(Keyword::Right) {
                return Err(self.err_here("不支持 `RIGHT JOIN`（REQ-SQL-006）"));
            } else if self.at_kw(Keyword::Full) {
                return Err(self.err_here("不支持 `FULL JOIN`（REQ-SQL-006）"));
            } else if self.at_kw(Keyword::Natural) {
                return Err(self.err_here("不支持 `NATURAL JOIN`（REQ-SQL-006）"));
            } else if self.at_kw(Keyword::Using) {
                return Err(self.err_here("不支持 `USING`（REQ-SQL-006）"));
            } else {
                break;
            };
            let rarg = self.table_primary()?;
            let quals = if self.eat_kw(Keyword::On) {
                Some(Box::new(self.expr()?))
            } else {
                None
            };
            let location = left.location().merge(rarg.location());
            left = FromItem::Join(Box::new(JoinExpr {
                jointype,
                larg: left,
                rarg,
                quals,
                location,
            }));
        }
        Ok(left)
    }

    /// A relation, attachment function or explicitly typed graph row source.
    fn table_primary(&mut self) -> Result<FromItem, ParseError> {
        let t = self.peek().clone();
        if matches!(&t.kind, TokenKind::Ident(name) if name == "graph_table")
            && matches!(self.peek2().kind, TokenKind::Punct(Punct::LParen))
        {
            let (_, location) = self.col_id()?;
            self.expect_punct(Punct::LParen)?;
            let (graph, _) = self.col_id()?;
            self.expect_punct(Punct::Comma)?;
            let query = self.graph_table_argument()?;
            let parameters = if matches!(&self.peek().kind, TokenKind::Ident(name) if name == "parameters")
            {
                self.advance();
                Some(self.graph_table_argument()?)
            } else {
                None
            };
            let budgets = if matches!(&self.peek().kind, TokenKind::Ident(name) if name == "budgets")
            {
                self.advance();
                Some(self.graph_table_argument()?)
            } else {
                None
            };
            self.expect_graph_word("columns")?;
            self.expect_punct(Punct::LParen)?;
            let mut columns = Vec::new();
            loop {
                let (colname, location) = self.col_id()?;
                let type_name = self.type_name()?;
                columns.push(ColumnDef {
                    colname,
                    type_name,
                    is_not_null: false,
                    location,
                });
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_punct(Punct::RParen)?;
            let end = self.expect_punct(Punct::RParen)?.span;
            let alias =
                if self.eat_kw(Keyword::As) || matches!(self.peek().kind, TokenKind::Ident(_)) {
                    Some(Alias {
                        aliasname: self.col_id()?.0,
                    })
                } else {
                    None
                };
            return Ok(FromItem::GraphTable(Box::new(GraphTable {
                graph,
                query,
                parameters,
                budgets,
                columns,
                alias,
                location: location.merge(end),
            })));
        }
        if matches!(&t.kind, TokenKind::Ident(name) if name == "attachment_grep")
            && matches!(self.peek2().kind, TokenKind::Punct(Punct::LParen))
        {
            let (name, location) = self.col_id()?;
            self.expect_punct(Punct::LParen)?;
            let mut args = Vec::new();
            if !self.at_punct(Punct::RParen) {
                loop {
                    args.push(self.expr()?);
                    if !self.eat_punct(Punct::Comma) {
                        break;
                    }
                }
            }
            self.expect_punct(Punct::RParen)?;
            let alias =
                if self.eat_kw(Keyword::As) || matches!(self.peek().kind, TokenKind::Ident(_)) {
                    Some(Alias {
                        aliasname: self.col_id()?.0,
                    })
                } else {
                    None
                };
            return Ok(FromItem::RangeFunction(RangeFunction {
                name,
                args,
                alias,
                location,
            }));
        }
        Ok(FromItem::RangeVar(self.relation_expr()?))
    }

    fn graph_table_argument(&mut self) -> Result<Expr, ParseError> {
        if !matches!(self.peek().kind, TokenKind::Str(_) | TokenKind::Param(_)) {
            return Err(
                self.err_here("GRAPH_TABLE arguments must be text literals or named parameters")
            );
        }
        self.primary()
    }

    /// `sort_by`（PG `SortBy` 的裁剪）：`a_expr [ASC|DESC]`。
    /// **`NULLS FIRST/LAST` 在清单外**——用到即拒绝（字段已留）。
    fn sort_by(&mut self) -> Result<SortBy, ParseError> {
        let node = self.expr()?;
        let mut sortby_dir = SortByDir::Default;
        let mut end = node.location();
        if self.eat_kw(Keyword::Desc) {
            sortby_dir = SortByDir::Desc;
            end = self.tokens[self.at.saturating_sub(1)].span;
        } else if self.eat_kw(Keyword::Asc) {
            sortby_dir = SortByDir::Asc;
            end = self.tokens[self.at.saturating_sub(1)].span;
        }
        if self.at_kw(Keyword::Nulls) {
            return Err(self.err_here(
                "不支持 `NULLS FIRST/LAST`（REQ-SQL-006；NULL 位置固定照 Oracle 默认）",
            ));
        }
        let location = node.location().merge(end);
        Ok(SortBy {
            node,
            sortby_dir,
            sortby_nulls: SortByNulls::Default,
            location,
        })
    }

    // ── 表达式（优先级照 PG 的 %left/%right/%nonassoc 声明）──

    /// 最低层：`OR`。
    fn expr(&mut self) -> Result<Expr, ParseError> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<Expr, ParseError> {
        let first = self.and_expr()?;
        if !self.at_kw(Keyword::Or) {
            return Ok(first);
        }
        let mut args = vec![first];
        while self.at_kw(Keyword::Or) {
            self.advance();
            args.push(self.and_expr()?);
        }
        let location = args[0].location().merge(args[args.len() - 1].location());
        Ok(Expr::BoolExpr(BoolExpr {
            boolop: BoolExprType::Or,
            args,
            location,
        }))
    }

    fn and_expr(&mut self) -> Result<Expr, ParseError> {
        let first = self.not_expr()?;
        if !self.at_kw(Keyword::And) {
            return Ok(first);
        }
        let mut args = vec![first];
        while self.at_kw(Keyword::And) {
            self.advance();
            args.push(self.not_expr()?);
        }
        let location = args[0].location().merge(args[args.len() - 1].location());
        Ok(Expr::BoolExpr(BoolExpr {
            boolop: BoolExprType::And,
            args,
            location,
        }))
    }

    /// `NOT`（**右结合、一元 `BoolExpr`**——照 PG）。
    fn not_expr(&mut self) -> Result<Expr, ParseError> {
        if self.at_kw(Keyword::Not) {
            let location = self.advance().span;
            let arg = self.not_expr()?;
            return Ok(Expr::BoolExpr(BoolExpr {
                boolop: BoolExprType::Not,
                args: vec![arg],
                location,
            }));
        }
        self.is_expr()
    }

    /// `IS [NOT] NULL`（PG 的 `%nonassoc IS ISNULL NOTNULL`——中缀、不可连写）。
    fn is_expr(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.cmp_expr()?;
        if self.at_kw(Keyword::Is) {
            let is_tok = self.advance();
            let negated = self.eat_kw(Keyword::Not);
            let null_tok = self.expect_kw(Keyword::Null)?;
            let location = lhs.location().merge(null_tok.span).merge(is_tok.span);
            return Ok(Expr::NullTest(NullTest {
                nulltesttype: if negated {
                    NullTestType::IsNotNull
                } else {
                    NullTestType::IsNull
                },
                arg: Box::new(lhs),
                location,
            }));
        }
        Ok(lhs)
    }

    /// 比较运算（PG `%nonassoc '<' '>' '=' ...`——**不可连写**）。
    fn cmp_expr(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.between_in_expr()?;
        let name = match self.peek().kind {
            TokenKind::Punct(Punct::Eq) => "=",
            TokenKind::Punct(Punct::Ne) => "<>",
            TokenKind::Punct(Punct::Lt) => "<",
            TokenKind::Punct(Punct::Le) => "<=",
            TokenKind::Punct(Punct::Gt) => ">",
            TokenKind::Punct(Punct::Ge) => ">=",
            _ => return Ok(lhs),
        };
        self.advance();
        let rhs = self.between_in_expr()?;
        let location = lhs.location().merge(rhs.location());
        Ok(Expr::AExpr(AExpr {
            kind: AExprKind::Op,
            name: name.to_owned(),
            lexpr: Some(Box::new(lhs)),
            rexpr: Some(Box::new(rhs)),
            rexpr_list: Vec::new(),
            location,
        }))
    }

    /// `BETWEEN` / `IN`（PG 的 `%nonassoc BETWEEN IN_P LIKE ILIKE SIMILAR NOT_LA`
    /// ——**比比较运算更紧**，照 PG 的声明顺序）。
    fn between_in_expr(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.multi_op_expr()?;
        let negated = self.at_kw(Keyword::Not)
            && matches!(
                self.peek2().kind,
                TokenKind::Keyword(Keyword::Between) | TokenKind::Keyword(Keyword::In)
            );
        if negated {
            self.advance();
        }
        if self.eat_kw(Keyword::Between) {
            let low = self.multi_op_expr()?;
            self.expect_kw(Keyword::And)?;
            let high = self.multi_op_expr()?;
            let location = lhs.location().merge(high.location());
            return Ok(Expr::AExpr(AExpr {
                kind: if negated {
                    AExprKind::NotBetween
                } else {
                    AExprKind::Between
                },
                name: "BETWEEN".to_owned(),
                lexpr: Some(Box::new(lhs)),
                rexpr: None,
                rexpr_list: vec![low, high],
                location,
            }));
        }
        if self.eat_kw(Keyword::In) {
            self.expect_punct(Punct::LParen)?;
            let mut list = Vec::new();
            loop {
                list.push(self.expr()?);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            let close = self.expect_punct(Punct::RParen)?.span;
            let location = lhs.location().merge(close);
            return Ok(Expr::AExpr(AExpr {
                kind: if negated {
                    AExprKind::NotIn
                } else {
                    AExprKind::In
                },
                name: "=".to_owned(), // 照 PG：IN 的 name 是 "="（值表在右侧）
                location,
                lexpr: Some(Box::new(lhs)),
                rexpr: None,
                rexpr_list: list,
            }));
        }
        if self.at_kw(Keyword::Like) {
            return Err(self.err_here("不支持 `LIKE`（REQ-SQL-006）"));
        }
        if negated {
            return Err(self.err_here("`NOT` 之后只能是 `BETWEEN` 或 `IN`"));
        }
        Ok(lhs)
    }

    /// 多字符操作符（PG `%left Op OPERATOR`——向量的 `<-> <=> <#>` 归这里）。
    fn multi_op_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.add_expr()?;
        loop {
            let name = match self.peek().kind {
                TokenKind::Punct(Punct::L2) => "<->",
                TokenKind::Punct(Punct::Cosine) => "<=>",
                TokenKind::Punct(Punct::NegInner) => "<#>",
                _ => return Ok(lhs),
            };
            self.advance();
            let rhs = self.add_expr()?;
            let location = lhs.location().merge(rhs.location());
            lhs = Expr::AExpr(AExpr {
                kind: AExprKind::Op,
                name: name.to_owned(),
                location,
                lexpr: Some(Box::new(lhs)),
                rexpr: Some(Box::new(rhs)),
                rexpr_list: Vec::new(),
            });
        }
    }

    fn add_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.mul_expr()?;
        loop {
            let name = match self.peek().kind {
                TokenKind::Punct(Punct::Plus) => "+",
                TokenKind::Punct(Punct::Minus) => "-",
                _ => return Ok(lhs),
            };
            self.advance();
            let rhs = self.mul_expr()?;
            let location = lhs.location().merge(rhs.location());
            lhs = Expr::AExpr(AExpr {
                kind: AExprKind::Op,
                name: name.to_owned(),
                location,
                lexpr: Some(Box::new(lhs)),
                rexpr: Some(Box::new(rhs)),
                rexpr_list: Vec::new(),
            });
        }
    }

    fn mul_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.unary_expr()?;
        loop {
            let name = match self.peek().kind {
                TokenKind::Punct(Punct::Star) => "*",
                TokenKind::Punct(Punct::Slash) => "/",
                _ => return Ok(lhs),
            };
            self.advance();
            let rhs = self.unary_expr()?;
            let location = lhs.location().merge(rhs.location());
            lhs = Expr::AExpr(AExpr {
                kind: AExprKind::Op,
                name: name.to_owned(),
                location,
                lexpr: Some(Box::new(lhs)),
                rexpr: Some(Box::new(rhs)),
                rexpr_list: Vec::new(),
            });
        }
    }

    /// 一元 `+` / `-`（PG 的 `%right UMINUS`）。
    ///
    /// **`-` 作用于数值字面量 ⇒ 折叠进常量**（PG `gram.y` 的 `doNegate` 同款）：
    /// 常量原文本前加 `-`，位置改记符号处；非字面量仍表达为
    /// `lexpr = NULL` 的 `A_Expr`（PG 形态——绑定侧再脱糖）。
    fn unary_expr(&mut self) -> Result<Expr, ParseError> {
        let name = match self.peek().kind {
            TokenKind::Punct(Punct::Minus) => "-",
            TokenKind::Punct(Punct::Plus) => "+",
            _ => return self.typecast_expr(),
        };
        let location = self.advance().span;
        let operand = self.unary_expr()?;
        if name == "-" {
            if let Expr::AConst(c) = &operand {
                let folded = match &c.value {
                    Some(ConstValue::Int(t)) => Some(ConstValue::Int(format!("-{t}"))),
                    Some(ConstValue::Float(t)) => Some(ConstValue::Float(format!("-{t}"))),
                    _ => None,
                };
                if let Some(value) = folded {
                    return Ok(Expr::AConst(AConst {
                        value: Some(value),
                        location,
                    }));
                }
            }
        }
        Ok(Expr::AExpr(AExpr {
            kind: AExprKind::Op,
            name: name.to_owned(),
            lexpr: None,
            rexpr: Some(Box::new(operand)),
            rexpr_list: Vec::new(),
            location,
        }))
    }

    /// `::` 转型（PG `%left TYPECAST`——后缀、可连写）。
    fn typecast_expr(&mut self) -> Result<Expr, ParseError> {
        let mut arg = self.primary()?;
        while self.at_punct(Punct::Cast) {
            self.advance();
            let type_name = self.type_name()?;
            let location = arg.location().merge(type_name.location);
            arg = Expr::TypeCast(TypeCast {
                arg: Box::new(arg),
                type_name,
                location,
            });
        }
        Ok(arg)
    }

    /// `primary`：字面量 / 列引用 / 参数 / `(expr)` / `CASE` / `CAST` /
    /// `COALESCE` / 函数调用。
    fn primary(&mut self) -> Result<Expr, ParseError> {
        let t = self.peek().clone();
        match t.kind {
            TokenKind::Number(s) => {
                self.advance();
                Ok(Expr::AConst(AConst {
                    value: Some(classify_number(&s)),
                    location: t.span,
                }))
            }
            TokenKind::Str(s) => {
                self.advance();
                Ok(Expr::AConst(AConst {
                    value: Some(ConstValue::Str(s)),
                    location: t.span,
                }))
            }
            TokenKind::Param(name) => {
                self.advance();
                Ok(Expr::ParamRef(ParamRef {
                    name,
                    location: t.span,
                }))
            }
            TokenKind::Keyword(Keyword::Null) => {
                self.advance();
                Ok(Expr::AConst(AConst {
                    value: None,
                    location: t.span,
                }))
            }
            TokenKind::Keyword(Keyword::True) | TokenKind::Keyword(Keyword::False) => {
                self.advance();
                let b = matches!(t.kind, TokenKind::Keyword(Keyword::True));
                Ok(Expr::AConst(AConst {
                    value: Some(ConstValue::Bool(b)),
                    location: t.span,
                }))
            }
            TokenKind::Punct(Punct::LParen) => {
                self.advance();
                let e = self.expr()?;
                self.expect_punct(Punct::RParen)?;
                Ok(e)
            }
            TokenKind::Keyword(Keyword::Case) => self.case_expr(),
            TokenKind::Keyword(Keyword::Cast) => self.cast_expr(),
            TokenKind::Keyword(Keyword::Coalesce) => {
                let location = self.advance().span;
                self.expect_punct(Punct::LParen)?;
                let mut args = Vec::new();
                loop {
                    args.push(self.expr()?);
                    if !self.eat_punct(Punct::Comma) {
                        break;
                    }
                }
                self.expect_punct(Punct::RParen)?;
                Ok(Expr::CoalesceExpr(CoalesceExpr { args, location }))
            }
            TokenKind::Keyword(Keyword::Select) => Err(ParseError {
                message: "不支持子查询（REQ-SQL-006）".to_owned(),
                span: t.span,
            }),
            TokenKind::Keyword(Keyword::Exists) => Err(ParseError {
                message: "不支持 `EXISTS` 子查询（REQ-SQL-006）".to_owned(),
                span: t.span,
            }),
            TokenKind::Keyword(Keyword::Over) => Err(ParseError {
                message: "不支持窗口函数（REQ-SQL-006）".to_owned(),
                span: t.span,
            }),
            TokenKind::Ident(_) | TokenKind::QIdent(_) | TokenKind::Keyword(_) => {
                self.columnref_or_call()
            }
            _ => Err(ParseError {
                message: "期望表达式".to_owned(),
                span: t.span,
            }),
        }
    }

    /// 列引用（含 `a.b` 与 `t.*`）或函数调用（含 `COUNT(*)` / `COUNT(DISTINCT x)`；
    /// **`NULLIF` 走 `A_Expr`，照 PG**）。
    fn columnref_or_call(&mut self) -> Result<Expr, ParseError> {
        let (first, fspan) = self.col_id()?;
        if self.at_punct(Punct::LParen) {
            self.advance();
            let mut agg_star = false;
            let mut agg_distinct = false;
            let mut args = Vec::new();
            if self.at_punct(Punct::Star) {
                self.advance();
                agg_star = true;
            } else {
                agg_distinct = self.eat_kw(Keyword::Distinct);
                loop {
                    args.push(self.expr()?);
                    if !self.eat_punct(Punct::Comma) {
                        break;
                    }
                }
            }
            let close = self.expect_punct(Punct::RParen)?.span;
            let location = fspan.merge(close);
            if first == "nullif" && args.len() == 2 {
                let mut it = args.into_iter();
                let a = it.next().expect("两项");
                let b = it.next().expect("两项");
                return Ok(Expr::AExpr(AExpr {
                    kind: AExprKind::NullIf,
                    name: "=".to_owned(), // 照 PG：NULLIF 的 name 是 "="
                    lexpr: Some(Box::new(a)),
                    rexpr: Some(Box::new(b)),
                    rexpr_list: Vec::new(),
                    location,
                }));
            }
            return Ok(Expr::FuncCall(FuncCall {
                funcname: first,
                args,
                agg_star,
                agg_distinct,
                location,
            }));
        }
        // 列引用（含 `a.b` / `t.*`）。
        let mut fields = vec![ColumnRefField::Name(first)];
        let mut end = fspan;
        while self.at_punct(Punct::Dot) {
            self.advance();
            if self.at_punct(Punct::Star) {
                end = self.advance().span;
                fields.push(ColumnRefField::AStar);
                break;
            }
            let (n, sp) = self.col_id()?;
            end = sp;
            fields.push(ColumnRefField::Name(n));
        }
        Ok(Expr::ColumnRef(ColumnRef {
            fields,
            location: fspan.merge(end),
        }))
    }

    /// `CASE`（PG `CaseExpr`）。
    fn case_expr(&mut self) -> Result<Expr, ParseError> {
        let location = self.expect_kw(Keyword::Case)?.span;
        let arg = if self.at_kw(Keyword::When) {
            None
        } else {
            Some(Box::new(self.expr()?))
        };
        let mut args = Vec::new();
        while self.at_kw(Keyword::When) {
            let wloc = self.advance().span;
            let cond = self.expr()?;
            self.expect_kw(Keyword::Then)?;
            let result = self.expr()?;
            args.push(CaseWhen {
                location: cond.location().merge(result.location()).merge(wloc),
                expr: cond,
                result,
            });
        }
        if args.is_empty() {
            return Err(self.err_here("`CASE` 至少需要一个 `WHEN`"));
        }
        let defresult = if self.eat_kw(Keyword::Else) {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        let end = self.expect_kw(Keyword::End)?.span;
        Ok(Expr::CaseExpr(CaseExpr {
            arg,
            args,
            defresult,
            location: location.merge(end),
        }))
    }

    /// `CAST '(' a_expr AS Typename ')'`（PG `TypeCast`）。
    fn cast_expr(&mut self) -> Result<Expr, ParseError> {
        let location = self.expect_kw(Keyword::Cast)?.span;
        self.expect_punct(Punct::LParen)?;
        let arg = self.expr()?;
        self.expect_kw(Keyword::As)?;
        let type_name = self.type_name()?;
        let end = self.expect_punct(Punct::RParen)?.span;
        Ok(Expr::TypeCast(TypeCast {
            arg: Box::new(arg),
            type_name,
            location: location.merge(end),
        }))
    }
}

/// 数字原文本的分类（PG 的 `ICONST`/`FCONST`：含 `.` 或指数 ⇒ Float）。
fn classify_number(text: &str) -> ConstValue {
    if text.contains('.') || text.contains('e') || text.contains('E') {
        ConstValue::Float(text.to_owned())
    } else {
        ConstValue::Int(text.to_owned())
    }
}

fn punct_text(p: Punct) -> &'static str {
    match p {
        Punct::LParen => "(",
        Punct::RParen => ")",
        Punct::LBracket => "[",
        Punct::RBracket => "]",
        Punct::Comma => ",",
        Punct::Dot => ".",
        Punct::Semi => ";",
        Punct::Star => "*",
        Punct::Plus => "+",
        Punct::Minus => "-",
        Punct::Slash => "/",
        Punct::Eq => "=",
        Punct::Ne => "<>",
        Punct::Lt => "<",
        Punct::Le => "<=",
        Punct::Gt => ">",
        Punct::Ge => ">=",
        Punct::Cast => "::",
        Punct::L2 => "<->",
        Punct::Cosine => "<=>",
        Punct::NegInner => "<#>",
    }
}
