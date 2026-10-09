use crate::model::{decimal, fail, Direction, Error, Value};

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Name(String),
    String(String),
    Number(String),
    Param(String),
    Symbol(String),
    End,
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Literal(Value),
    Variable(String),
    Parameter(String),
    Property(Box<Expr>, String),
    Label(Box<Expr>, String),
    List(Vec<Expr>),
    Map(Vec<(String, Expr)>),
    Function(String, Vec<Expr>, bool),
    Binary(String, Box<Expr>, Box<Expr>),
    Unary(String, Box<Expr>),
    Case(Vec<(Expr, Expr)>, Box<Expr>),
    Index(Box<Expr>, Box<Expr>),
    Slice(Box<Expr>, Option<Box<Expr>>, Option<Box<Expr>>),
    Comprehension(String, Box<Expr>, Option<Box<Expr>>, Box<Expr>),
    Quantifier(String, String, Box<Expr>, Box<Expr>),
    Reduce(String, Box<Expr>, String, Box<Expr>, Box<Expr>),
    Exists(Box<Query>),
    Star,
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodePattern {
    pub variable: Option<String>,
    pub labels: Vec<String>,
    pub properties: Vec<(String, Expr)>,
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EdgePattern {
    pub variable: Option<String>,
    pub types: Vec<String>,
    pub properties: Vec<(String, Expr)>,
    pub direction: Direction,
    pub minimum: usize,
    pub maximum: usize,
    pub variable_length: bool,
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Pattern {
    pub variable: Option<String>,
    pub nodes: Vec<NodePattern>,
    pub edges: Vec<EdgePattern>,
    pub shortest: bool,
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Projection {
    pub items: Vec<(Expr, String)>,
    pub distinct: bool,
    pub order: Vec<(Expr, bool)>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
    pub filter: Option<Expr>,
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Clause {
    Match(Vec<Pattern>, bool, Option<Expr>),
    Unwind(Expr, String),
    Filter(Expr),
    Call(Vec<String>, Box<Query>),
    Search(Vec<Expr>, Vec<(String, String)>),
    Fulltext(
        crate::property_index::EntityKind,
        Vec<Expr>,
        Vec<(String, String)>,
    ),
    Project(Projection, bool),
    Create(Vec<Pattern>),
    Merge(Vec<Pattern>, Vec<(Expr, Expr)>, Vec<(Expr, Expr)>),
    Set(Vec<(Expr, Expr)>),
    Remove(Vec<Expr>),
    Delete(Vec<Expr>, bool),
}
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub(crate) clauses: Vec<Clause>,
    pub(crate) union: Option<(bool, Box<Query>)>,
}
impl Query {
    pub fn is_read_only(&self) -> bool {
        !self.clauses.iter().any(|c| {
            matches!(
                c,
                Clause::Create(..)
                    | Clause::Merge(..)
                    | Clause::Set(..)
                    | Clause::Remove(..)
                    | Clause::Delete(..)
            ) || matches!(c,Clause::Call(_,q) if !q.is_read_only())
        }) && self.union.as_ref().map_or(true, |(_, q)| q.is_read_only())
    }
}

fn lex(input: &str) -> Result<Vec<Token>, Error> {
    if input.len() > 1024 * 1024 {
        return Err(fail("Cypher query exceeds 1 MiB"));
    }
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            if i + 1 >= chars.len() {
                return Err(fail("unterminated comment"));
            }
            i += 2;
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            i += 1;
            let mut value = String::new();
            let mut closed = false;
            while i < chars.len() {
                let ch = chars[i];
                i += 1;
                if ch == c {
                    if chars.get(i) == Some(&c) {
                        value.push(c);
                        i += 1;
                        continue;
                    }
                    closed = true;
                    break;
                }
                if ch == '\\' && c != '`' {
                    let e = *chars.get(i).ok_or_else(|| fail("unterminated escape"))?;
                    i += 1;
                    value.push(match e {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '\\' => '\\',
                        '\'' => '\'',
                        '"' => '"',
                        _ => return Err(fail("unsupported string escape")),
                    });
                } else {
                    value.push(ch);
                }
            }
            if !closed {
                return Err(fail("unterminated quoted value"));
            }
            out.push(if c == '`' {
                Token::Name(value)
            } else {
                Token::String(value)
            });
            continue;
        }
        if c == '$' {
            i += 1;
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            if i == start {
                return Err(fail("parameter name required"));
            }
            out.push(Token::Param(chars[start..i].iter().collect()));
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if chars.get(i) == Some(&'.') && chars.get(i + 1) != Some(&'.') {
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if chars.get(i).is_some_and(|c| *c == 'e' || *c == 'E') {
                i += 1;
                if chars.get(i).is_some_and(|c| *c == '+' || *c == '-') {
                    i += 1;
                }
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            out.push(Token::Number(chars[start..i].iter().collect()));
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(Token::Name(chars[start..i].iter().collect()));
            continue;
        }
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        if ["->", "<-", "<=", ">=", "<>", "!=", "..", "+=", "=~"].contains(&two.as_str()) {
            out.push(Token::Symbol(two));
            i += 2;
            continue;
        }
        if "()[]{}:,.|+-*/%=<>;".contains(c) {
            out.push(Token::Symbol(c.to_string()));
            i += 1;
            continue;
        }
        return Err(fail(format!("unsupported Cypher character {c}")));
    }
    if out.len() > 100_000 {
        return Err(fail("Cypher token budget exceeded"));
    }
    out.push(Token::End);
    Ok(out)
}
pub fn parse(input: &str) -> Result<Query, Error> {
    let mut p = Parser {
        tokens: lex(input)?,
        pos: 0,
        depth: 0,
    };
    let q = p.query()?;
    p.eat(";");
    if p.peek() != &Token::End {
        return Err(fail(format!("unsupported/trailing Cypher: {:?}", p.peek())));
    }
    Ok(q)
}
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    depth: usize,
}
impl Parser {
    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::End)
    }
    fn at(&self, name: &str) -> bool {
        match self.peek() {
            Token::Name(s) => s.eq_ignore_ascii_case(name),
            Token::Symbol(s) => s == name,
            _ => false,
        }
    }
    fn eat(&mut self, name: &str) -> bool {
        if self.at(name) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn need(&mut self, name: &str) -> Result<(), Error> {
        if self.eat(name) {
            Ok(())
        } else {
            Err(fail(format!("expected {name}, got {:?}", self.peek())))
        }
    }
    fn name(&mut self) -> Result<String, Error> {
        if let Token::Name(s) = self.peek().clone() {
            self.pos += 1;
            Ok(s)
        } else {
            Err(fail("identifier required"))
        }
    }
    fn query(&mut self) -> Result<Query, Error> {
        self.depth += 1;
        if self.depth > 64 {
            return Err(fail("Cypher query nesting exceeded"));
        }
        let result = self.query_inner();
        self.depth -= 1;
        result
    }
    fn yield_columns(&mut self) -> Result<Vec<(String, String)>, Error> {
        let mut yielded = vec![];
        loop {
            let column = self.name()?;
            let alias = if self.eat("AS") {
                self.name()?
            } else {
                column.clone()
            };
            yielded.push((column, alias));
            if !self.eat(",") {
                break;
            }
        }
        Ok(yielded)
    }
    fn query_inner(&mut self) -> Result<Query, Error> {
        let mut clauses = Vec::new();
        loop {
            let clause = if self.eat("OPTIONAL") {
                self.need("MATCH")?;
                let pats = self.patterns()?;
                let cond = if self.eat("WHERE") {
                    Some(self.expr(0)?)
                } else {
                    None
                };
                Clause::Match(pats, true, cond)
            } else if self.eat("MATCH") {
                let pats = self.patterns()?;
                let cond = if self.eat("WHERE") {
                    Some(self.expr(0)?)
                } else {
                    None
                };
                Clause::Match(pats, false, cond)
            } else if self.eat("CALL") {
                if self.eat("bicdb") {
                    self.need(".")?;
                    self.need("searchNodes")?;
                    self.need("(")?;
                    let args = self.exprs()?;
                    self.need(")")?;
                    self.need("YIELD")?;
                    Clause::Search(args, self.yield_columns()?)
                } else if self.eat("db") {
                    self.need(".")?;
                    self.need("index")?;
                    self.need(".")?;
                    self.need("fulltext")?;
                    self.need(".")?;
                    let entity = if self.eat("queryNodes") {
                        crate::property_index::EntityKind::Node
                    } else if self.eat("queryRelationships") {
                        crate::property_index::EntityKind::Relationship
                    } else {
                        return Err(fail(
                            "full-text procedure must be queryNodes or queryRelationships",
                        ));
                    };
                    self.need("(")?;
                    let args = self.exprs()?;
                    if args.len() != 3 {
                        return Err(fail(
                            "full-text procedure expects index,query,options with explicit db_type",
                        ));
                    }
                    self.need(")")?;
                    self.need("YIELD")?;
                    Clause::Fulltext(entity, args, self.yield_columns()?)
                } else {
                    self.need("(")?;
                    let mut imported = vec![];
                    if !self.eat(")") {
                        loop {
                            imported.push(self.name()?);
                            if self.eat(")") {
                                break;
                            }
                            self.need(",")?;
                        }
                    }
                    self.need("{")?;
                    let query = self.query()?;
                    self.need("}")?;
                    Clause::Call(imported, Box::new(query))
                }
            } else if self.eat("UNWIND") {
                let e = self.expr(0)?;
                self.need("AS")?;
                Clause::Unwind(e, self.name()?)
            } else if self.at("WITH") || self.at("RETURN") {
                let terminal = self.eat("RETURN");
                if !terminal {
                    self.need("WITH")?;
                }
                Clause::Project(self.projection()?, terminal)
            } else if self.eat("CREATE") {
                Clause::Create(self.patterns()?)
            } else if self.eat("MERGE") {
                let patterns = self.patterns()?;
                if patterns.len() != 1 {
                    return Err(fail(
                        "MERGE requires one complete path pattern; use separate MERGE clauses",
                    ));
                }
                let mut on_create = Vec::new();
                let mut on_match = Vec::new();
                while self.eat("ON") {
                    let create = self.eat("CREATE");
                    if !create {
                        self.need("MATCH")?;
                    }
                    self.need("SET")?;
                    let items = self.set_items()?;
                    if create {
                        on_create.extend(items);
                    } else {
                        on_match.extend(items);
                    }
                }
                Clause::Merge(patterns, on_create, on_match)
            } else if self.eat("SET") {
                Clause::Set(self.set_items()?)
            } else if self.eat("REMOVE") {
                Clause::Remove(self.exprs()?)
            } else if self.at("DETACH") || self.at("DELETE") {
                let detach = self.eat("DETACH");
                self.need("DELETE")?;
                Clause::Delete(self.exprs()?, detach)
            } else {
                break;
            };
            let yielded = matches!(&clause, Clause::Search(..) | Clause::Fulltext(..));
            clauses.push(clause);
            if yielded && self.eat("WHERE") {
                clauses.push(Clause::Filter(self.expr(0)?));
            }
        }
        if clauses.is_empty() {
            return Err(fail(
                "supported Cypher clause required; procedures are not implemented",
            ));
        }
        let union = if self.eat("UNION") {
            let all = self.eat("ALL");
            Some((all, Box::new(self.query()?)))
        } else {
            None
        };
        Ok(Query { clauses, union })
    }
    /// Shared SET grammar, also used by MERGE's conditional actions.
    fn set_items(&mut self) -> Result<Vec<(Expr, Expr)>, Error> {
        let mut items = Vec::new();
        loop {
            let lhs = self.expr(6)?;
            let rhs = if matches!(lhs, Expr::Label(..)) {
                if self.at("=") {
                    return Err(fail("SET label does not accept an assigned value"));
                }
                Expr::Literal(Value::Bool(true))
            } else {
                self.need("=")?;
                self.expr(0)?
            };
            items.push((lhs, rhs));
            if !self.eat(",") {
                break;
            }
        }
        Ok(items)
    }
    fn exprs(&mut self) -> Result<Vec<Expr>, Error> {
        let mut values = vec![self.expr(0)?];
        while self.eat(",") {
            values.push(self.expr(0)?);
        }
        Ok(values)
    }
    fn projection(&mut self) -> Result<Projection, Error> {
        let distinct = self.eat("DISTINCT");
        let mut items = Vec::new();
        loop {
            let start = self.pos;
            let e = self.expr(0)?;
            let alias = if self.eat("AS") {
                self.name()?
            } else {
                match &e {
                    Expr::Variable(n) => n.clone(),
                    Expr::Property(_, n) => n.clone(),
                    Expr::Star => "*".into(),
                    _ => format!("column_{}", items.len() + 1),
                }
            };
            if self.pos == start {
                return Err(fail("empty projection"));
            }
            items.push((e, alias));
            if !self.eat(",") {
                break;
            }
        }
        let mut order = Vec::new();
        let (mut skip, mut limit, mut filter) = (None, None, None);
        loop {
            if self.eat("ORDER") {
                self.need("BY")?;
                loop {
                    let e = self.expr(0)?;
                    let desc = self.eat("DESC");
                    if !desc {
                        self.eat("ASC");
                    }
                    order.push((e, desc));
                    if !self.eat(",") {
                        break;
                    }
                }
            } else if self.eat("SKIP") {
                skip = Some(self.expr(0)?);
            } else if self.eat("LIMIT") {
                limit = Some(self.expr(0)?);
            } else if self.eat("WHERE") {
                filter = Some(self.expr(0)?);
            } else {
                break;
            }
        }
        Ok(Projection {
            items,
            distinct,
            order,
            skip,
            limit,
            filter,
        })
    }
    fn patterns(&mut self) -> Result<Vec<Pattern>, Error> {
        let mut patterns = vec![self.pattern()?];
        while self.eat(",") {
            patterns.push(self.pattern()?);
        }
        Ok(patterns)
    }
    fn pattern(&mut self) -> Result<Pattern, Error> {
        let variable = if matches!(self.peek(), Token::Name(_))
            && self.tokens.get(self.pos + 1) == Some(&Token::Symbol("=".into()))
        {
            let v = self.name()?;
            self.need("=")?;
            Some(v)
        } else {
            None
        };
        let shortest = self.eat("shortestPath");
        if shortest {
            self.need("(")?;
        }
        let mut nodes = vec![self.node_pattern()?];
        let mut edges = Vec::new();
        while self.at("-") || self.at("<-") {
            edges.push(self.edge_pattern()?);
            nodes.push(self.node_pattern()?);
            if edges.len() > 64 {
                return Err(fail("Cypher pattern depth exceeded"));
            }
        }
        if shortest {
            self.need(")")?;
            if edges.len() != 1 {
                return Err(fail(
                    "shortestPath requires one variable-length relationship",
                ));
            }
        }
        Ok(Pattern {
            variable,
            nodes,
            edges,
            shortest,
        })
    }
    fn props(&mut self) -> Result<Vec<(String, Expr)>, Error> {
        if !self.eat("{") {
            return Ok(vec![]);
        }
        let mut values = Vec::new();
        if self.eat("}") {
            return Ok(values);
        }
        loop {
            let name = match self.peek().clone() {
                Token::String(s) => {
                    self.pos += 1;
                    s
                }
                _ => self.name()?,
            };
            self.need(":")?;
            values.push((name, self.expr(0)?));
            if self.eat("}") {
                break;
            }
            self.need(",")?;
        }
        Ok(values)
    }
    fn node_pattern(&mut self) -> Result<NodePattern, Error> {
        self.need("(")?;
        let variable = if matches!(self.peek(), Token::Name(_)) {
            Some(self.name()?)
        } else {
            None
        };
        let mut labels = Vec::new();
        while self.eat(":") {
            labels.push(self.name()?);
        }
        let properties = self.props()?;
        self.need(")")?;
        Ok(NodePattern {
            variable,
            labels,
            properties,
        })
    }
    fn edge_pattern(&mut self) -> Result<EdgePattern, Error> {
        let incoming = self.eat("<-");
        if !incoming {
            self.need("-")?;
        }
        let mut edge = EdgePattern {
            variable: None,
            types: vec![],
            properties: vec![],
            direction: Direction::Both,
            minimum: 1,
            maximum: 1,
            variable_length: false,
        };
        if self.eat("[") {
            if matches!(self.peek(), Token::Name(_)) {
                edge.variable = Some(self.name()?);
            }
            if self.eat(":") {
                edge.types.push(self.name()?);
                while self.eat("|") {
                    self.eat(":");
                    edge.types.push(self.name()?);
                }
            }
            if self.eat("*") {
                edge.variable_length = true;
                let lo = if let Token::Number(s) = self.peek().clone() {
                    self.pos += 1;
                    Some(s.parse::<usize>().map_err(|_| fail("invalid path bound"))?)
                } else {
                    None
                };
                if self.eat("..") {
                    edge.minimum = lo.unwrap_or(1);
                    let Token::Number(s) = self.peek().clone() else {
                        return Err(fail("finite upper path bound required"));
                    };
                    self.pos += 1;
                    edge.maximum = s.parse().map_err(|_| fail("invalid path bound"))?;
                } else {
                    let n = lo.ok_or_else(|| fail("finite path bound required"))?;
                    edge.minimum = n;
                    edge.maximum = n;
                }
                if edge.minimum > edge.maximum || edge.maximum > 64 {
                    return Err(fail("invalid/excessive path bounds"));
                }
            }
            edge.properties = self.props()?;
            self.need("]")?;
        }
        if incoming {
            self.need("-")?;
            edge.direction = Direction::In;
        } else if self.eat("->") {
            edge.direction = Direction::Out;
        } else {
            self.need("-")?;
        }
        Ok(edge)
    }
    fn expr(&mut self, min: u8) -> Result<Expr, Error> {
        self.depth += 1;
        if self.depth > 64 {
            return Err(fail("Cypher expression depth exceeded"));
        }
        let result = self.expr_inner(min);
        self.depth -= 1;
        result
    }
    fn expr_inner(&mut self, min: u8) -> Result<Expr, Error> {
        let mut lhs = if self.eat("NOT") {
            Expr::Unary("not".into(), Box::new(self.expr(3)?))
        } else if self.eat("-") {
            Expr::Unary("-".into(), Box::new(self.expr(8)?))
        } else if self.eat("+") {
            self.expr(8)?
        } else if self.eat("(") {
            let e = self.expr(0)?;
            self.need(")")?;
            e
        } else if self.eat("[") {
            if matches!(self.peek(), Token::Name(_))
                && matches!(self.tokens.get(self.pos+1),Some(Token::Name(n)) if n.eq_ignore_ascii_case("IN"))
            {
                let var = self.name()?;
                self.need("IN")?;
                let list = self.expr(0)?;
                let cond = if self.eat("WHERE") {
                    Some(Box::new(self.expr(0)?))
                } else {
                    None
                };
                let expr = if self.eat("|") {
                    self.expr(0)?
                } else {
                    Expr::Variable(var.clone())
                };
                self.need("]")?;
                Expr::Comprehension(var, Box::new(list), cond, Box::new(expr))
            } else {
                let values = if self.eat("]") {
                    vec![]
                } else {
                    let v = self.exprs()?;
                    self.need("]")?;
                    v
                };
                Expr::List(values)
            }
        } else if self.at("{") {
            Expr::Map(self.props()?)
        } else if self.eat("CASE") {
            let operand = if self.at("WHEN") {
                None
            } else {
                Some(self.expr(0)?)
            };
            let mut pairs = Vec::new();
            while self.eat("WHEN") {
                let mut cond = self.expr(0)?;
                if let Some(op) = &operand {
                    cond = Expr::Binary("=".into(), Box::new(op.clone()), Box::new(cond));
                }
                self.need("THEN")?;
                pairs.push((cond, self.expr(0)?));
            }
            if pairs.is_empty() {
                return Err(fail("CASE requires WHEN"));
            }
            let fallback = if self.eat("ELSE") {
                self.expr(0)?
            } else {
                Expr::Literal(Value::Null)
            };
            self.need("END")?;
            Expr::Case(pairs, Box::new(fallback))
        } else if self.at("EXISTS")
            && self.tokens.get(self.pos + 1) == Some(&Token::Symbol("{".into()))
        {
            self.need("EXISTS")?;
            self.need("{")?;
            let q = self.query()?;
            self.need("}")?;
            if !q.is_read_only() {
                return Err(fail("EXISTS must be read-only"));
            }
            Expr::Exists(Box::new(q))
        } else {
            let tok = self.peek().clone();
            self.pos += 1;
            match tok {
                Token::String(s) => Expr::Literal(Value::String(s)),
                Token::Number(n) => Expr::Literal(Value::Number(decimal(&n)?)),
                Token::Param(n) => Expr::Parameter(n),
                Token::Symbol(s) if s == "*" => Expr::Star,
                Token::Name(n) if n.eq_ignore_ascii_case("null") => Expr::Literal(Value::Null),
                Token::Name(n)
                    if n.eq_ignore_ascii_case("true") || n.eq_ignore_ascii_case("false") =>
                {
                    Expr::Literal(Value::Bool(n.eq_ignore_ascii_case("true")))
                }
                Token::Name(n) => {
                    if self.eat("(") {
                        let lower = n.to_ascii_lowercase();
                        if ["any", "all", "none", "single"].contains(&lower.as_str()) {
                            let var = self.name()?;
                            self.need("IN")?;
                            let list = self.expr(0)?;
                            self.need("WHERE")?;
                            let cond = self.expr(0)?;
                            self.need(")")?;
                            Expr::Quantifier(lower, var, Box::new(list), Box::new(cond))
                        } else if lower == "reduce" {
                            let accumulator = self.name()?;
                            self.need("=")?;
                            let init = self.expr(0)?;
                            self.need(",")?;
                            let var = self.name()?;
                            self.need("IN")?;
                            let list = self.expr(0)?;
                            self.need("|")?;
                            let expression = self.expr(0)?;
                            self.need(")")?;
                            Expr::Reduce(
                                accumulator,
                                Box::new(init),
                                var,
                                Box::new(list),
                                Box::new(expression),
                            )
                        } else {
                            let distinct = self.eat("DISTINCT");
                            let args = if self.eat(")") {
                                vec![]
                            } else {
                                let args = self.exprs()?;
                                self.need(")")?;
                                args
                            };
                            Expr::Function(lower, args, distinct)
                        }
                    } else {
                        Expr::Variable(n)
                    }
                }
                _ => return Err(fail(format!("Cypher expression required, got {tok:?}"))),
            }
        };
        let mut chain_depth = 0;
        loop {
            chain_depth += 1;
            if chain_depth > 64 {
                return Err(fail("Cypher expression chain depth exceeded"));
            }
            if self.eat(".") {
                lhs = Expr::Property(Box::new(lhs), self.name()?);
                continue;
            }
            if self.eat(":") {
                lhs = Expr::Label(Box::new(lhs), self.name()?);
                continue;
            }
            if self.eat("[") {
                let start = if self.at("..") {
                    None
                } else {
                    Some(Box::new(self.expr(0)?))
                };
                if self.eat("..") {
                    let end = if self.at("]") {
                        None
                    } else {
                        Some(Box::new(self.expr(0)?))
                    };
                    self.need("]")?;
                    lhs = Expr::Slice(Box::new(lhs), start, end);
                } else {
                    self.need("]")?;
                    lhs = Expr::Index(
                        Box::new(lhs),
                        start.ok_or_else(|| fail("list index required"))?,
                    );
                }
                continue;
            }
            let (op, precedence, width) = if self.at("OR") {
                ("or", 1, 1)
            } else if self.at("XOR") {
                ("xor", 2, 1)
            } else if self.at("AND") {
                ("and", 3, 1)
            } else if self.at("IS") {
                ("is", 4, 1)
            } else if self.at("IN") {
                ("in", 4, 1)
            } else if self.at("CONTAINS") {
                ("contains", 4, 1)
            } else if self.at("STARTS") {
                ("starts", 4, 2)
            } else if self.at("ENDS") {
                ("ends", 4, 2)
            } else if ["=", "<>", "!=", "<", ">", "<=", ">="]
                .iter()
                .any(|s| self.at(s))
            {
                let Token::Symbol(s) = self.peek() else {
                    unreachable!()
                };
                (
                    match s.as_str() {
                        "=" => "=",
                        "<>" | "!=" => "<>",
                        "<" => "<",
                        ">" => ">",
                        "<=" => "<=",
                        _ => ">=",
                    },
                    4,
                    1,
                )
            } else if self.at("+") {
                ("+", 6, 1)
            } else if self.at("-") {
                ("-", 6, 1)
            } else if self.at("*") {
                ("*", 7, 1)
            } else if self.at("/") {
                ("/", 7, 1)
            } else {
                break;
            };
            if precedence < min {
                break;
            }
            self.pos += 1;
            if width == 2 {
                self.need("WITH")?;
            }
            if op == "is" {
                let neg = self.eat("NOT");
                self.need("NULL")?;
                lhs = Expr::Unary(
                    if neg { "isnotnull" } else { "isnull" }.into(),
                    Box::new(lhs),
                );
            } else {
                let rhs = self.expr(precedence + 1)?;
                lhs = Expr::Binary(op.into(), Box::new(lhs), Box::new(rhs));
            }
        }
        Ok(lhs)
    }
}
