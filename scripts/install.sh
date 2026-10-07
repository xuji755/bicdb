#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# bicdb 安装：一个 BICDB_HOME，四个固定目录（**运维不用到处找**）
#
#   scripts/install.sh [--home <目录>] [--no-build] [--force]
#
#   --home <目录>   部署根。不给则挑第一个**可写**的（装完打印是哪个）：
#                     /u01/app/bicdb  →  /opt/bicdb  →  $HOME/bicdb-home
#   --no-build      不重新 cargo build（用现有 target/release）
#   --force         目标已存在且**不像 bicdb home** 时也照装（默认拒绝）
#
# 布局（doc/安装布局_v0.1.md）：
#   <BICDB_HOME>/
#   ├── app/        程序（只读；升级 = 换这个目录）
#   │   ├── bin/    bicdb · bicdbcli · db_check · page_dump
#   │   ├── share/  doc/（手册与协议）· examples/（示例脚本）
#   │   └── VERSION
#   ├── public/     **PUBLIC 工作区**（数据：control/ · wal/ · data/）——`bicdb init` 建
#   ├── log/        数据库日志（按工作区命名：public.log）
#   └── backup/     默认的备份包落地处
#
# **纪律**：重复安装只覆盖 `app/`——`public/`、`log/`、`backup/` **一动不动**。
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOME_DIR="${BICDB_HOME:-}"
BUILD=1
FORCE=0

while [ $# -gt 0 ]; do
  case "$1" in
    --home|--base) HOME_DIR="${2:?--home 缺目录}"; shift 2 ;;   # --base 兼容上一版
    --no-build) BUILD=0; shift ;;
    --force) FORCE=1; shift ;;
    -h|--help) sed -n '3,26p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) printf '安装：不认识的选项 `%s`（--help 看用法）\n' "$1" >&2; exit 2 ;;
  esac
done

say() { printf '%s\n' "$*"; }
die() { printf '安装：%s\n' "$*" >&2; exit 1; }

# 可写判据：目录已存在，**或**最近的已存在祖先可写（能一路 mkdir 出来）。
writable_below() {
  local p="$1"
  while [ ! -e "$p" ]; do p="$(dirname "$p")"; [ "$p" = "/" ] && break; done
  [ -d "$p" ] && [ -w "$p" ]
}
if [ -z "$HOME_DIR" ]; then
  for cand in /u01/app/bicdb /opt/bicdb "$HOME/bicdb-home"; do
    if [ -d "$cand" ] || writable_below "$cand"; then
      HOME_DIR="$cand"; WHY="$cand（第一个可写的惯例位置）"; break
    fi
  done
else
  WHY="$HOME_DIR（--home 指定）"
fi
[ -n "${HOME_DIR:-}" ] || die "挑不出可写的部署根——用 --home <目录> 显式指定"
HOME_DIR="$(readlink -f "$(dirname "$HOME_DIR")")/$(basename "$HOME_DIR")"   # 归一化

[ "$BUILD" = 1 ] && { say "构建（cargo build --release --workspace）…"; (cd "$REPO" && cargo build --release --workspace >/dev/null); }

BINS=(bicdb bicdbcli db_check page_dump)
for b in "${BINS[@]}"; do
  [ -x "$REPO/target/release/$b" ] || die "没找到 $REPO/target/release/$b —— 先构建（不加 --no-build）"
done
# `--no-build` 的常见坑：源码比产物新 ⇒ 装上去的是旧二进制（实测踩过两次）。
# 比对 `crates/` 下最新的 .rs 与 `bicdb` 二进制的 mtime，旧了就**拦住**。
if [ "$BUILD" = 0 ]; then
  newest_src="$(find "$REPO/crates" -name '*.rs' -newer "$REPO/target/release/bicdb" 2>/dev/null | head -1)"
  if [ -n "$newest_src" ]; then
    die "--no-build 但源码比 target/release 新（如 ${newest_src#"$REPO"/}）——先 cargo build --release，或去掉 --no-build"
  fi
fi

# 目标：不存在就建；已存在则要求"像 bicdb home"（有 app/ 或 VERSION）或 --force
if [ -e "$HOME_DIR" ]; then
  [ -d "$HOME_DIR" ] || die "$HOME_DIR 不是目录"
  if [ ! -e "$HOME_DIR/app" ] && [ "$FORCE" != 1 ]; then
    die "$HOME_DIR 已存在，且不像 bicdb home（没有 app/）——确认无误后加 --force"
  fi
else
  mkdir -p "$HOME_DIR"
fi

VERSION="$(cd "$REPO" && grep -m1 '^version' Cargo.toml | sed 's/.*= *"//; s/"//')"
PLATFORM="$(uname -m)-$(uname -s)"

mkdir -p "$HOME_DIR/app/bin" "$HOME_DIR/app/share/doc" "$HOME_DIR/app/share/examples" \
         "$HOME_DIR/log" "$HOME_DIR/backup"

# ① 程序（只读；升级 = 换这一份）
for b in "${BINS[@]}"; do
  install -m 0755 "$REPO/target/release/$b" "$HOME_DIR/app/bin/$b"
done
for f in docs/使用手册.md docs/客户端协议_v0.1.md docs/platform-support.md; do
  [ -f "$REPO/$f" ] && install -m 0644 "$REPO/$f" "$HOME_DIR/app/share/doc/$(basename "$f")"
done
# 示例：**不是模板**——参数文件的权威来源是二进制里的 `bicdb init`（见安装布局 §0）
cat > "$HOME_DIR/app/share/examples/load.sql" <<'SQL'
-- 示例脚本（bicdbcli 跑：`bicdbcli -p public @load.sql`）
-- 注意：**参数文件的模板不在 share/ 里**——`bicdb init` 现场生成的那份才是权威
-- （版本随二进制走；抄一份出来改，改了不生效）。
CREATE TABLE emp (empno NUMBER NOT NULL, ename VARCHAR2(20), sal NUMBER);
CREATE UNIQUE INDEX emp_pk ON emp (empno);
INSERT INTO emp VALUES (1001, 'SMITH', 800);
INSERT INTO emp VALUES (1002, 'ALLEN', 1600);
SELECT empno, ename, sal FROM emp ORDER BY empno;
SQL

# ② VERSION + 安装记录
{
  echo "version: $VERSION"
  echo "platform: $PLATFORM"
  echo "installed: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  echo "source: $REPO"
} > "$HOME_DIR/app/VERSION"
printf '%s install %s %s -> %s\n' \
  "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$VERSION" "$PLATFORM" "$HOME_DIR" \
  >> "$HOME_DIR/log/install.log"

say "已安装：$HOME_DIR（$WHY）"
say "  程序    $HOME_DIR/app/{bin,share}（版本 $VERSION / $PLATFORM）"
say "  数据    $HOME_DIR/public        （\`bicdb init\` 建 PUBLIC 工作区）"
say "  日志    $HOME_DIR/log/          （各工作区一个：public.log）"
say "  备份    $HOME_DIR/backup/       （默认落地处；生产请指到另一块盘）"
say
say "下一步："
say "  export BICDB_HOME=$HOME_DIR"
say "  export PATH=\"$HOME_DIR/app/bin:\$PATH\""
say "  bicdb home                      # 程序/数据/日志/备份四条路径"
say "  bicdb init && bicdb start -p public && bicdb list"
