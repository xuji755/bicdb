#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# bicdb 卸载：删程序，**保留数据/日志/备份**
#
#   scripts/uninstall.sh [--home <目录>] [--purge] [--force] [--yes]
#
#   --home <目录>  部署根（BICDB_HOME）；不给则取 $BICDB_HOME，再退到
#                  $HOME/bicdb-home > /opt/bicdb > /u01/app/bicdb（与 install.sh 同序）
#   --purge        连 public/（数据）、log/、backup/ 一起删（**会二次确认**）
#   --force        有工作区在跑时也照卸（默认拒绝——卸载不该把跑着的库的启动器删掉）
#   --yes          不确认（脚本化用；--purge 时请先想清楚）
#
# 默认只删**程序**（`app/`）；数据（`public/`）、日志（`log/`）、备份（`backup/`）
# **保留**——卸载工具不该顺手毁掉用户的库（要毁得明说，还要确认）。
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

HOME_DIR="${BICDB_HOME:-}"
PURGE=0
FORCE=0
YES=0

while [ $# -gt 0 ]; do
  case "$1" in
    --home|--base) HOME_DIR="${2:?--home 缺目录}"; shift 2 ;;
    --purge) PURGE=1; shift ;;
    --force) FORCE=1; shift ;;
    --yes) YES=1; shift ;;
    -h|--help) sed -n '3,19p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) printf '卸载：不认识的选项 `%s`（--help 看用法）\n' "$1" >&2; exit 2 ;;
  esac
done

say() { printf '%s\n' "$*"; }
die() { printf '卸载：%s\n' "$*" >&2; exit 1; }

if [ -z "$HOME_DIR" ]; then
  for cand in "$HOME/bicdb-home" /opt/bicdb /u01/app/bicdb; do
    [ -d "$cand" ] && { HOME_DIR="$cand"; break; }
  done
fi
[ -n "${HOME_DIR:-}" ] || die "没找到部署根——用 --home <目录> 指定（或 export BICDB_HOME）"
[ -d "$HOME_DIR" ] || die "$HOME_DIR 不存在"
# 认得出是 bicdb home 的三种形态：完整安装（app/）、**卸了一半**（只剩 public/ 或
# log/，上一轮默认卸载留下的）。只认第一种会让"卸了程序、想再 --purge 清数据"
# 变成死路（实测试出来的）。
[ -d "$HOME_DIR/app" ] || [ -d "$HOME_DIR/public" ] || [ -d "$HOME_DIR/log" ] \
  || die "$HOME_DIR 不像 bicdb home（没有 app/public/log）——用 --home 指对地方"

VGREP="$(grep -m1 '^version: ' "$HOME_DIR/app/VERSION" 2>/dev/null || echo 'version: ?')"
say "卸载：$HOME_DIR（$VGREP）"

# **在跑的实例**：卸掉 bin/ 就把它们的管理入口抽走了（服务进程还在，但你没法
# 再 stop/status 了）。默认拒绝，让用户先停——用实例自己的二进制问一次状态
# （此刻它还在），不在这层重写"判活"逻辑。
if [ -x "$HOME_DIR/app/bin/bicdb" ]; then
  running="$(BICDB_HOME="$HOME_DIR" "$HOME_DIR/app/bin/bicdb" list 2>/dev/null \
    | awk '$2=="运行中"||$2=="直连中"{print $1}' | tr '\n' ' ')"
  if [ -n "$running" ]; then
    say "  以下工作区**还在跑**：$running"
    if [ "$FORCE" != 1 ]; then
      say "  先停掉（\`$HOME_DIR/app/bin/bicdb stop -p <名字>\`）再加 --force 重来。"
      exit 1
    fi
    if [ "$PURGE" = 1 ]; then
      for n in $running; do
        BICDB_HOME="$HOME_DIR" "$HOME_DIR/app/bin/bicdb" stop -p "$n" -m immediate -w 30 >/dev/null 2>&1 || true
      done
      say "  （--purge：已尽力停掉它们）"
    else
      say "  （--force：继续卸载——它们的服务进程会失去管理入口）"
    fi
  fi
fi

if [ "$PURGE" = 1 ]; then
  if [ "$YES" != 1 ]; then
    # 数一数要毁掉什么，让确认有意义（不是一句干巴巴的"确定吗"）。
    n=0
    if [ -d "$HOME_DIR/public" ]; then n=$((n + 1)); fi
    extra=$(find "$HOME_DIR" -maxdepth 1 -mindepth 1 -type d 2>/dev/null \
      | grep -vE '/(app|log|backup|public)$' | wc -l)
    n=$((n + extra))
    printf '  --purge 会**连数据一起删**：%s 下有 %s 个工作区、以及 log/ backup/\n' \
      "$HOME_DIR" "$n"
    printf '  确认删除请输入 yes：'
    read -r ans
    [ "$ans" = "yes" ] || { say "已取消（什么都没删）"; exit 1; }
  fi
  rm -rf "$HOME_DIR/public" "$HOME_DIR/log" "$HOME_DIR/backup"
  # 额外工作区（`bicdb init <名字>` 建的）也一并清掉；保留 app/ 之外只留空根。
  for d in "$HOME_DIR"/*/; do
    [ -f "${d}bicdb.ini" ] && rm -rf "$d"
  done
  say "  已删数据/日志/备份：public/ log/ backup/（+ 额外工作区）"
fi

# 删**程序**（app/）；说清楚"没删到什么"——静默 no-op 比报错更糟。
if [ -d "$HOME_DIR/app" ]; then
  rm -rf "$HOME_DIR/app"
  say "  已删程序：app/"
else
  say "  程序：没有 app/——没有可卸的程序（数据仍按下面处理）"
fi
if [ -d "$HOME_DIR/public" ] || [ -d "$HOME_DIR/log" ] || [ -d "$HOME_DIR/backup" ]; then
  say "  保留：$( [ -d "$HOME_DIR/public" ] && printf 'public/ ' )$( [ -d "$HOME_DIR/log" ] && printf 'log/ ' )$( [ -d "$HOME_DIR/backup" ] && printf 'backup/ ' )（要一起清就用 --purge）"
fi
# 空壳就顺手收掉（非空则留着，不硬删）
rmdir "$HOME_DIR" 2>/dev/null && say "  目录已空，一并删除：$HOME_DIR" || true
