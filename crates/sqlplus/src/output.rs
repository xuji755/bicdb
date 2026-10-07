//! **输出与 SPOOL**（SQL*Plus 的列格式与 `SPOOL` 语义）。
//!
//! ```text
//!         ID NAME
//! ---------- ----------
//!          1 alpha
//!          2 beta
//!
//! 2 rows selected.
//! ```
//!
//! 三条 SQL*Plus 口径：
//! 1. **数值列右对齐、文本列左对齐**（按列的**形态**判定——引擎给的 `ColKind`，
//!    不靠"字面像不像数"猜：`VARCHAR2` 里存的 `'123'` 仍是文本列）；
//! 2. 表头一行 + 虚线一行，列宽 = max(表头, 取值) 且受 `LINESIZE` 约束；
//! 3. `PAGESIZE` 行一页：翻页时空一行、**重打表头**（`PAGESIZE = 0` 不分页）。

use std::io::Write;
use std::path::{Path, PathBuf};

use bicdb_exec::{ColKind, Value};
use bicdb_sql::session::{format_value, ColumnMeta, QueryResult};

use crate::settings::Settings;

/// 输出面：终端 +（可选）SPOOL 文件。
pub struct Output {
    spool: Option<std::fs::File>,
    spool_path: PathBuf,
}

impl Default for Output {
    fn default() -> Self {
        Self::new()
    }
}

impl Output {
    /// 建输出面（未开 SPOOL）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            spool: None,
            spool_path: PathBuf::new(),
        }
    }

    /// SPOOL 开着吗。
    #[must_use]
    pub fn spooling(&self) -> bool {
        self.spool.is_some()
    }

    /// SPOOL 文件路径（`SPOOL OUT`/`SHOW SPOOL` 用）。
    #[must_use]
    pub fn spool_path(&self) -> &Path {
        &self.spool_path
    }

    /// **`SPOOL <文件>`**：开（重开即换文件；SQL*Plus 会先关旧的）。
    pub fn spool_on(&mut self, path: &Path) -> std::io::Result<()> {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        self.spool = Some(f);
        self.spool_path = path.to_path_buf();
        Ok(())
    }

    /// **`SPOOL OFF`**：关。
    pub fn spool_off(&mut self) {
        if let Some(mut f) = self.spool.take() {
            let _ = f.flush();
        }
    }

    /// 写一行（终端 + SPOOL）。
    ///
    /// `to_term` = 是否上终端：`SET TERMOUT OFF` 或脚本静默路径传 `false`
    /// ——但 **SPOOL 照收**（SQL*Plus 正是这个分工：termout 管屏幕、spool 管文件）。
    pub fn line(&mut self, to_term: bool, text: &str) {
        if to_term {
            println!("{text}");
        }
        if let Some(f) = self.spool.as_mut() {
            let _ = writeln!(f, "{text}");
        }
    }

    /// **一个结果**（结果集 / 影响行数 / DDL / 事务回执）。
    pub fn result(&mut self, s: &Settings, r: &QueryResult, to_term: bool) {
        match r {
            QueryResult::Rows { columns, rows } => {
                self.table(s, columns, rows, to_term);
                if s.feedback {
                    let n = rows.len();
                    let word = if n == 1 { "row" } else { "rows" };
                    self.line(to_term, "");
                    self.line(to_term, &format!("{n} {word} selected."));
                }
            }
            QueryResult::Affected(n) => {
                // SQL*Plus 的 DML 反馈：`1 row updated.` 一类的形态。
                let word = if *n == 1 { "row" } else { "rows" };
                self.line(to_term, &format!("{n} {word} affected."));
            }
            QueryResult::Ddl(t) => self.line(to_term, t),
            QueryResult::Txn(t) => self.line(to_term, t),
        }
    }

    /// 结果集表格（含分页）。
    fn table(&mut self, s: &Settings, columns: &[ColumnMeta], rows: &[Vec<Value>], to_term: bool) {
        if columns.is_empty() {
            return;
        }
        // 取值形态：NULL 按 `SET NULL` 的文本显示（**按值判空**，不是按字面
        // `"NULL"`——那样内容恰为 `NULL` 的文本列会被换掉）；宽度按**显示后**
        // 的字符数算。
        let cell = |v: &Value| -> String {
            if v.is_null() {
                s.null_text.clone()
            } else {
                format_value(v)
            }
        };
        let shown: Vec<Vec<String>> = rows.iter().map(|r| r.iter().map(&cell).collect()).collect();
        // 列宽：表头与取值取大，受 LINESIZE 约束（超出按比例收窄）。
        let mut widths: Vec<usize> = columns.iter().map(|c| c.name.chars().count()).collect();
        for r in &shown {
            for (i, c) in r.iter().enumerate() {
                if i < widths.len() {
                    widths[i] = widths[i].max(c.chars().count());
                }
            }
        }
        // 数值列右对齐（**列的形态**说了算；空列不右对齐）。
        let numeric: Vec<bool> = columns.iter().map(|c| c.kind == ColKind::Number).collect();
        shrink_to(&mut widths, s.linesize, columns.len());

        let header = |o: &mut Self, to_term: bool| {
            let h: Vec<String> = columns
                .iter()
                .enumerate()
                .map(|(i, c)| pad(&c.name, widths[i], false))
                .collect();
            o.line(to_term, h.join(" ").trim_end());
            let d: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
            o.line(to_term, d.join(" ").trim_end());
        };
        header(self, to_term);
        let pagesize = s.pagesize;
        for (i, r) in shown.iter().enumerate() {
            if pagesize > 0 && i > 0 && i % pagesize == 0 {
                self.line(to_term, "");
                header(self, to_term);
            }
            let cells: Vec<String> = (0..columns.len())
                .map(|c| {
                    let v = r.get(c).map(String::as_str).unwrap_or("");
                    pad(v, widths[c], numeric[c])
                })
                .collect();
            self.line(to_term, cells.join(" ").trim_end());
        }
    }
}

/// 按显示宽度补齐（`right` = 右对齐）。
fn pad(text: &str, width: usize, right: bool) -> String {
    let n = text.chars().count();
    if n >= width {
        return text.chars().take(width).collect();
    }
    let fill = " ".repeat(width - n);
    if right {
        format!("{fill}{text}")
    } else {
        format!("{text}{fill}")
    }
}

/// 总宽超过 `linesize` 时按比例收窄（至少留 3 列宽，SQL*Plus 会折行；
/// 我们收窄并在末尾留白——折行会把表格读花，收窄至少不误导）。
fn shrink_to(widths: &mut [usize], linesize: usize, ncols: usize) {
    let gaps = ncols.saturating_sub(1);
    loop {
        let total: usize = widths.iter().sum::<usize>() + gaps;
        if total <= linesize {
            return;
        }
        let Some(max) = widths.iter().max().copied() else {
            return;
        };
        if max <= 3 {
            return;
        }
        if let Some(w) = widths.iter_mut().max_by_key(|w| **w) {
            *w -= 1;
        }
    }
}

/// SQL*Plus 形态的计时（`Elapsed: 00:00:00.01`）。
#[must_use]
pub fn format_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    format!(
        "Elapsed: {:02}:{:02}:{:02}.{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60,
        d.subsec_millis() / 10
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个结果集（列按**字节串**形态——文本列；数值列见 `rows_typed`）。
    fn rows(names: &[&str], data: &[&[&str]]) -> QueryResult {
        QueryResult::Rows {
            columns: names
                .iter()
                .map(|n| ColumnMeta {
                    name: (*n).to_owned(),
                    kind: ColKind::Bytes,
                })
                .collect(),
            rows: data
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|s| Value::Bytes(s.as_bytes().to_vec()))
                        .collect()
                })
                .collect(),
        }
    }

    #[test]
    fn numbers_right_align_and_text_left() {
        assert_eq!(pad("7", 3, true), "  7");
        assert_eq!(pad("ab", 3, false), "ab ");
        assert_eq!(pad("abcd", 3, false), "abc");
        // 中文字符按**字符**计宽（不是字节）。
        assert_eq!(pad("甲乙", 3, false), "甲乙 ");
    }

    #[test]
    fn shrink_respects_linesize() {
        let mut w = vec![50, 50, 50];
        shrink_to(&mut w, 40, 3);
        assert!(w.iter().sum::<usize>() + 2 <= 40, "{w:?}");
        // 已经够窄就不动。
        let mut w2 = vec![3, 3];
        shrink_to(&mut w2, 100, 2);
        assert_eq!(w2, vec![3, 3]);
    }

    #[test]
    fn elapsed_looks_like_sqlplus() {
        let t = std::time::Duration::from_millis(1234);
        assert_eq!(format_elapsed(t), "Elapsed: 00:00:01.23");
    }

    #[test]
    fn rows_result_reports_counts() {
        // `result()` 走终端 + spool：这里只验证不 panic 且空列不炸。
        let mut o = Output::new();
        let s = Settings {
            feedback: false,
            ..Settings::default()
        };
        o.result(&s, &rows(&[], &[]), false);
        o.result(&s, &QueryResult::Affected(2), false);
        o.result(
            &s,
            &QueryResult::Ddl("CREATE TABLE t（1 列）".to_owned()),
            false,
        );
    }
}
