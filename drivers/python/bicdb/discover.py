"""**客户端侧寻址**（找到控制套接字；与服务端同一口径）。

```text
-p <参数文件|根区目录>（这里是函数参数） > 环境变量 BICDB_INI > 当前目录 ./bicdb.ini
```

只读两项：``[instance] db_root``（相对套接字的解析基准）与 ``[service] socket``。
**权威解析在服务端**（`bicdb-cli` 的 `config`：闭集校验、类型、建区期核对）——
这里刻意**不做**那些校验（两份实现必然漂移），未知节/键直接忽略。
"""

from __future__ import annotations

import os
from pathlib import Path

#: 参数文件名（与 `bicdb-cli::config::FILE_NAME` 同源）。
FILE_NAME = "bicdb.ini"

#: 环境变量（与服务端一致）。
ENV_INI = "BICDB_INI"


class DiscoverError(Exception):
    """找不到/读不了参数文件。"""


def find_ini(target: object = None) -> Path:
    """定位参数文件（``target`` = 参数文件或根区目录，或 ``None``）。"""
    candidates: list[Path] = []
    if target is not None:
        candidates.append(Path(str(target)))
    env = os.environ.get(ENV_INI, "").strip()
    if env:
        candidates.append(Path(env))
    candidates.append(Path(FILE_NAME))
    tried: list[Path] = []
    for cand in candidates:
        path = cand / FILE_NAME if cand.is_dir() else cand
        if not path.exists():
            tried.append(path)
            continue
        return path
    listing = "；".join(str(p) for p in tried)
    raise DiscoverError(
        f"找不到参数文件 {FILE_NAME}——试过：{listing}"
        f"（用 -p <参数文件|根区目录> 或环境变量 {ENV_INI}）"
    )


def read_instance(ini: Path) -> tuple[Path, str]:
    """读 ``(db_root, socket 项)``（socket 缺项按默认名）。"""
    try:
        text = ini.read_text(encoding="utf-8", errors="replace")
    except OSError as e:
        raise DiscoverError(f"读 {ini}：{e}") from e
    section = ""
    root: str | None = None
    sock = "bicdb.sock"
    for raw in text.splitlines():
        code = raw.split("#", 1)[0].split(";", 1)[0].strip()
        if not code:
            continue
        if code.startswith("["):
            section = code.strip("[]").strip().lower()
            continue
        if "=" not in code:
            continue
        key, value = code.split("=", 1)
        key = key.strip().lower()
        value = value.strip().strip('"')
        if section == "instance" and key == "db_root":
            root = value
        elif section == "service" and key == "socket":
            sock = value
    if root is None:
        raise DiscoverError(f"{ini} 里缺 `[instance] db_root`（它注册根区目录）")
    return Path(root), sock


def socket_for(target: object = None) -> Path:
    """**找控制套接字**（绝对路径）。"""
    ini = find_ini(target)
    # 参数文件的位置是权威来源之一：显式给了**目录**时按该目录解析相对项；
    # 否则按参数文件内容里的 `db_root`（它注册的就是根区）。
    root, sock = read_instance(ini)
    path = Path(sock)
    return path if path.is_absolute() else root / path
