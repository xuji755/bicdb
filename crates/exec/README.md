# bicdb-exec

执行算子：火山拉取、行式、扫描/回表内部批量

| 项 | 值 |
| --- | --- |
| 设计依据 | `doc/执行算子设计_v0.1.md`（PG 拉取模型 + Oracle 批量化技巧；证据包 `exec-ops-20261006`） |
| 对应阶段 | P5（切片 1 已落地） |
| 当前状态 | **v0.5**——v0.4 + **去重与集合运算**（`Append`/`Unique`/`SetOp`：UNION [ALL] / INTERSECT [ALL] / EXCEPT [ALL]，排序归并路线、NULL 等价类、重数语义） |

## 状态说明

- **模型**（`doc/执行算子设计_v0.1.md` §1）：**火山拉取**（`open`/`next`/
  `rescan`/`close`，一次一行；短路自然）+ 行式；**批量化只在扫描与回表内部**
  （区读 ≤ 8 页 + 批量回表 + 每块一次 CR——落在 `bicdb-storage::scan`）。
  并行/向量化明确不做（SQL 面清单外；向量化绑定列存层）。
- **四条纪律**：执行器不碰页（行经存储服务，**可见性由服务负责**）；读路径
  不含锁；deadline/取消贯穿每个 `next()`（`ExecContext::check`）；计划不可变、
  执行状态单次新造（每次执行新建算子树）。
- **算子**（切片 1）：`SeqScan`（拉存储行游标，逐行解码）、`Filter`
  （三值逻辑，只放行 TRUE）、`Project`（表达式求值）、`Limit`
  （`LIMIT/OFFSET`；**无 ORDER BY 时是真短路**——不再调子）。
- **值域**（切片 1 子集）：`Value::{Null, Bool, Number, Bytes}` + `RowShape`
  （列形态由列定义给出，行内不自描述）；行布局约定 = 全变长区（`fixed_len=0`），
  定长列/完整类型域随 TYP 切片扩展。比较：NUMBER 数值序、BOOLEAN 假<真、
  BYTES 字节序（`CONV` §1.3）；NULL 参与的比较 ⇒ UNKNOWN。
- **直译执行器**（`direct::execute_direct`）：逐行按语义求值、不建哈希、
  不下推、不用索引——与算子执行器**共享语义查询对象、不共享执行**
  （`SelectQuery::to_plan` 把同一份语义转成计划）。
- **测试**（`tests/differential.rs`，真段 + 真缓冲池 + 真撤销链）：
  两路执行逐行差分（全表/谓词/投影/LIMIT+OFFSET/IS NULL/空结果/空表）、
  **LIMIT 真短路**（扫描行数可观测：`SeqScan` 统计 = LIMIT）、取消与截止
  （两路同判定）、NULL 三值逻辑。

**下一步（切片 6 起）**：`HashJoin` + 溢出（temp 段 + 完整 WMM：分配/缩减/三态）；DML（切片 7）——见设计 §7 切片表。

实现进度请以仓库根 `README.md` 的阶段表为准。
