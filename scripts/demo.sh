#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# 本机**测试环境**（`~/bicdb/demo`）的建/起/停/重置
#
# 用途：日常手测、驱动联调、演示——一个**固定位置、固定内容**的实例，
# 不必每次自己 init/start/塞数据。
#
# **形态与真实部署一致**（不只是方便）：`demo/` 就是一台 `BICDB_HOME`——
#   demo/app · demo/control（实例注册表）· demo/public（PUBLIC 工作区）· demo/log · demo/backup
# 所以"管理面"（`user$`/`ws$`/`fs$` 三张自举表）与注册表在这里**都在**，
# DCL（建用户/工作区/文件系统）能在 demo 上真跑。
#
#   scripts/demo.sh create    建区 + 起服务 + 灌测试数据（幂等：已建则跳过 init）
#   scripts/demo.sh start     起服务（没建区就先建）
#   scripts/demo.sh stop [fast|immediate]   停服务（默认 fast = 完全检查点）
#   scripts/demo.sh status    服务/实例状态
#   scripts/demo.sh list      工作区一览（含注册表状态/工作区号）
#   scripts/demo.sh reset     停掉 + 删整个部署根 + 重建（回到初始数据）
#   scripts/demo.sh cli       进 SQL*Plus 形态的客户端（bicdbcli）
#   scripts/demo.sh sql "…"   跑一条 SQL（经服务）
#   scripts/demo.sh py        用 Python 驱动连一下（自检）
#
# **纪律**：`demo/` 是本机测试实例（`/demo/` 已在 .gitignore），**不进仓库**；
# 这份脚本进仓库（位置与口径固定，换机器/换人照跑）。
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# **一台部署根**（与真实安装同形）：`<demo>/public` 是 PUBLIC 工作区。
export BICDB_HOME="${BICDB_DEMO_DIR:-$REPO/demo}"
DEMO="$BICDB_HOME/public"
BIN="${BICDB_BIN:-$REPO/target/release/bicdb}"
CLI="${BICDBCLI_BIN:-$REPO/target/release/bicdbcli}"
LOG="$BICDB_HOME/log/public.log"

say() { printf '%s\n' "$*"; }
die() { printf 'demo：%s\n' "$*" >&2; exit 1; }

need_bins() {
  [ -x "$BIN" ] || die "没找到 $BIN —— 先 \`cargo build --release\`"
}

# 服务在跑吗（`bicdb status` 的退出码不足以判定，看输出更稳）。
serving() {
  "$BIN" status -p public 2>/dev/null | grep -q '^运行中：pid .*服务模式'
}

# 测试数据：小而全（数值/文本/NULL/唯一索引/普通索引/表选项）。
# 幂等：表已存在就跳过（`CREATE TABLE` 没有 IF NOT EXISTS，故先查再建）。
seed() {
  if "$BIN" sql -p public "SELECT id FROM t LIMIT 1" >/dev/null 2>&1; then
    say "测试数据已在（跳过灌数据）"
    return 0
  fi
  say "灌测试数据…"
  "$BIN" sql -p public "
    CREATE TABLE t (id NUMBER NOT NULL, name VARCHAR2(32));
    CREATE UNIQUE INDEX t_pk ON t (id);
    INSERT INTO t VALUES (1, 'alpha');
    INSERT INTO t VALUES (2, 'beta');
    INSERT INTO t VALUES (3, NULL);

    CREATE TABLE emp (empno NUMBER NOT NULL, ename VARCHAR2(20), sal NUMBER);
    CREATE UNIQUE INDEX emp_pk ON emp (empno);
    INSERT INTO emp VALUES (1001, 'SMITH', 800);
    INSERT INTO emp VALUES (1002, 'ALLEN', 1600);
    INSERT INTO emp VALUES (1003, 'WARD', 1250);
    INSERT INTO emp VALUES (1004, 'JONES', 2975);

    CREATE TABLE big (k NUMBER NOT NULL, v VARCHAR2(64)) WITH (pctfree = 20, itl_max = 8);
    CREATE UNIQUE INDEX big_pk ON big (k);
  " >/dev/null
  # 1000 行：**多条语句一次发**（`;` 分隔）——一次往返灌一批，比逐行起进程快
  # 两个数量级；批大小 250 是随手取的（帧上限 64 MiB，远不到）。
  local batch="" i=1
  while [ $i -le 1000 ]; do
    batch="$batch INSERT INTO big VALUES ($i, 'row-$i');"
    if [ $((i % 250)) -eq 0 ]; then
      "$BIN" sql -p public "$batch" >/dev/null
      batch=""
    fi
    i=$((i + 1))
  done
  [ -n "$batch" ] && "$BIN" sql -p public "$batch" >/dev/null
  # 自检：末行在（灌完没灌完，一条查询说话）。
  # 取"最后一个纯数字行"——表格的版面（表头/虚线/值/`（N 行）`）会变，按行号取脆。
  local last
  last=$("$BIN" sql -p public "SELECT k FROM big ORDER BY k DESC LIMIT 1" 2>/dev/null \
    | awk '/^[[:space:]]*[0-9]+[[:space:]]*$/{v=$1} END{print v}')
  [ "$last" = "1000" ] || die "灌数据自检失败：big 的最大 k 是 '$last'（应为 1000）"
  say "测试数据就绪：t(3 行) / emp(4 行) / big(1000 行)"
}

cmd_create() {
  need_bins
  if [ -f "$DEMO/bicdb.ini" ]; then
    say "实例已存在：$DEMO"
  else
    say "建区：$DEMO"
    "$BIN" init >/dev/null          # 无参 = 建 <BICDB_HOME>/public（并登记进注册表）
  fi
  if serving; then
    say "服务已在跑"
  else
    "$BIN" start -p public >/dev/null
    say "服务已启动"
  fi
  seed
  cmd_status
}

cmd_start() {
  need_bins
  [ -f "$DEMO/bicdb.ini" ] || "$BIN" init >/dev/null
  if serving; then say "服务已在跑"; else "$BIN" start -p public >/dev/null; say "服务已启动"; fi
}

cmd_stop() {
  need_bins
  # `-w 300`：完全检查点在大库上要时间（默认取 service.stop_wait_s，这里显式写死更稳）。
  "$BIN" stop -p public -m "${1:-fast}" -w 300
}

cmd_status() {
  need_bins
  "$BIN" status -p public
  say "  部署根  $BICDB_HOME（注册表在 $BICDB_HOME/control）"
  say "  目录    $DEMO"
  say "  参数    $DEMO/bicdb.ini（\`$BIN params -p public\` 看有效值）"
  say "  日志    $LOG"
}

cmd_reset() {
  need_bins
  say "重置：停服务 + 删库 + 重建"
  serving && "$BIN" stop -p public -m immediate -w 300 >/dev/null 2>&1 || true
  # **整个 home 一起删**（含 control/ 注册表）——只删 public 会留下"登记了但目录不在"。
  rm -rf "$BICDB_HOME"
  cmd_create
}

cmd_cli() { need_bins; "$CLI" -p public; }
cmd_sql() { need_bins; [ $# -ge 1 ] || die 'sql 缺语句'; "$BIN" sql -p public "$@"; }

cmd_py() {
  need_bins
  PYTHONPATH="$REPO/drivers/python${PYTHONPATH:+:$PYTHONPATH}" python3 -I - "$DEMO" <<'PY'
import sys
sys.path.insert(0, __import__("os").environ.get("PYTHONPATH", "").split(":")[0])
import bicdb
c = bicdb.connect(sys.argv[1])
cur = c.cursor()
cur.execute("SELECT id, name FROM t ORDER BY id")
print("Python 驱动自检：引擎", c.server_version, "| t =", cur.fetchall())
c.close()
PY
}

case "${1:-}" in
  create) shift; cmd_create "$@" ;;
  list)   shift; need_bins; "$BIN" list "$@" ;;
  start)  shift; cmd_start "$@" ;;
  stop)   shift; cmd_stop "$@" ;;
  status) shift; cmd_status "$@" ;;
  reset)  shift; cmd_reset "$@" ;;
  cli)    shift; cmd_cli "$@" ;;
  sql)    shift; cmd_sql "$@" ;;
  py)     shift; cmd_py "$@" ;;
  *)
    sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
