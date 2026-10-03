#!/usr/bin/env python3
"""批量验证知识库检索命令，输出压缩判定表。完整输出落盘 /tmp/kbcheck/。"""
import os, re, subprocess, sys, json
from concurrent.futures import ThreadPoolExecutor

SCRIPTS = "/u01/deepseek/scripts"
OUT = "/tmp/kbcheck"
os.makedirs(OUT, exist_ok=True)

ENV = dict(os.environ)
ENV["KG_RETRIEVAL_RUNTIME_CONFIG"] = f"{SCRIPTS}/config/kg_retrieval_runtime_config.json"
ENV["PATH"] = "/u01/deepseek/.local/share/kg-retrieval-venv/bin:" + ENV.get("PATH", "")

# (编号, 分组, argv)
CASES = [
    # ---- 阶段检索计划中的 [待验] ----
    ("P0-1", "类型", ["kb_graph.py", "oracle", "entity", "knowledge", "NUMBER 精度 标度 舍入"]),
    ("P0-2", "类型", ["kb_graph.py", "oracle", "problem", "空字符串 NULL 语义"]),
    ("P0-3", "类型", ["doc_retrieve.py", "NLS_LANG AL32UTF8", "--db", "oracle"]),
    ("P0-4", "存储", ["kb_graph.py", "oracle", "entity", "knowledge", "块 区 段 逻辑存储结构"]),
    ("P1-1", "隔离", ["kb_graph.py", "oracle", "entity", "knowledge", "权限 角色 对象权限 最小权限"]),
    ("P1-2", "隔离", ["kb_graph.py", "linux", "problem", "符号链接 路径穿越 权限 提权"]),
    ("P1-3", "隔离", ["kb_graph.py", "ob", "problem", "资源隔离 租户 资源单元 配额"]),
    ("P2-1", "存储", ["kb_graph.py", "postgresql", "problem", "TOAST 大字段 外联存储 压缩"]),
    ("P2-2", "存储", ["kb_graph.py", "oracle", "entity", "knowledge", "行链接 行迁移 链式行"]),
    ("P2-3", "存储", ["kb_graph.py", "postgresql", "entity", "knowledge", "FSM 空闲空间映射 页面复用"]),
    ("P3-1", "恢复", ["kb_graph.py", "postgresql", "problem", "检查点 checkpoint 恢复 重做 起点"]),
    ("P3-2", "恢复", ["kb_graph.py", "oracle", "problem", "ORA-01555 快照过旧"]),
    ("P4-1", "并发", ["kb_graph.py", "oracle", "problem", "死锁 ORA-00060 锁等待"]),
    ("P4-2", "并发", ["kb_graph.py", "oracle", "problem", "latch 闩锁 争用 自旋"]),
    ("P4-3", "并发", ["kb_graph.py", "postgresql", "entity", "knowledge", "唯一索引 并发 插入 冲突"]),
    ("P5-1", "SQL", ["kb_graph.py", "postgresql", "entity", "knowledge", "解析器 原始语法树 语义分析 绑定"]),
    ("P5-2", "SQL", ["kb_graph.py", "oracle", "entity", "operator", "NESTED LOOPS"]),
    ("P5-3", "SQL", ["kb_graph.py", "postgresql", "problem", "JSON 类型 路径 索引 表达式"]),
    ("P5-4", "SQL", ["kb_graph.py", "ob", "problem", "执行计划缓存 失效 绑定变量"]),
    ("P6-1", "资产", ["kb_graph.py", "mysql", "problem", "大对象 外部文件 一致性 校验"]),
    ("P6-2", "资产", ["kb_graph.py", "postgresql", "entity", "knowledge", "级联删除 依赖 引用完整性"]),
    ("P6-3", "资产", ["kb_graph.py", "redis", "problem", "TTL 过期 淘汰 惰性删除"]),
    ("P6-4", "资产", ["kb_graph.py", "postgresql", "entity", "knowledge", "幂等 事务 重试 唯一键"]),
    ("P7-1", "检索", ["kb_graph.py", "postgresql", "problem", "混合检索 融合 排序 RRF"]),
    ("P7-2", "检索", ["kb_graph.py", "postgresql", "entity", "knowledge", "pgvector 距离 余弦 内积 精度"]),
    ("P8-1", "ANN", ["kb_graph.py", "postgresql", "entity", "knowledge", "IVFFlat 训练 nprobe 列表"]),
    ("P10-1", "图", ["kb_graph.py", "postgresql", "entity", "knowledge", "openCypher CREATE MATCH RETURN"]),
    ("P10-2", "图", ["kb_graph.py", "postgresql", "problem", "图遍历 变长路径 深度限制"]),
    ("P11-1", "可靠性", ["kb_graph.py", "postgresql", "problem", "故障注入 崩溃恢复 演练"]),
    ("P11-2", "可靠性", ["kb_graph.py", "oracle", "problem", "RMAN 备份 校验 一致性"]),
    ("P11-3", "可靠性", ["kb_graph.py", "postgresql", "problem", "磁盘满 fsync 慢 数据页 刷盘"]),
    # ---- 附录A 中的 [待验] 工具/子命令 ----
    ("A-1", "子命令", ["kb_graph.py", "ob", "entity", "view", "GV$OB_MEMORY", "--version", "4.2.1", "--scope", "SYS"]),
    ("A-2", "子命令", ["kb_graph.py", "ob", "batch", "合并阻塞", "内存不足", "转储"]),
    ("A-3", "子命令", ["kb_graph.py", "oracle", "diagnose", "索引扫描后回表变慢"]),
    ("A-4", "子命令", ["kb_graph.py", "oracle", "fault", "日志切换缓慢"]),
    ("A-5", "子命令", ["kb_graph.py", "ob", "entity", "opsqllist", "--version", "4.2.1"]),
    ("A-6", "子命令", ["kb_graph.py", "oracle", "analyze", "sysaux 表空间增长过快"]),
    ("A-7", "子命令", ["kb_graph.py", "oracle", "diagnostic", "归档日志空间满"]),
    ("A-8", "子命令", ["kb_graph.py", "oracle", "impact", "view", "GV$SESSION"]),
    ("A-9", "子命令", ["kb_graph.py", "oracle", "discover"]),
    ("A-10", "子命令", ["kb_graph.py", "ob", "equivalent", "GV$OB_MEMORY"]),
    ("A-11", "子命令", ["kb_graph.py", "oracle", "entity", "want", "x"]),  # 负例：未知类型应报错
    ("A-12", "子命令", ["kb_graph.py", "oracle", "entity", "knowledge", "统计信息收集", "--limit", "3"]),
    ("A-13", "工具", ["kg_deep_diagnose.py", "oracle", "日志切换缓慢", "--limit", "10", "--format", "json"]),
    ("A-14", "工具", ["search_kg.py", "--db", "oracle", "执行计划", "--limit", "10", "--json"]),
    ("A-15", "工具", ["search_sqlgen.py", "--db", "oracle", "--category", "SQL改写", "--keyword", "谓词推入"]),
    ("A-16", "工具", ["view_join_search.py", "GV$OB_MEMORY", "--db", "ob", "--trusted-only", "--json"]),
    ("A-17", "工具", ["metric_lookup.py", "oracle", "search", "逻辑读", "--json"]),
    ("A-18", "工具", ["wait_event_info.py", "oracle", "log file sync", "json"]),
    ("A-19", "工具", ["doc_retrieve.py", "shared_pool", "--db", "oracle"]),
    ("A-20", "工具", ["parameter_info.py", "oracle", "shared_pool_size", "pga_aggregate_target"]),
]

PLACEHOLDER = re.compile(r"占位内容|占位|placeholder", re.I)
EMPTY_MARK = re.compile(r"未找到|无结果|未命中|not found|没有找到|无匹配", re.I)
NOISE = re.compile(
    r"^(\s*$|候选覆盖有界|={4,}|─{4,}|📊|🔍?\s*图搜索|\[问题诊断\]|"
    r"三路召回|正文核实|FaultModel：|KnowledgeEntry：|CaseStudy：|📗|📘|📙|📕)"
)


def classify(text, rc):
    if rc != 0:
        return "ERR"
    if PLACEHOLDER.search(text):
        return "PLACEHOLDER"
    if EMPTY_MARK.search(text):
        return "EMPTY"
    return "OK"


def first_lines(text, n=3):
    out = []
    for ln in text.splitlines():
        s = ln.strip()
        if not s or NOISE.match(ln):
            continue
        out.append(s[:150])
        if len(out) >= n:
            break
    return out


def run(case):
    cid, group, argv = case
    cmd = [sys.executable if argv[0].endswith(".py") else argv[0]] if False else ["python3"] + argv
    stdin_data = None
    if "--stdin" in argv:
        stdin_data = "ORA-04031: unable to allocate memory\nORA-00600: internal error code"
    try:
        p = subprocess.run(cmd, cwd=SCRIPTS, env=ENV, capture_output=True, text=True,
                           timeout=120, input=stdin_data)
        text = (p.stdout or "") + (p.stderr or "")
        rc = p.returncode
    except subprocess.TimeoutExpired:
        text, rc = "TIMEOUT", 124
    with open(f"{OUT}/{cid}.txt", "w") as f:
        f.write(text)
    return {
        "id": cid, "group": group, "cmd": " ".join(argv),
        "verdict": classify(text, rc), "rc": rc,
        "chars": len(text), "lines": text.count("\n") + 1,
        "head": first_lines(text),
    }


with ThreadPoolExecutor(max_workers=4) as ex:
    results = list(ex.map(run, CASES))

# 额外的 stdin 用例
extra = [
    ("A-21", "工具", "kg_log_search.py --db oracle --stdin"),
    ("A-22", "工具", "kg_log_search.py --db mysql --no-search 'plain text'"),
    ("A-23", "工具", "plsql_full_check.py --db oracle --sql 'SELECT 1 FROM dual'"),
]
for cid, group, cmdline in extra:
    argv = cmdline.split()
    stdin_data = "ORA-04031: unable to allocate memory\nORA-00600: internal error code" if "--stdin" in argv else None
    try:
        p = subprocess.run(["python3"] + argv, cwd=SCRIPTS, env=ENV, capture_output=True,
                           text=True, timeout=120, input=stdin_data)
        text = (p.stdout or "") + (p.stderr or "")
        rc = p.returncode
    except subprocess.TimeoutExpired:
        text, rc = "TIMEOUT", 124
    with open(f"{OUT}/{cid}.txt", "w") as f:
        f.write(text)
    results.append({"id": cid, "group": group, "cmd": cmdline,
                    "verdict": classify(text, rc), "rc": rc, "chars": len(text),
                    "lines": text.count("\n") + 1, "head": first_lines(text)})

json.dump(results, open(f"{OUT}/report.json", "w"), ensure_ascii=False, indent=1)

print(f"{'ID':<7}{'判定':<12}{'行':>6}  {'分组':<7} 命令 / 首行摘要")
print("-" * 150)
for r in results:
    head = r["head"][0][:88] if r["head"] else "(无有效内容)"
    print(f"{r['id']:<7}{r['verdict']:<12}{r['lines']:>6}  {r['group']:<7} {r['cmd'][:60]}")
    print(f"{'':<26}{head}")
print("-" * 150)
from collections import Counter
print("汇总:", dict(Counter(r["verdict"] for r in results)))
