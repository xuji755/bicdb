//! 驱动的错误面（**分得清"连不上"和"语句错"**——调用的处置完全不同）。

use std::path::PathBuf;

/// 驱动错误。
#[derive(Debug)]
pub enum Error {
    /// **连不上**（套接字不存在/被拒；实例多半没在跑）。
    Connect {
        /// 试过的套接字路径。
        path: PathBuf,
        /// 底层原因。
        source: std::io::Error,
    },
    /// **实例在忙**（服务一次只服务一条连接；另一条连接正占着）——
    /// **可重试**，与"连不上"分开判。
    Busy {
        /// 套接字路径。
        path: PathBuf,
    },
    /// **寻址失败**（找不到/读不了 `bicdb.ini`）。
    Discover {
        /// 原始说明（已含试过的位置）。
        why: String,
    },
    /// **协议层**（帧非法、版本不兼容、载荷解不开）——这条连接已不可信。
    Protocol {
        /// 说明。
        why: String,
    },
    /// **服务端报错**（`ERR` 的**原文**——服务的错误是给人看的具名文本，
    /// 驱动不改写、不翻译、不截断）。
    Server {
        /// 原文。
        message: String,
    },
    /// 想取结果集，但这条语句给的是别的（影响行数/DDL/事务回执）。
    NotResultSet {
        /// 实际拿到的是什么（中文短名）。
        got: &'static str,
    },
    /// 想取原生类型，但这一列/这个值的形态不符。
    Type {
        /// 列名（取不到时是列序号）。
        column: String,
        /// 要的形态。
        wanted: &'static str,
        /// 实际的形态。
        got: &'static str,
    },
    /// 本地 I/O（trace 输出等）。
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Connect { path, source } => write!(
                f,
                "连不上实例：{}：{source}（服务没在跑？`bicdb start -p <根区目录>`）",
                path.display()
            ),
            Error::Busy { path } => write!(
                f,
                "实例正忙：{} 上的服务一次只服务一条连接（V1.0 单写者）——稍后重试",
                path.display()
            ),
            Error::Discover { why } => f.write_str(why),
            Error::Protocol { why } => write!(f, "协议错：{why}"),
            Error::Server { message } => f.write_str(message),
            Error::NotResultSet { got } => {
                write!(f, "这条语句不是结果集（得到：{got}）——要结果集请用 `query`")
            }
            Error::Type {
                column,
                wanted,
                got,
            } => write!(f, "列 `{column}` 不是 {wanted}（实际是 {got}）"),
            Error::Io(e) => write!(f, "I/O：{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Connect { source, .. } => Some(source),
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// 服务端报错（`ERR`）：**判据是"服务说得上话，但拒绝了这次请求"**——
/// 连接仍可继续用（除 `SHUTDOWN`）。
impl Error {
    /// 是不是"实例在忙"（**可重试**；其余错误按各自语义处置）。
    #[must_use]
    pub fn is_busy(&self) -> bool {
        matches!(self, Error::Busy { .. })
    }

    /// 是不是服务端的具名错误（对应 SQL 层的失败）。
    #[must_use]
    pub fn is_server(&self) -> bool {
        matches!(self, Error::Server { .. })
    }

    /// 服务端的错误原文（不是服务端报错 ⇒ `None`）。
    #[must_use]
    pub fn server_message(&self) -> Option<&str> {
        match self {
            Error::Server { message } => Some(message),
            _ => None,
        }
    }
}
