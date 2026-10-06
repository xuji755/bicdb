//! **语法**（设计 `doc/SQL前端设计_v0.1.md` §3.2；切片 S1）：
//! 手写递归下降 + 显式优先级的二元运算。
//!
//! **闭集纪律**（REQ-SQL-006）：不提供清单里的构造**没有产生式**——用到即是
//! 语法错误（文案"不支持该构造"），不是"解析后再拒绝"。

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
    let mut p = Parser { tokens, at: 0 };
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
    let mut p = Parser { tokens, at: 0 };
    let mut out = Vec::new();
    loop {
        while p.eat_punct(Punct::Semi) {}
        if matches!(p.peek().kind, TokenKind::Eof) {
            return Ok(out);
        }
        out.push(p.statement()?);
        // 语句之间必须有 `;`（或到末尾）。
        if !matches!(p.peek().kind, TokenKind::Eof) && !p.eat_punct(Punct::Semi) {
            return Err(p.err_here("语句之间需要 `;`"));
        }
    }
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
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

    /// 当前是不是某个关键字。
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

    /// 标识符形态的名字（表名/列名不设保留字——闭集关键字也可用作名字）。
    ///
    /// **口径**：普通标识符**保留原文本**（大小写折叠在 ②，REQ-SQL-002）；
    /// 与关键字同形的名字（如列名 `name`）取关键字的**规范大写形态**——
    /// 与 ② 的折叠结果一致（Oracle 风格大写折叠），故无歧义。
    fn ident_like(&mut self) -> Result<(String, Span), ParseError> {
        let t = self.advance();
        match t.kind {
            TokenKind::Ident(s) => Ok((s, t.span)),
            TokenKind::Keyword(k) => Ok((k.text().to_owned(), t.span)),
            _ => Err(ParseError {
                message: "期望名字".to_owned(),
                span: t.span,
            }),
        }
    }

    /// 工作区 id（`ALTER/DROP WORKSPACE <id>`；**原文本**，数字或标识符形态）。
    fn workspace_ref(&mut self) -> Result<(String, Span), ParseError> {
        let t = self.advance();
        match t.kind {
            TokenKind::Ident(s) | TokenKind::Number(s) => Ok((s, t.span)),
            _ => Err(ParseError {
                message: "期望工作区 id".to_owned(),
                span: t.span,
            }),
        }
    }

    fn expect_ident(&mut self) -> Result<(String, Span), ParseError> {
        let t = self.advance();
        match t.kind {
            TokenKind::Ident(s) => Ok((s, t.span)),
            _ => Err(ParseError {
                message: "期望标识符".to_owned(),
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

    /// 原文本数字（`LIMIT n` / 选项值等——语义在 ②）。
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

    // ── 语句分派 ──────────────────────────────────────────

    fn statement(&mut self) -> Result<Stmt, ParseError> {
        let t = self.peek().clone();
        match t.kind {
            TokenKind::Keyword(Keyword::Select) => Ok(Stmt::Select(self.select_stmt()?)),
            TokenKind::Keyword(Keyword::Insert) => Ok(Stmt::Insert(self.insert_stmt()?)),
            TokenKind::Keyword(Keyword::Update) => Ok(Stmt::Update(self.update_stmt()?)),
            TokenKind::Keyword(Keyword::Delete) => Ok(Stmt::Delete(self.delete_stmt()?)),
            TokenKind::Keyword(Keyword::Create) => self.create_stmt(),
            TokenKind::Keyword(Keyword::Drop) => self.drop_stmt(),
            TokenKind::Keyword(Keyword::Alter) => self.alter_stmt(),
            TokenKind::Keyword(Keyword::Begin) => {
                let span = self.advance().span;
                Ok(Stmt::Txn {
                    kind: TxnStmt::Begin,
                    span,
                })
            }
            TokenKind::Keyword(Keyword::Commit) => {
                let span = self.advance().span;
                Ok(Stmt::Txn {
                    kind: TxnStmt::Commit,
                    span,
                })
            }
            TokenKind::Keyword(Keyword::Rollback) => {
                let span = self.advance().span;
                Ok(Stmt::Txn {
                    kind: TxnStmt::Rollback,
                    span,
                })
            }
            TokenKind::Keyword(Keyword::With) => Err(ParseError {
                message: "不支持该构造：`WITH` / CTE / 递归（REQ-SQL-006）".to_owned(),
                span: t.span,
            }),
            _ => Err(ParseError {
                message: "不是一条可识别的语句".to_owned(),
                span: t.span,
            }),
        }
    }

    fn create_stmt(&mut self) -> Result<Stmt, ParseError> {
        let start = self.expect_kw(Keyword::Create)?.span;
        if self.eat_kw(Keyword::Table) {
            return Ok(Stmt::CreateTable(self.create_table(start)?));
        }
        let unique = self.eat_kw(Keyword::Unique);
        if self.eat_kw(Keyword::Index) {
            return Ok(Stmt::CreateIndex(self.create_index(start, unique)?));
        }
        if unique {
            return Err(self.err_here("`UNIQUE` 只能用于 `CREATE INDEX`"));
        }
        if self.eat_kw(Keyword::Graph) {
            let (name, end) = self.ident_like()?;
            return Ok(Stmt::CreateGraph(CreateGraphStmt {
                name,
                span: start.merge(end),
            }));
        }
        if self.eat_kw(Keyword::Workspace) {
            return Ok(Stmt::CreateWorkspace(self.create_workspace(start)?));
        }
        Err(self.err_here("`CREATE` 之后只能是 TABLE / [UNIQUE] INDEX / GRAPH / WORKSPACE"))
    }

    fn drop_stmt(&mut self) -> Result<Stmt, ParseError> {
        let start = self.expect_kw(Keyword::Drop)?.span;
        if self.eat_kw(Keyword::Table) {
            let (name, end) = self.ident_like()?;
            return Ok(Stmt::DropTable(DropTableStmt {
                name,
                span: start.merge(end),
            }));
        }
        if self.eat_kw(Keyword::Index) {
            let (name, end) = self.ident_like()?;
            return Ok(Stmt::DropIndex(DropIndexStmt {
                name,
                span: start.merge(end),
            }));
        }
        if self.eat_kw(Keyword::Graph) {
            let (name, end) = self.ident_like()?;
            return Ok(Stmt::DropGraph(DropGraphStmt {
                name,
                span: start.merge(end),
            }));
        }
        if self.eat_kw(Keyword::Workspace) {
            let (id, end) = self.workspace_ref()?;
            return Ok(Stmt::DropWorkspace(DropWorkspaceStmt {
                workspace_id: id,
                span: start.merge(end),
            }));
        }
        Err(self.err_here("`DROP` 之后只能是 TABLE / INDEX / GRAPH / WORKSPACE"))
    }

    fn alter_stmt(&mut self) -> Result<Stmt, ParseError> {
        let start = self.expect_kw(Keyword::Alter)?.span;
        self.expect_kw(Keyword::Workspace)?;
        let (workspace_id, _) = self.workspace_ref()?;
        self.expect_kw(Keyword::Set)?;
        if self.eat_kw(Keyword::Name) {
            self.expect_punct(Punct::Eq)?;
            let (name, span) = if self.eat_kw(Keyword::Null) {
                (None, self.peek().span)
            } else {
                let (s, sp) = self.expect_string()?;
                (Some(s), sp)
            };
            Ok(Stmt::AlterWorkspace(AlterWorkspaceStmt {
                workspace_id,
                action: AlterAction::SetName(name),
                span: start.merge(span),
            }))
        } else if self.eat_kw(Keyword::Quota) {
            let (items, span) = self.option_list()?;
            Ok(Stmt::AlterWorkspace(AlterWorkspaceStmt {
                workspace_id,
                action: AlterAction::SetQuota(items),
                span: start.merge(span),
            }))
        } else {
            Err(self.err_here("`ALTER WORKSPACE … SET` 之后只能是 `NAME` 或 `QUOTA`"))
        }
    }

    // ── DDL 体 ────────────────────────────────────────────

    fn create_table(&mut self, start: Span) -> Result<CreateTableStmt, ParseError> {
        let (name, _) = self.ident_like()?;
        self.expect_punct(Punct::LParen)?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.column_def()?);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        let close = self.expect_punct(Punct::RParen)?.span;
        let mut options = Vec::new();
        let mut end = close;
        if self.eat_kw(Keyword::With) {
            let (items, span) = self.option_list()?;
            options = items;
            end = span;
        }
        Ok(CreateTableStmt {
            name,
            columns,
            options,
            span: start.merge(end),
        })
    }

    fn column_def(&mut self) -> Result<ColumnDef, ParseError> {
        let (name, nspan) = self.ident_like()?;
        let type_name = self.type_name()?;
        let not_null = if self.eat_kw(Keyword::Not) {
            self.expect_kw(Keyword::Null)?;
            true
        } else {
            false
        };
        // 列约束清单外的一切（DEFAULT/PK/FK/CHECK）在语法上就没有产生式。
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
        let end = self.tokens[self.at.saturating_sub(1)].span;
        Ok(ColumnDef {
            name,
            type_name,
            not_null,
            span: nspan.merge(end),
        })
    }

    /// 类型名（文本 + 可选参数；**合法性在 ②**）。
    fn type_name(&mut self) -> Result<TypeName, ParseError> {
        let (name, span) = self.ident_like()?;
        let mut args = Vec::new();
        let mut end = span;
        if self.at_punct(Punct::LParen) {
            self.advance();
            loop {
                let (n, _sp) = self.expect_number_text()?;
                args.push(n);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            end = self.expect_punct(Punct::RParen)?.span;
        }
        Ok(TypeName {
            name,
            args,
            span: span.merge(end),
        })
    }

    fn option_list(&mut self) -> Result<(Vec<OptionItem>, Span), ParseError> {
        self.expect_punct(Punct::LParen)?;
        let mut items = Vec::new();
        loop {
            let (name, nspan) = self.ident_like()?;
            self.expect_punct(Punct::Eq)?;
            let t = self.advance();
            let value = match t.kind {
                TokenKind::Ident(s) => OptionValue::Ident(s),
                TokenKind::Keyword(k) => OptionValue::Ident(k.text().to_ascii_lowercase()),
                TokenKind::Number(s) => OptionValue::Number(s),
                TokenKind::Str(s) => OptionValue::Str(s),
                other => {
                    return Err(ParseError {
                        message: format!("选项值形态非法（{other:?}）"),
                        span: t.span,
                    })
                }
            };
            items.push(OptionItem {
                name,
                value,
                span: nspan.merge(t.span),
            });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        let close = self.expect_punct(Punct::RParen)?.span;
        Ok((items, close))
    }

    fn create_index(&mut self, start: Span, unique: bool) -> Result<CreateIndexStmt, ParseError> {
        let (name, _) = self.ident_like()?;
        self.expect_kw(Keyword::On)?;
        let (target_name, _) = self.ident_like()?;
        let target = if self.eat_kw(Keyword::Vertex) {
            IndexTarget::Vertex(target_name)
        } else if self.eat_kw(Keyword::Edge) {
            IndexTarget::Edge(target_name)
        } else {
            IndexTarget::Table(target_name)
        };
        self.expect_punct(Punct::LParen)?;
        let mut keys = Vec::new();
        loop {
            keys.push(self.expr()?);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        let end = self.expect_punct(Punct::RParen)?.span;
        Ok(CreateIndexStmt {
            name,
            unique,
            target,
            keys,
            span: start.merge(end),
        })
    }

    fn create_workspace(&mut self, start: Span) -> Result<CreateWorkspaceStmt, ParseError> {
        self.expect_kw(Keyword::For)?;
        self.expect_kw(Keyword::User)?;
        let (subject, mut end) = self.ident_like()?;
        let mut name = None;
        let mut clone_of = None;
        loop {
            if self.eat_kw(Keyword::Name) {
                let (s, sp) = self.expect_string()?;
                name = Some(s);
                end = sp;
            } else if self.eat_kw(Keyword::Clone) {
                self.expect_kw(Keyword::Of)?;
                let (s, sp) = self.workspace_ref()?;
                clone_of = Some(s);
                end = sp;
            } else {
                break;
            }
        }
        Ok(CreateWorkspaceStmt {
            subject,
            name,
            clone_of,
            span: start.merge(end),
        })
    }

    // ── DML ──────────────────────────────────────────────

    fn insert_stmt(&mut self) -> Result<InsertStmt, ParseError> {
        let start = self.expect_kw(Keyword::Insert)?.span;
        self.expect_kw(Keyword::Into)?;
        let (table, _) = self.ident_like()?;
        let mut columns = None;
        if self.at_punct(Punct::LParen) {
            // `INSERT INTO t (a, b) VALUES …` —— 列清单（列名）。
            self.advance();
            let mut cols = Vec::new();
            loop {
                let (c, _) = self.ident_like()?;
                cols.push(c);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_punct(Punct::RParen)?;
            columns = Some(cols);
        }
        self.expect_kw(Keyword::Values)?;
        let mut rows = Vec::new();
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
            rows.push(row);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        let end = self.tokens[self.at.saturating_sub(1)].span;
        Ok(InsertStmt {
            table,
            columns,
            rows,
            span: start.merge(end),
        })
    }

    fn update_stmt(&mut self) -> Result<UpdateStmt, ParseError> {
        let start = self.expect_kw(Keyword::Update)?.span;
        let (table, _) = self.ident_like()?;
        self.expect_kw(Keyword::Set)?;
        let mut sets = Vec::new();
        loop {
            let (col, cspan) = self.ident_like()?;
            self.expect_punct(Punct::Eq)?;
            let e = self.expr()?;
            sets.push((col, e));
            let _ = cspan;
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        let mut end = self.tokens[self.at.saturating_sub(1)].span;
        let filter = if self.eat_kw(Keyword::Where) {
            let e = self.expr()?;
            end = e.span();
            Some(e)
        } else {
            None
        };
        Ok(UpdateStmt {
            table,
            sets,
            filter,
            span: start.merge(end),
        })
    }

    fn delete_stmt(&mut self) -> Result<DeleteStmt, ParseError> {
        let start = self.expect_kw(Keyword::Delete)?.span;
        self.expect_kw(Keyword::From)?;
        let (table, _) = self.ident_like()?;
        let mut end = self.tokens[self.at.saturating_sub(1)].span;
        let filter = if self.eat_kw(Keyword::Where) {
            let e = self.expr()?;
            end = e.span();
            Some(e)
        } else {
            None
        };
        Ok(DeleteStmt {
            table,
            filter,
            span: start.merge(end),
        })
    }

    // ── SELECT ────────────────────────────────────────────

    fn select_stmt(&mut self) -> Result<SelectStmt, ParseError> {
        let start = self.expect_kw(Keyword::Select)?.span;
        let first = self.select_core(start)?;
        let mut set_ops = Vec::new();
        loop {
            let op = if self.eat_kw(Keyword::Union) {
                SetOpKind::Union
            } else if self.eat_kw(Keyword::Intersect) {
                SetOpKind::Intersect
            } else if self.eat_kw(Keyword::Except) {
                SetOpKind::Except
            } else {
                break;
            };
            let all = self.eat_kw(Keyword::All);
            let op_span = self.tokens[self.at.saturating_sub(1)].span;
            let core_start = self.expect_kw(Keyword::Select)?.span;
            let core = self.select_core(core_start)?;
            let span = op_span.merge(core.span);
            set_ops.push(SetOpTail {
                op,
                all,
                core,
                span,
            });
        }
        let mut order_by = Vec::new();
        if self.at_kw(Keyword::Order) {
            self.advance();
            self.expect_kw(Keyword::By)?;
            loop {
                let e = self.expr()?;
                let mut desc = false;
                let mut end = e.span();
                if self.eat_kw(Keyword::Desc) {
                    desc = true;
                    end = self.tokens[self.at.saturating_sub(1)].span;
                } else if self.eat_kw(Keyword::Asc) {
                    end = self.tokens[self.at.saturating_sub(1)].span;
                }
                order_by.push(OrderItem {
                    span: e.span().merge(end),
                    expr: e,
                    desc,
                });
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
        }
        let mut limit = None;
        let mut offset = None;
        if self.eat_kw(Keyword::Limit) {
            let (n, _) = self.expect_number_text()?;
            limit = Some(n);
        }
        if self.eat_kw(Keyword::Offset) {
            let (n, _) = self.expect_number_text()?;
            offset = Some(n);
        }
        let end = self.tokens[self.at.saturating_sub(1)].span;
        Ok(SelectStmt {
            first,
            set_ops,
            order_by,
            limit,
            offset,
            span: start.merge(end),
        })
    }

    fn select_core(&mut self, start: Span) -> Result<SelectCore, ParseError> {
        let distinct = self.eat_kw(Keyword::Distinct);
        // `DISTINCT` 与 `*` 之外：投影项列表。
        let mut projection = Vec::new();
        loop {
            if self.at_punct(Punct::Star) {
                let span = self.advance().span;
                projection.push(SelectItem {
                    expr: None,
                    alias: None,
                    span,
                });
            } else {
                let e = self.expr()?;
                let mut alias = None;
                let mut end = e.span();
                if self.eat_kw(Keyword::As) {
                    let (a, sp) = self.ident_like()?;
                    alias = Some(a);
                    end = sp;
                } else if matches!(self.peek().kind, TokenKind::Ident(_)) {
                    // 裸别名（`expr name`）
                    let (a, sp) = self.expect_ident()?;
                    alias = Some(a);
                    end = sp;
                }
                projection.push(SelectItem {
                    span: e.span().merge(end),
                    expr: Some(e),
                    alias,
                });
            }
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        let mut from = None;
        if self.eat_kw(Keyword::From) {
            from = Some(self.parse_from_clause()?);
        }
        let mut filter = None;
        if self.eat_kw(Keyword::Where) {
            filter = Some(self.expr()?);
        }
        let mut group_by = Vec::new();
        if self.at_kw(Keyword::Group) {
            self.advance();
            self.expect_kw(Keyword::By)?;
            loop {
                group_by.push(self.expr()?);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
        }
        let mut having = None;
        if self.eat_kw(Keyword::Having) {
            having = Some(self.expr()?);
        }
        let end = self.tokens[self.at.saturating_sub(1)].span;
        Ok(SelectCore {
            distinct,
            projection,
            from,
            filter,
            group_by,
            having,
            span: start.merge(end),
        })
    }

    fn parse_from_clause(&mut self) -> Result<FromClause, ParseError> {
        let base = self.table_ref()?;
        let start = table_ref_span(&base);
        let mut joins = Vec::new();
        loop {
            // 逗号连接 = 普通连接（INNER、无 ON）。
            if self.eat_punct(Punct::Comma) {
                let table = self.table_ref()?;
                let span = start.merge(table_ref_span(&table));
                joins.push(Join {
                    kind: JoinKind::Inner,
                    table,
                    on: None,
                    span,
                });
                continue;
            }
            let kind = if self.eat_kw(Keyword::Inner) {
                self.expect_kw(Keyword::Join)?;
                JoinKind::Inner
            } else if self.eat_kw(Keyword::Left) {
                self.eat_kw(Keyword::Outer);
                self.expect_kw(Keyword::Join)?;
                JoinKind::Left
            } else if self.eat_kw(Keyword::Join) {
                JoinKind::Inner
            } else if self.at_kw(Keyword::Right) {
                return Err(self.err_here("不支持 `RIGHT JOIN`（REQ-SQL-006）"));
            } else {
                break;
            };
            let table = self.table_ref()?;
            let on = if self.eat_kw(Keyword::On) {
                Some(self.expr()?)
            } else {
                None
            };
            let end = on
                .as_ref()
                .map_or_else(|| table_ref_span(&table), Expr::span);
            joins.push(Join {
                kind,
                table,
                on,
                span: start.merge(end),
            });
        }
        let end = joins
            .last()
            .map_or_else(|| table_ref_span(&base), |j| j.span);
        Ok(FromClause {
            base,
            joins,
            span: start.merge(end),
        })
    }

    fn table_ref(&mut self) -> Result<TableRef, ParseError> {
        let t = self.peek().clone();
        // GRAPH_TABLE 的表值接口随 GRP 域接入（本切片不解析——**响亮拒绝**）。
        if let TokenKind::Ident(ref s) = t.kind {
            if s.eq_ignore_ascii_case("graph_table") {
                return Err(ParseError {
                    message: "`GRAPH_TABLE` 随图域接入（本切片未实现）".to_owned(),
                    span: t.span,
                });
            }
        }
        let (name, nspan) = self.ident_like()?;
        let mut alias = None;
        let mut end = nspan;
        if self.eat_kw(Keyword::As) {
            let (a, sp) = self.ident_like()?;
            alias = Some(a);
            end = sp;
        } else if matches!(self.peek().kind, TokenKind::Ident(_)) {
            let (a, sp) = self.expect_ident()?;
            alias = Some(a);
            end = sp;
        }
        Ok(TableRef::Name {
            name,
            alias,
            span: nspan.merge(end),
        })
    }

    // ── 表达式（显式优先级）────────────────────────────────

    fn expr(&mut self) -> Result<Expr, ParseError> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.and_expr()?;
        while self.at_kw(Keyword::Or) {
            let span = self.advance().span;
            let right = self.and_expr()?;
            let full = left.span().merge(right.span()).merge(span);
            left = Expr::Binary {
                op: BinaryOp::Or,
                left: Box::new(left),
                right: Box::new(right),
                span: full,
            };
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.not_expr()?;
        while self.at_kw(Keyword::And) {
            let span = self.advance().span;
            let right = self.not_expr()?;
            let full = left.span().merge(right.span()).merge(span);
            left = Expr::Binary {
                op: BinaryOp::And,
                left: Box::new(left),
                right: Box::new(right),
                span: full,
            };
        }
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<Expr, ParseError> {
        if self.at_kw(Keyword::Not) {
            let span = self.advance().span;
            let inner = self.not_expr()?;
            let full = span.merge(inner.span());
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(inner),
                span: full,
            });
        }
        self.cmp_expr()
    }

    fn cmp_expr(&mut self) -> Result<Expr, ParseError> {
        let left = self.add_expr()?;
        // 比较 / IS / IN / BETWEEN（后三者可带 NOT）。
        let negated = self.at_kw(Keyword::Not)
            && matches!(
                self.peek2().kind,
                TokenKind::Keyword(Keyword::In)
                    | TokenKind::Keyword(Keyword::Between)
                    | TokenKind::Keyword(Keyword::Like)
            );
        if negated {
            self.advance();
        }
        if self.at_kw(Keyword::Is) {
            self.advance();
            let not2 = self.eat_kw(Keyword::Not);
            let end = self.expect_kw(Keyword::Null)?.span;
            let full = left.span().merge(end);
            return Ok(Expr::IsNull {
                expr: Box::new(left),
                negated: not2,
                span: full,
            });
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
            let end = self.expect_punct(Punct::RParen)?.span;
            let full = left.span().merge(end);
            return Ok(Expr::InList {
                expr: Box::new(left),
                list,
                negated,
                span: full,
            });
        }
        if self.eat_kw(Keyword::Between) {
            let low = self.add_expr()?;
            self.expect_kw(Keyword::And)?;
            let high = self.add_expr()?;
            let full = left.span().merge(high.span());
            return Ok(Expr::Between {
                expr: Box::new(left),
                low: Box::new(low),
                high: Box::new(high),
                negated,
                span: full,
            });
        }
        if self.at_kw(Keyword::Like) {
            return Err(self.err_here("不支持 `LIKE`（REQ-SQL-006）"));
        }
        let op = match self.peek().kind {
            TokenKind::Punct(Punct::Eq) => BinaryOp::Eq,
            TokenKind::Punct(Punct::Ne) => BinaryOp::Ne,
            TokenKind::Punct(Punct::Lt) => BinaryOp::Lt,
            TokenKind::Punct(Punct::Le) => BinaryOp::Le,
            TokenKind::Punct(Punct::Gt) => BinaryOp::Gt,
            TokenKind::Punct(Punct::Ge) => BinaryOp::Ge,
            TokenKind::Punct(Punct::L2) => BinaryOp::VecL2,
            TokenKind::Punct(Punct::Cosine) => BinaryOp::VecCosine,
            TokenKind::Punct(Punct::NegInner) => BinaryOp::VecNegInner,
            _ => return Ok(left),
        };
        self.advance();
        let right = self.add_expr()?;
        let full = left.span().merge(right.span());
        Ok(Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
            span: full,
        })
    }

    fn add_expr(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.mul_expr()?;
        loop {
            let op = if self.at_punct(Punct::Plus) {
                BinaryOp::Add
            } else if self.at_punct(Punct::Minus) {
                BinaryOp::Sub
            } else {
                return Ok(left);
            };
            self.advance();
            let right = self.mul_expr()?;
            let full = left.span().merge(right.span());
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                span: full,
            };
        }
    }

    fn mul_expr(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.unary_expr()?;
        loop {
            let op = if self.at_punct(Punct::Star) {
                BinaryOp::Mul
            } else if self.at_punct(Punct::Slash) {
                BinaryOp::Div
            } else {
                return Ok(left);
            };
            self.advance();
            let right = self.unary_expr()?;
            let full = left.span().merge(right.span());
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                span: full,
            };
        }
    }

    fn unary_expr(&mut self) -> Result<Expr, ParseError> {
        if self.at_punct(Punct::Minus) {
            let span = self.advance().span;
            let inner = self.unary_expr()?;
            let full = span.merge(inner.span());
            return Ok(Expr::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(inner),
                span: full,
            });
        }
        if self.at_punct(Punct::Plus) {
            let span = self.advance().span;
            let inner = self.unary_expr()?;
            let full = span.merge(inner.span());
            return Ok(Expr::Unary {
                op: UnaryOp::PosAffirm,
                expr: Box::new(inner),
                span: full,
            });
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        let t = self.peek().clone();
        match t.kind {
            TokenKind::Number(s) => {
                self.advance();
                Ok(Expr::Literal {
                    value: Literal::Number(s),
                    span: t.span,
                })
            }
            TokenKind::Str(s) => {
                self.advance();
                Ok(Expr::Literal {
                    value: Literal::Str(s),
                    span: t.span,
                })
            }
            TokenKind::Param(name) => {
                self.advance();
                Ok(Expr::Param { name, span: t.span })
            }
            TokenKind::Keyword(Keyword::Null) => {
                self.advance();
                Ok(Expr::Literal {
                    value: Literal::Null,
                    span: t.span,
                })
            }
            TokenKind::Keyword(Keyword::True) => {
                self.advance();
                Ok(Expr::Literal {
                    value: Literal::Bool(true),
                    span: t.span,
                })
            }
            TokenKind::Keyword(Keyword::False) => {
                self.advance();
                Ok(Expr::Literal {
                    value: Literal::Bool(false),
                    span: t.span,
                })
            }
            TokenKind::Punct(Punct::LParen) => {
                self.advance();
                let e = self.expr()?;
                self.expect_punct(Punct::RParen)?;
                Ok(e)
            }
            TokenKind::Keyword(Keyword::Case) => self.case_expr(),
            TokenKind::Keyword(Keyword::Cast) => self.cast_expr(),
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
            TokenKind::Ident(_) | TokenKind::Keyword(_) => self.name_or_call(),
            _ => Err(ParseError {
                message: "期望表达式".to_owned(),
                span: t.span,
            }),
        }
    }

    fn name_or_call(&mut self) -> Result<Expr, ParseError> {
        let (first, fspan) = self.ident_like()?;
        // 限定列名 `a.b`（只允许一层）。
        if self.at_punct(Punct::Dot) {
            self.advance();
            let (second, sspan) = self.ident_like()?;
            let full = fspan.merge(sspan);
            return Ok(Expr::Column {
                qualifier: Some(first),
                name: second,
                span: full,
            });
        }
        if self.at_punct(Punct::LParen) {
            self.advance();
            let distinct = self.eat_kw(Keyword::Distinct);
            let mut args = Vec::new();
            if !self.at_punct(Punct::RParen) {
                loop {
                    // 实参位置允许 `*`（`COUNT(*)`）；其他位置语法层不可达。
                    if self.at_punct(Punct::Star) {
                        let span = self.advance().span;
                        args.push(Expr::Star { span });
                    } else {
                        args.push(self.expr()?);
                    }
                    if !self.eat_punct(Punct::Comma) {
                        break;
                    }
                }
            }
            let end = self.expect_punct(Punct::RParen)?.span;
            return Ok(Expr::Call {
                name: first,
                distinct,
                args,
                span: fspan.merge(end),
            });
        }
        Ok(Expr::Column {
            qualifier: None,
            name: first,
            span: fspan,
        })
    }

    fn case_expr(&mut self) -> Result<Expr, ParseError> {
        let start = self.expect_kw(Keyword::Case)?.span;
        let operand = if self.at_kw(Keyword::When) {
            None
        } else {
            Some(Box::new(self.expr()?))
        };
        let mut whens = Vec::new();
        while self.eat_kw(Keyword::When) {
            let cond = self.expr()?;
            self.expect_kw(Keyword::Then)?;
            let result = self.expr()?;
            whens.push((cond, result));
        }
        if whens.is_empty() {
            return Err(self.err_here("`CASE` 至少需要一个 `WHEN`"));
        }
        let otherwise = if self.eat_kw(Keyword::Else) {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        let end = self.expect_kw(Keyword::End)?.span;
        Ok(Expr::Case {
            operand,
            whens,
            otherwise,
            span: start.merge(end),
        })
    }

    fn cast_expr(&mut self) -> Result<Expr, ParseError> {
        let start = self.expect_kw(Keyword::Cast)?.span;
        self.expect_punct(Punct::LParen)?;
        let expr = self.expr()?;
        self.expect_kw(Keyword::As)?;
        let type_name = self.type_name()?;
        let end = self.expect_punct(Punct::RParen)?.span;
        Ok(Expr::Cast {
            expr: Box::new(expr),
            type_name,
            span: start.merge(end),
        })
    }
}

fn table_ref_span(t: &TableRef) -> Span {
    match t {
        TableRef::Name { span, .. } => *span,
    }
}

fn punct_text(p: Punct) -> &'static str {
    match p {
        Punct::LParen => "(",
        Punct::RParen => ")",
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
        Punct::L2 => "<->",
        Punct::Cosine => "<=>",
        Punct::NegInner => "<#>",
    }
}
