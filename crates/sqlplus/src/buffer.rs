//! **SQL 缓冲区与编辑命令**（SQL*Plus 的当前缓冲区语义）。
//!
//! ```text
//! SQL> SELECT id        ← 输入即入缓冲
//!   2  FROM t           ← 续行提示符 = 行号（SQL*Plus 同款）
//!   3  ;
//!                       ← 执行后缓冲**保留**（`/` 可重跑）
//! SQL> /
//! ```
//!
//! **三条语义（照 SQL*Plus）**：
//! 1. `;` 结尾 ⇒ 执行；**空行** ⇒ 结束输入**但不执行**（缓冲区保留）；
//! 2. 单独一行 `/` ⇒ **重跑缓冲区**（不回显内容，除非 `SET ECHO ON`）；
//! 3. 缓冲是**行的列表**：`LIST`/`DEL`/`APPEND`/`INPUT`/`CHANGE` 按**行号**
//!    （1 起）编辑，行号即续行提示符上显示的那个数。

/// 当前缓冲区（按行）。
#[derive(Debug, Default, Clone)]
pub struct SqlBuffer {
    lines: Vec<String>,
}

impl SqlBuffer {
    /// 空缓冲。
    #[must_use]
    pub fn new() -> Self {
        Self { lines: Vec::new() }
    }

    /// 行数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// 是不是空的。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// 追加一行（输入路径）。
    pub fn push(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    /// 取全部文本（执行/重跑用）。
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = self.lines.join("\n");
        out.push('\n');
        out
    }

    /// 清空（`CLEAR BUFFER`）。
    pub fn clear(&mut self) {
        self.lines.clear();
    }

    /// 第 `n` 行（1 起）。
    #[must_use]
    pub fn get(&self, n: usize) -> Option<&str> {
        n.checked_sub(1)
            .and_then(|i| self.lines.get(i))
            .map(String::as_str)
    }

    /// 插入一行到第 `n` 行之后（`n = 0` = 插到最前）。
    pub fn insert_after(&mut self, n: usize, line: impl Into<String>) {
        let at = n.min(self.lines.len());
        self.lines.insert(at, line.into());
    }

    /// 在第 `n` 行**追加**文本（`APPEND`：追加到当前行末尾）。
    pub fn append_to(&mut self, n: usize, text: &str) {
        if let Some(l) = n.checked_sub(1).and_then(|i| self.lines.get_mut(i)) {
            l.push_str(text);
        }
    }

    /// 删一行（1 起）。
    pub fn delete(&mut self, n: usize) {
        if let Some(i) = n.checked_sub(1) {
            if i < self.lines.len() {
                self.lines.remove(i);
            }
        }
    }

    /// 替换第 `n` 行（`CHANGE` 用）。
    pub fn set(&mut self, n: usize, line: impl Into<String>) {
        if let Some(l) = n.checked_sub(1).and_then(|i| self.lines.get_mut(i)) {
            *l = line.into();
        }
    }

    /// 全部行（`LIST` 用）。
    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// **一句话完了吗**：整段里最后一段非注释代码以 `;` 结尾。
    ///
    /// 行注释（`--` 到行尾）不参与判定——`SELECT 1; -- 说明` 必须算完。
    #[must_use]
    pub fn ends_with_semicolon(text: &str) -> bool {
        let mut last = String::new();
        for line in text.lines() {
            let code = line.split_once("--").map_or(line, |(c, _)| c);
            if !code.trim().is_empty() {
                last = code.trim_end().to_owned();
            }
        }
        last.ends_with(';')
    }
}

/// **`CHANGE /旧/新/` 的解析**（SQL*Plus 的分隔符规则：首个非空字符即分隔符）。
///
/// 返回 `(旧, 新, 尾标志)`：`尾标志 = Some('*')` 表示把**剩余整行**接在后面
/// （SQL*Plus 的 `C /a/b/*` 语义）。
#[must_use]
pub fn parse_change(arg: &str) -> Option<(String, String, Option<char>)> {
    let trimmed = arg.trim_end();
    let delim = trimmed.trim_start().chars().next()?;
    let body = trimmed.trim_start().strip_prefix(delim)?;
    let (old, rest) = body.split_once(delim)?;
    let (new, tail) = match rest.split_once(delim) {
        Some((n, t)) => (n.to_owned(), t.trim()),
        None => (rest.to_owned(), ""),
    };
    let tail_flag = tail.chars().next().filter(|c| !c.is_whitespace());
    Some((old.to_owned(), new, tail_flag))
}

/// **`LIST` 的范围解析**：`L`、`L 3`、`L 2 5`、`L 2 *`、`L *`、`L LAST`。
#[must_use]
pub fn parse_range(arg: &str, len: usize) -> Option<(usize, usize)> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let last = len.max(1);
    match parts.as_slice() {
        [] => Some((1, last)),
        [only] => {
            if *only == "*" {
                Some((1, last))
            } else if only.eq_ignore_ascii_case("last") {
                Some((last, last))
            } else {
                let n: usize = only.parse().ok()?;
                Some((n, n))
            }
        }
        [a, b] => {
            let from = if *a == "*" { 1 } else { a.parse().ok()? };
            let to = if *b == "*" || b.eq_ignore_ascii_case("last") {
                last
            } else {
                b.parse().ok()?
            };
            Some((from, to))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semicolon_ignores_trailing_line_comments() {
        assert!(SqlBuffer::ends_with_semicolon("SELECT 1;"));
        assert!(SqlBuffer::ends_with_semicolon("SELECT 1; -- 说明"));
        assert!(SqlBuffer::ends_with_semicolon("SELECT 1\n  FROM t\n  ;\n"));
        assert!(!SqlBuffer::ends_with_semicolon("SELECT 1"));
        assert!(!SqlBuffer::ends_with_semicolon("SELECT 1 -- ;"));
    }

    #[test]
    fn change_uses_the_first_non_space_char_as_delimiter() {
        assert_eq!(
            parse_change("/id/ID/"),
            Some(("id".to_owned(), "ID".to_owned(), None))
        );
        assert_eq!(
            parse_change("#a#b#"),
            Some(("a".to_owned(), "b".to_owned(), None))
        );
        // 尾标志（SQL*Plus 的 `C /a/b/*` 形态）。
        let (old, new, tail) = parse_change("/a/b/*").expect("解析");
        assert_eq!((old.as_str(), new.as_str(), tail), ("a", "b", Some('*')));
        assert!(parse_change("").is_none());
    }

    #[test]
    fn range_forms_follow_sqlplus() {
        assert_eq!(parse_range("", 5), Some((1, 5)));
        assert_eq!(parse_range("*", 5), Some((1, 5)));
        assert_eq!(parse_range("3", 5), Some((3, 3)));
        assert_eq!(parse_range("2 4", 5), Some((2, 4)));
        assert_eq!(parse_range("LAST", 5), Some((5, 5)));
        assert_eq!(parse_range("2 *", 5), Some((2, 5)));
        assert_eq!(parse_range("xx", 5), None);
    }

    #[test]
    fn edit_operations_are_line_numbered() {
        let mut b = SqlBuffer::new();
        b.push("SELECT id");
        b.push("FROM t");
        assert_eq!(b.len(), 2);
        b.append_to(2, " -- 尾注");
        assert_eq!(b.get(2), Some("FROM t -- 尾注"));
        b.insert_after(1, "  ");
        assert_eq!(b.get(2), Some("  "));
        b.set(2, "   , name");
        assert_eq!(b.get(2), Some("   , name"));
        b.delete(2);
        assert_eq!(b.get(2), Some("FROM t -- 尾注"));
        b.clear();
        assert!(b.is_empty());
    }
}
