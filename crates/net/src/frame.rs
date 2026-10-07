//! **帧**：长度前缀 + 文本载荷（本机协议的传输层）。
//!
//! ```text
//! 请求： VERB \\n <载荷字节数> \\n <载荷字节>
//! 应答： OK|ERR \\n <载荷字节数> \\n <载荷字节>
//! ```
//!
//! **为什么长度前缀**：SQL 与结果里必然有换行——按行读会在第一行就断错；
//! 长度前缀让"载荷是任意字节"成立。
//!
//! **纪律（照 MySQL 包序号错乱的教训）**：读**必须读满**声明的字节数
//! （`read_exact`），读到一半就当对端出错——绝不"宽容地"按行补读，
//! 那会让后续帧整体错位且**看不出错**。长度上限见 [`MAX_FRAME`]。

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

/// 单帧载荷上限（64 MiB）：坏客户端不该让对端分配无界内存。
pub const MAX_FRAME: u32 = 64 * 1024 * 1024;

/// 首行上限（动词/状态行）。
const MAX_HEAD_LINE: usize = 1024;

/// 帧错误。
#[derive(Debug)]
pub enum FrameError {
    /// I/O（含对端提前关闭）。
    Io(std::io::Error),
    /// 帧格式非法（缺行/长度越界/非 UTF-8）。
    Bad(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "连接：{e}"),
            FrameError::Bad(w) => write!(f, "协议帧非法：{w}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<std::io::Error> for FrameError {
    fn from(e: std::io::Error) -> Self {
        FrameError::Io(e)
    }
}

/// 写一帧（`head` = 首行：请求是动词，应答是 `OK`/`ERR`）。
pub fn write_frame(s: &mut UnixStream, head: &str, payload: &str) -> Result<(), FrameError> {
    write_frame_bytes(s, head, payload.as_bytes())
}

/// 写一帧（载荷是字节——**结果集里的原始字节不经 UTF-8**）。
pub fn write_frame_bytes(s: &mut UnixStream, head: &str, payload: &[u8]) -> Result<(), FrameError> {
    let mut out = Vec::with_capacity(payload.len() + head.len() + 16);
    out.extend_from_slice(head.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(payload.len().to_string().as_bytes());
    out.push(b'\n');
    out.extend_from_slice(payload);
    s.write_all(&out)?;
    s.flush()?;
    Ok(())
}

/// 读一帧（字节载荷；调用方按自己的口径解释）。
pub fn read_frame_bytes(s: &mut UnixStream) -> Result<(String, Vec<u8>), FrameError> {
    let head = read_line(s)?;
    let len_line = read_line(s)?;
    let len: u32 = std::str::from_utf8(&len_line)
        .map_err(|_| FrameError::Bad("长度行非 UTF-8".to_owned()))?
        .trim()
        .parse()
        .map_err(|_| FrameError::Bad("长度行不是数".to_owned()))?;
    if len > MAX_FRAME {
        return Err(FrameError::Bad(format!(
            "载荷 {len} 字节超过上限 {MAX_FRAME}"
        )));
    }
    let mut body = vec![0u8; len as usize];
    // **读满**（不宽容）——半帧即错，绝不把后续帧读串。
    s.read_exact(&mut body)?;
    Ok((String::from_utf8_lossy(&head).trim().to_owned(), body))
}

/// 读一帧（文本载荷；UTF-8 非严格——文本协议内的字段都是 ASCII 边框）。
pub fn read_frame(s: &mut UnixStream) -> Result<(String, String), FrameError> {
    let (head, body) = read_frame_bytes(s)?;
    Ok((head, String::from_utf8_lossy(&body).into_owned()))
}

fn read_line(s: &mut UnixStream) -> Result<Vec<u8>, FrameError> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        s.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            return Ok(out);
        }
        out.push(byte[0]);
        if out.len() > MAX_HEAD_LINE {
            return Err(FrameError::Bad("首行过长".to_owned()));
        }
    }
}
