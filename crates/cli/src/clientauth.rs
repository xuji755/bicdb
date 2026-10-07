//! **客户端侧的认证凭据**（`-U <主体>` + 口令从哪来）。
//!
//! 服务端的认证在 `bicdb-sql::auth`（校验）与守护进程的 `AUTH` 分支（准入）；
//! 这里只管**客户端怎么拿到凭据**——两个客户端（`bicdb sql`、`bicdbcli`）共用。
//!
//! # 口令从哪来（**不给命令行参数**）
//!
//! | 顺序 | 来源 | 为什么 |
//! | --- | --- | --- |
//! | 1 | 环境变量 `BICDB_PASSWORD` | 脚本/自动化用（照 PG `PGPASSWORD` 的口径） |
//! | 2 | 终端提示（`/dev/tty`） | 交互用；**不给 `-P <口令>`**——命令行会进 `ps`/shell 历史 |
//!
//! **回声不屏蔽**（记档，不是疏忽）：屏蔽回声要动 termios（`tcgetattr`/`tcsetattr`），
//! 本仓 `#![forbid(unsafe_code)]`，没有 libc 绑定就做不了。代价有限——口令是
//! **本机套接字**上的（协议 v0.1 的本机形态），提示语里已注明。
//!
//! **不给 `BICDB_PASSWORD` 又开不了 `/dev/tty`** ⇒ **具名拒绝**（不退回读 stdin：
//! `bicdb sql -` 的 SQL 正文就是从 stdin 读的，抢着读会**吃掉 SQL**）。

use std::io::BufRead;

/// 环境变量：口令（照 PG `PGPASSWORD` 的口径）。
pub const ENV_PASSWORD: &str = "BICDB_PASSWORD";

/// 一份认证凭据（主体名 + 口令）。
///
/// **`Debug` 手写**：不让口令跟着 `{:?}` 漏进日志/panic 消息。
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// 主体名。
    pub user: String,
    /// 口令（明文——只应走本机套接字）。
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 口令**不出现**（打出来就是泄露）。
        f.debug_struct("Credentials")
            .field("user", &self.user)
            .field("password", &"<不显示>")
            .finish()
    }
}

/// **从命令行里取 `-U/--user <主体>`**；没给 ⇒ `None`（不认证 = 本机/OS 身份）。
///
/// 口令按上表取（`BICDB_PASSWORD` → 终端提示）。
///
/// # Errors
/// `-U` 给了空串，或口令取不到（无环境变量且无终端）。
pub fn from_args(args: &[String]) -> Result<Option<Credentials>, String> {
    let user = flag_value(args, &["-U", "--user"])?;
    let Some(user) = user else {
        return Ok(None);
    };
    if user.trim().is_empty() {
        return Err("`-U/--user` 给了空主体名".to_owned());
    }
    let user = user.trim().to_owned();
    let password = match std::env::var_os(ENV_PASSWORD) {
        Some(v) => v.to_string_lossy().into_owned(),
        None => prompt_password(&user)?,
    };
    Ok(Some(Credentials { user, password }))
}

/// 取一个旗标的值（`-U x` / `--user=x` 两种写法；都没给 ⇒ `None`）。
fn flag_value(args: &[String], names: &[&str]) -> Result<Option<String>, String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        for n in names {
            if a == n {
                return Ok(Some(
                    it.next()
                        .ok_or_else(|| format!("`{n}` 后面缺主体名"))?
                        .clone(),
                ));
            }
            if let Some(v) = a.strip_prefix(&format!("{n}=")) {
                return Ok(Some(v.to_owned()));
            }
        }
    }
    Ok(None)
}

/// 终端提示读口令（`/dev/tty`；**不回退 stdin**——见模块头）。
fn prompt_password(user: &str) -> Result<String, String> {
    eprint!("{user} 的口令（输入会回显；设 `{ENV_PASSWORD}` 可免提示）：");
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|e| {
            format!(
                "取口令失败：没有 `{ENV_PASSWORD}`，也开不了 /dev/tty（{e}）——\
                 非交互场景请设 `{ENV_PASSWORD}`（**不给命令行参数**：那会进 ps 与 shell 历史）"
            )
        })?;
    let mut line = String::new();
    std::io::BufReader::new(&mut tty)
        .read_line(&mut line)
        .map_err(|e| format!("读 /dev/tty 失败：{e}"))?;
    // 行尾的 `\r\n`/`\n` 都去掉——**只有行尾**（口令里可以有空格）。
    let pw = line.trim_end_matches(['\n', '\r']).to_owned();
    Ok(pw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_forms_are_recognized() {
        let two = vec!["-U".to_owned(), "alice".to_owned(), "SELECT 1".to_owned()];
        assert_eq!(
            flag_value(&two, &["-U", "--user"]).unwrap().unwrap(),
            "alice"
        );
        let eq = vec!["--user=alice".to_owned()];
        assert_eq!(
            flag_value(&eq, &["-U", "--user"]).unwrap().unwrap(),
            "alice"
        );
        let none = vec!["-p".to_owned(), "public".to_owned()];
        assert!(flag_value(&none, &["-U", "--user"]).unwrap().is_none());
        let missing = vec!["-U".to_owned()];
        assert!(flag_value(&missing, &["-U", "--user"]).is_err());
    }

    #[test]
    fn credentials_never_print_the_password() {
        let c = Credentials {
            user: "alice".to_owned(),
            password: "s3cr3t".to_owned(),
        };
        let shown = format!("{c:?}");
        assert!(shown.contains("alice"));
        assert!(!shown.contains("s3cr3t"), "口令不该出现在 {:?} 里", shown);
    }
}
