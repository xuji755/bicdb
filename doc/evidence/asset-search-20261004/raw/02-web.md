# 外部检索摘要（2026-10-04，2 轮 WebSearch）
# 用途：支撑 evidence.md 表 3 / 4。

## 1. pg_trgm：两段式（候选 + recheck）

- `pg_trgm` 的 `gin_trgm_ops` 支持 `%`、`LIKE` / `ILIKE`、`~` / `~*`（PG14+ 还有 `=`）。
- 对 `LIKE` / 正则：**从模式提取 trigram → 查索引 → 得候选**；**GIN 扫描永远是两步**：
  索引给候选行 → **取行重查**真实谓词（EXPLAIN 的 `Rows Removed by Index Recheck`）。
  `pg_trgm` 的 LIKE 策略在"全部 trigram 命中"后返回 **`GIN_MAYBE`**——"全部命中"只是
  **必要条件而非充分条件**；**没有开关能关掉 recheck**（关掉即错）。
- **提取不出 trigram 的模式**（太短等）：退化——**比全索引扫描还差**。
- 假阳性规模示例：10 万行表查 `value % 'lorem'` → 10 万候选全部被 recheck 移除（0 真命中）；
  另一例 recheck 占查询时间 >99%（0.25s 扫描 vs 30.7s recheck）。
- trigram 大小写折叠、非词字符视作边界；相似度阈值 `pg_trgm.similarity_threshold`（默认 0.3）。

链接：
- http://repo.postgrespro.ru/doc/pgpro/9.5.11.1/en/postgres-A4.pdf （consistent/triconsistent 函数与 recheck 语义）
- https://github.com/damusix/skills/blob/main/postgres/references/93-pg-trgm.md
- https://stackoverflow.com/questions/61619608/turn-off-recheck-in-trgm-index

## 2. ripgrep：扫描式搜索的工程形态

- 架构：`rg` 二进制 + `grep-*` 系列 crate——`grep-matcher`（Matcher 抽象）、
  **`grep-searcher`（行式搜索循环：行缓冲、行号、上下文、二进制检测、编码）**、`grep-printer`、`grep-regex`（默认引擎，
  基于 `regex-automata`，SIMD 加速）/可选 PCRE2。
- **行式**：`LineBuffer` 保证 Matcher 总看到"整行"（一行可能横跨两次读）；支持 CRLF 终止符选项。
- **二进制检测**：`BinaryDetection = None / Quit(byte) / Convert(byte)`，默认按 **NUL** 判定、跳过；
  `-a/--text` 强制当文本。**坑**：不用 mmap 时检测覆盖全部被搜索字节；用 mmap 时**只看首 ~64KB 与命中行**
  ——**同一文件可能因内部策略不同而"是否二进制"不一致**（测试注释明确此点）。我们要求**策略无关的确定性**。
- **上下文行**：`-C/-A/-B`；Sink 回调 `matched / context / context_break / binary_data / begin / finish`。
- 其他：`-z/--search-zip`、`--pre` 预处理器（如 PDF 转文本再搜）、`-U/--multiline`。

链接：
- https://github.com/BurntSushi/ripgrep/blob/master/GUIDE.md
- https://deepwiki.com/BurntSushi/ripgrep/2-architecture
- https://factory.ai/open-source-wikis/ripgrep?page=packages%2Fsearcher.md
