//! **会话设置**（SQL*Plus 的 `SET` / `SHOW` 参数）。
//!
//! 只收**有落点**的参数——收下却没人读的参数就是空壳（本项目刚为此做过一轮
//! 全仓审计）。每个参数在实现里都写明"谁读它"。
//!
//! | 参数 | 取值 | 谁读 |
//! | --- | --- | --- |
//! | `ECHO` | on/off | 执行缓冲前回显（`/` 重跑时最有用） |
//! | `FEEDBACK` | on/off | 结果集后的 `N rows selected.` |
//! | `TIMING` | on/off | 每条语句后的 `Elapsed: …` |
//! | `PAGESIZE` | n（0 = 不分页） | 结果集分页（每 n 行重复表头） |
//! | `LINESIZE` | n | 结果集列宽上限与分隔线长度 |
//! | `NULL` | 文本 | NULL 的显示形态 |
//! | `TERMOUT` | on/off | 脚本执行时是否往终端回显（`@file` 用） |
//! | `SQLPROMPT` | 文本 | 主提示符 |
//! | `VERIFY` | on/off | 替换变量（`&name`）替换后回显该行 |
//! | `SPOOL` | 见 `spool.rs` | —— |
//! | `WHENEVER SQLERROR` | EXIT/CONTINUE | 脚本里 SQL 失败时的行为 |

/// `SET` 参数表。
#[derive(Debug, Clone)]
pub struct Settings {
    /// 执行前回显缓冲。
    pub echo: bool,
    /// 结果集后的行数反馈。
    pub feedback: bool,
    /// 计时。
    pub timing: bool,
    /// 页大小（0 = 不分页）。
    pub pagesize: usize,
    /// 行宽。
    pub linesize: usize,
    /// NULL 的显示文本。
    pub null_text: String,
    /// 脚本执行时是否回显到终端。
    pub termout: bool,
    /// 主提示符。
    pub sqlprompt: String,
    /// 替换变量替换后回显。
    pub verify: bool,
    /// 脚本遇错退出。
    pub sqlerror_exit: bool,
    /// 替换变量（`&name`）的前缀。
    pub concat: char,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            echo: false,
            feedback: true,
            timing: false,
            pagesize: 24,
            linesize: 120,
            null_text: String::new(),
            termout: true,
            sqlprompt: "SQL> ".to_owned(),
            verify: false,
            sqlerror_exit: false,
            concat: '&',
        }
    }
}

/// `SET` 的结果。
#[derive(Debug, PartialEq, Eq)]
pub enum SetOutcome {
    /// 已设置（供回显）。
    Ok(String),
    /// 参数不认识。
    Unknown(String),
    /// 取值非法。
    BadValue(String),
}

impl Settings {
    /// **`SET <名> <值>`**（SQL*Plus 的写法；`SET TIMING ON` 也接受裸 `SET TIMING`）。
    pub fn set(&mut self, name: &str, value: &str) -> SetOutcome {
        let n = name.to_ascii_lowercase();
        let v = value.trim();
        let on = |v: &str| -> Option<bool> {
            match v.to_ascii_lowercase().as_str() {
                "on" => Some(true),
                "off" => Some(false),
                _ => None,
            }
        };
        match n.as_str() {
            "echo" => self.bool_param(v, on, |s, b| s.echo = b, "ECHO"),
            "feedback" => self.bool_param(v, on, |s, b| s.feedback = b, "FEEDBACK"),
            "timing" => self.bool_param(v, on, |s, b| s.timing = b, "TIMING"),
            "termout" => self.bool_param(v, on, |s, b| s.termout = b, "TERMOUT"),
            "verify" => self.bool_param(v, on, |s, b| s.verify = b, "VERIFY"),
            "pagesize" => match v.parse::<usize>() {
                Ok(n) if n <= 50_000 => {
                    self.pagesize = n;
                    SetOutcome::Ok(format!("pagesize {n}"))
                }
                _ => SetOutcome::BadValue(format!("PAGESIZE 要 0–50000 的数，给的是 `{v}`")),
            },
            "linesize" => match v.parse::<usize>() {
                Ok(n) if (1..=32_767).contains(&n) => {
                    self.linesize = n;
                    SetOutcome::Ok(format!("linesize {n}"))
                }
                _ => SetOutcome::BadValue(format!("LINESIZE 要 1–32767 的数，给的是 `{v}`")),
            },
            "null" => {
                self.null_text = v.to_owned();
                SetOutcome::Ok(format!("null `{v}`"))
            }
            "sqlprompt" => {
                self.sqlprompt = v.to_owned();
                SetOutcome::Ok(format!("sqlprompt `{v}`"))
            }
            "concat" => match v.chars().next() {
                Some(c) if v.chars().count() == 1 && !c.is_alphanumeric() => {
                    self.concat = c;
                    SetOutcome::Ok(format!("concat `{c}`"))
                }
                _ => SetOutcome::BadValue("CONCAT 要一个非字母数字字符".to_owned()),
            },
            "whenever" => SetOutcome::Unknown(
                "WHENEVER 是独立命令（`WHENEVER SQLERROR EXIT|CONTINUE`）".to_owned(),
            ),
            other => SetOutcome::Unknown(format!("SET 不支持参数 `{other}`")),
        }
    }

    fn bool_param(
        &mut self,
        v: &str,
        on: impl Fn(&str) -> Option<bool>,
        apply: impl Fn(&mut Self, bool),
        shown: &str,
    ) -> SetOutcome {
        match if v.is_empty() {
            Some(true) // SQL*Plus：裸 `SET FEEDBACK` = ON
        } else {
            on(v)
        } {
            Some(b) => {
                apply(self, b);
                SetOutcome::Ok(format!("{shown} {}", if b { "ON" } else { "OFF" }))
            }
            None => SetOutcome::BadValue(format!("{shown} 要 ON/OFF，给的是 `{v}`")),
        }
    }

    /// **`SHOW [参数]`**：参数名 + 取值（一列对齐，SQL*Plus 形态）。
    #[must_use]
    pub fn show(&self, which: Option<&str>) -> Vec<(String, String)> {
        let all = vec![
            ("echo".to_owned(), onoff(self.echo)),
            ("feedback".to_owned(), onoff(self.feedback)),
            ("timing".to_owned(), onoff(self.timing)),
            ("pagesize".to_owned(), self.pagesize.to_string()),
            ("linesize".to_owned(), self.linesize.to_string()),
            ("null".to_owned(), format!("`{}`", self.null_text)),
            ("termout".to_owned(), onoff(self.termout)),
            ("sqlprompt".to_owned(), format!("`{}`", self.sqlprompt)),
            ("verify".to_owned(), onoff(self.verify)),
            (
                "whenever sqlerror".to_owned(),
                if self.sqlerror_exit {
                    "EXIT".to_owned()
                } else {
                    "CONTINUE".to_owned()
                },
            ),
            ("concat".to_owned(), format!("`{}`", self.concat)),
        ];
        match which {
            None => all,
            Some(w) => {
                let w = w.to_ascii_lowercase();
                all.into_iter().filter(|(k, _)| k.starts_with(&w)).collect()
            }
        }
    }
}

fn onoff(b: bool) -> String {
    if b {
        "ON".to_owned()
    } else {
        "OFF".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_show_round_trip() {
        let mut s = Settings::default();
        assert_eq!(
            s.set("timing", "on"),
            SetOutcome::Ok("TIMING ON".to_owned())
        );
        assert!(s.timing);
        assert_eq!(
            s.set("PAGESIZE", "0"),
            SetOutcome::Ok("pagesize 0".to_owned())
        );
        assert_eq!(s.pagesize, 0);
        // 裸 `SET FEEDBACK` = ON（SQL*Plus 口径）。
        s.feedback = false;
        assert_eq!(
            s.set("feedback", ""),
            SetOutcome::Ok("FEEDBACK ON".to_owned())
        );
        assert!(s.feedback);
        // 取值非法要具名拒绝。
        assert!(matches!(s.set("timing", "也许"), SetOutcome::BadValue(_)));
        assert!(matches!(s.set("没这个参数", "1"), SetOutcome::Unknown(_)));
        // SHOW 支持前缀过滤。
        let rows = s.show(Some("page"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "pagesize");
    }

    #[test]
    fn concat_must_be_a_single_punctuation() {
        let mut s = Settings::default();
        assert_eq!(
            s.set("concat", "#"),
            SetOutcome::Ok("concat `#`".to_owned())
        );
        assert!(matches!(s.set("concat", "ab"), SetOutcome::BadValue(_)));
        assert!(matches!(s.set("concat", "a"), SetOutcome::BadValue(_)));
    }
}
