# NUMA 第二级绑定设计 v0.1

> 摘要见 `doc/arch/05-页.md`（§5.10"NUMA 第二级绑定"）；本文是**详设**——
> 配置模型、cgroup 形态、线程模型、内存放置、重绑定协议、降级分类、
> 诊断与测试策略、实施切片。**一次 FFI 都不引（零 unsafe）**：
> 全部经 sysfs / proc / cgroup 的**文件读写**完成。
>
> 定位重申：**本地性是优化，不是正确性**。未配置、探测失败、绑定失败
> ⇒ 一律退化为普通调度且**结果不变**；绑定不得进入任何正确性路径。

## 1. 目标链路与不变量

```text
工作区 ──▶ 工作集（§5.10 分区）──▶ NUMA 节点
             ├─ 帧区间按集独立分配（START/END_BUF 语义），首次触碰落本地   [P4 切片]
             ├─ 本工作区的日志缓冲（LogBuffer，每工作区独立）同落该节点    [P4 切片]
             ├─ 承载该工作区会话的执行线程 ∈ cpuset                        [本切片]
             ├─ 该集的写线程与该工作区的 LGWR 角色 ∈ 同一 cpuset           [P4 切片]
             └─ （可选）该工作区 WAL/数据文件放节点本地设备                 [运维]
```

**不变量**：
1. **正确性无关**：任何绑定失败都不得改变读写结果、可见性、恢复语义；
2. **幂等**：重复 `ensure`/`bind` 无副作用；
3. **可退**：任何时刻允许整条链路退化为"未绑定"（重启、换机器、换内核）；
4. **可诊断**：状态（每个工作区 → 节点 → cgroup 路径；降级原因）必须可读。

## 2. 拓扑与探测

- **节点枚举**：`/sys/devices/system/node/` 下的 `nodeN/` 目录
  （`nodeN/cpulist` = 该节点 CPU 列表；`nodeN/state` 或 `node/online`
  判定在线）。**无 `nodeN` 目录 = 非 NUMA 机器**（单节点）⇒ 绑定无意义、
  按"未配置"处理。
- **cpulist 解析**（纯函数）：`"0-3,8,10-11"` → `[0,1,2,3,8,10,11]`；
  上界防御（`ranges` 不超过 4096 个 CPU，越界即 `BadTopology`）。
- **sysfs 根可注入**（默认 `/sys/devices/system/node`）——测试用假目录树。

## 3. cgroup 形态与准备

### 3.1 版本探测

- `/proc/self/cgroup`：v2 为 `0::/path`；v1 为 `N:控制器1,控制器2:/path`。
  **v1 必须包含 `cpuset` 控制器**（否则该环境没有 cpuset ⇒ 降级）。
- 挂载点：从 `/proc/mounts` 找——v2：`cgroup2` 类型行；v1：任一
  `- cgroup ... cpuset ...` 行（取 cpuset 控制器所在挂载点）。
- 两个探测输入（`/proc/self/cgroup` 与 `/proc/mounts` 的**路径**）
  都可注入——测试指向假文件。

### 3.2 v1：直接可做

v1 的 cpuset 支持**线程级**绑定：把 tid 写进 `<组>/tasks`（追加）。
组目录由我们创建（`cpuset.cpus`、`cpuset.mems` 各写一行）。

### 3.3 v2：需要 threaded 子树（kernel 的硬规则）

v2 的 domain cgroup **不允许只搬进程内某一个线程**（"all threads or none"）。
线程级绑定必须走 **threaded 子树**：

```text
<mount>/bicdb/                 ← 我们的根（domain）
   cgroup.subtree_control      ← 写 "+cpuset"（让子层能用 cpuset）
bicdb/bicdb-node<N>/           ← 每节点一组
   cgroup.subtree_control      ← 写 "+cpuset"
   cpuset.cpus / cpuset.mems   ← 该节点 CPU 列表 / 节点 id
bicdb/bicdb-node<N>/threads/   ← 叶（threaded）
   cgroup.type                 ← 写 "threaded"
   cgroup.threads              ← 绑定落点（追加 tid）
```

**准备顺序**（`ensure_node_group`）：建根 → 建节点组 → 写节点组的
`cgroup.subtree_control` → 建 `threads` 叶 → 写叶的 `cgroup.type=threaded`
→ 写节点组的 `cpuset.cpus/mems`。任一步失败 ⇒ **降级**并记录**是哪一步**。

> **运维预置形态（推荐）**：生产部署由编排系统预先建好上述树并**委托**
> cpuset 控制器（`systemd` slice: `AllowedCPUs=`/`AllowedMemoryNodes=`）。
> 提供两个模式：`Provision`（我们建，需要 `cgroup.subtree_control` 权限）
> 与 `AttachExisting`（只使用已存在的 `<root>/bicdb-node<N>`，不写任何
> 控制文件——**检查通过才绑定**，否则降级）。默认 `AttachExisting`：
> 权限最小化，容器化部署可用。

### 3.4 v2 不可用时的替代（明确记录，不实现）

- `sched_setaffinity`（FFI，违反零 unsafe——**不采用**）；
- 每工作区**独立进程**（以 `cgroup.procs` 绑进程）：架构级改动，作为
  将来"多进程 worker"路线的备选注记，不在本设计内。

## 4. 配置模型

```rust
pub struct NumaConfig {
    /// 总开关（默认 false——单节点机器毫无意义）。
    pub enabled: bool,
    /// 绑定模式：Provision（自建 cgroup 树）/ AttachExisting（用预置树）。
    pub mode: BindMode,
    /// cgroup 根（v2 默认 /sys/fs/cgroup/bicdb；v1 为 cpuset 挂载点下的目录）。
    pub cgroup_root: PathBuf,
    /// sysfs 节点根（默认 /sys/devices/system/node）。
    pub sysfs_root: PathBuf,
    /// cgroup 版本探测输入（默认 /proc/self/cgroup、/proc/mounts；测试可注入）。
    pub proc_self_cgroup: PathBuf,
    pub proc_mounts: PathBuf,
    /// 工作区 → 节点 的显式映射（缺省的工作区 = 不绑定）。
    pub assignments: Vec<(WorkspaceId, u32)>,
}
```

**校验（`validate`）**：
- `enabled` 时 `assignments` 非空，且每个节点在拓扑里**在线**；
- 同一工作区不得映射到两个节点（重复项 ⇒ 明确错误）；
- 节点 CPU 列表与 `cpuset.mems` 由拓扑给出，**不从配置里取**（防止
  配置与硬件漂移）；配置里只写"工作区 → 节点号"。

**默认值 = 全关**：`NumaConfig::default()` 为 `enabled: false`，任何路径
不得因为"NUMA 未配置"而产生行为差异。

## 5. 线程模型与绑定点

| 阶段 | 谁被绑 | 绑定点 | 状态 |
| --- | --- | --- | --- |
| **A（本切片）** | 执行工作区任务的**池线程** | `Supervisor::submit` 的任务闭包：运行 task 前，把**当前线程**绑到该工作区节点组（线程级缓存去抖：同一线程连续跑同一工作区的任务不重复写） | 可做、可测 |
| B（P4） | 每工作区的**执行线程**与**写线程**、LGWR 角色 | 线程**创建时**即入组（创建线程的函数先写 `cgroup.threads/tasks` 再启动） | 随分片/线程化 |

**A 的代价面**：共享池线程会在工作区之间迁移（每次绑定 = 一次
`cgroup.threads` 写 + 内核迁移 + 该线程后续分配的本地性漂移）。因此
A 的定位是"**能让多路机器今天就受益的近似**"，B 才是终态（每工作区线程
天然不迁移）。线程级缓存把连续同工作区任务的重复写降为 0。

**会话侧**：会话把任务提交给 `submit(&WorkspaceContext, ..)`——上下文
已有工作区 ⇒ 绑定自动生效，**接入层不需要知道 NUMA 的存在**。

## 6. 内存放置

| 结构 | 放置途径 | 状态 |
| --- | --- | --- |
| 日志缓冲（LogBuffer 页池） | **惰性按需分配、用完回收**——页在首次使用时分配；线程已在节点组内 ⇒ **首次触碰即本地落位**，零额外代码 | 随 A 生效 |
| 缓冲池帧区间 | **按集独立分配**（P4 结构前提，`START/END_BUF` 语义）✅ **已落地（2026-10-05）**：每分区一段独立分配 + 帧的页缓冲**惰性分配**（装页的线程 = 首次触碰者）——"先触碰再分配"取代"构造时全部零页"（旧形态下构造线程替所有帧触了内存，落位与所属线程无关） | ✅ 已实现 |
| 会话工作内存（行缓存等） | 同 A：线程在哪、分配在哪 | 随 A 生效 |

**容量纪律**：节点预算 = Σ(该节点工作集的帧区间) + Σ(该节点工作区日志缓冲)
+ 余量。两份容量都是每工作区配置项。**不设硬内存上限**（cgroup
`memory.max` 不用——远程分配 ≪ swap；`memory.high` 仅作告警）。

## 7. 重绑定协议（节点迁移/重配置，P4 切片）

```text
状态机：Bound(node) → Draining → Rebinding → Bound(node')
                      ↑ 无在途会话后触发
Draining：flush_workspace（脏页按序写回，§11.7）
          + 丢弃全部净帧（缓冲是副本，可重建——不是数据）
Rebinding：更新 工作集 → 节点/cgroup 绑定
          + 在新组内重新分配帧区间（首次触碰落新节点）
```

**不搬内存**。崩溃/重启不涉及本协议（绑定是纯运行期状态，磁盘格式零改动）。

**实现（2026-10-05）**：栈的两半都已就位——

- **池侧（`bicdb-storage`）**：`BufferPool::drain_partition`（Draining ①②：
  按写列表序刷尽 → 丢净帧并**释放页缓冲**）、`drop_clean_frames`（净帧释放的
  独立入口）、`allocated_frames`（诊断/核对）；仍有脏帧或钉住帧 ⇒
  `BufferError::DrainBlocked`（**不静默丢帧**）。`pinned` 判据当前不可达
  （卫兵持分区闩锁），留作 O2（守卫 ≠ 持锁）的预置。
- **绑定侧（`bicdb-daemon`）**：`NumaBinder::rebind(workspace, node)`——新节点组
  按需准备（与启动同一路径）、映射更新、**世代号（`epoch`）自增**。世代号是
  线程级缓存的**跨线程失效**手段：缓存键 =（绑定器实例号, 节点, 世代），
  `rebind` 之后所有线程下次绑定时重写 cgroup——否则"缓存命中"会让线程留在
  旧节点组里。
- **顺序纪律（调用方）**：先 `drain_partition`（该工作区所在**分区**——分区是
  帧区间的单位），再 `rebind`，最后放行；其间**无在途会话**（池契约）。
  端到端用例（假 cgroup + 真缓冲池，`crates/daemon/tests/numa.rs`）钉住：
  脏页先落盘 → 帧缓冲释放 → 改绑 → 再绑定落新组 → 重新装入从盘重建。

## 8. 降级与错误分类（全部具名，绝不静默）

| 情形 | 行为 | 诊断原因 |
| --- | --- | --- |
| 未配置（`enabled=false`） | 跳过 | `Disabled` |
| 非 NUMA 机器（无 nodeN） | 跳过 | `NoTopology` |
| 无 cpuset 控制器 / 挂载点找不到 | 跳过 | `NoCpuset` |
| v2 但线程叶不可用（无 threaded 权限） | 跳过 | `ThreadedUnavailable` |
| 某一步写入失败（权限/不存在） | **跳过该工作区的绑定**，记录 **哪一步** | `Prepare { step, source }` |
| 绑定某次写失败 | 该次任务**照常执行**，记录失败计数与最后一次原因 | `Bind { source }` |
| 工作区未在 `assignments` 里 | 跳过 | `Unassigned` |

**禁止**：任何降级都不得让任务失败、不得改变结果、不得重试风暴
（绑定失败后**本次任务不再重试**；下次任务自然重试一次）。

## 9. 诊断

- `BindingStatus`：`Disabled / Degraded { reason } / Enabled { groups:
  Vec<(node, cgroup_path, cpus, mems)>, assignments }` —— 由
  `Supervisor::numa_status()` 暴露；
- **绑定失败计数**与最后错误（`Mutex<Option<..>>`），不伪造本地/远程
  命中统计（以 `numastat` / `perf c2c` 为准——设计口径，不在库内编）。

## 10. 测试策略

- **全部纯函数化 + 路径注入**，CI 不需要真 cgroup 权限：
  - `parse_cpulist` / `detect_cgroup_version` / `find_cpuset_mount` 纯函数用例；
  - `Topology::discover(假 sysfs 目录树)`（含离线节点、坏文件）；
  - `NumaBinder` 对**假 cgroup 根**（tempdir）的写序列断言：
    v2（subtree_control → 节点组 → threads 叶 → cgroup.type → cpuset.cpus/mems）
    与 v1（节点组直接 + tasks 落点）；幂等重入；
  - 降级矩阵逐项（8 的每一行一个用例）；
  - 监督器集成：fake cgroup + fake proc 输入 → `submit` 一个任务 →
    断言假 `cgroup.threads`/`tasks` 文件里出现的 tid = **任务所在线程**的 tid
    （任务内用 `/proc/thread-self` 读自己的 tid），且**任务照常完成**；
- **真机冒烟**（不进 CI，运维验收用）：双 socket 机器上 `numastat -p` 观察
  绑定进程的本地命中比例。

## 11. 实施切片

| 切片 | 内容 | 状态 |
| --- | --- | --- |
| **本批（阶段 A）** | 拓扑探测 + cgroup 探测/准备/绑定原语 + 配置校验 + `Supervisor::submit` 绑定点（线程级缓存）+ 降级矩阵 + 诊断 | ✅ **已实现**（`bicdb-daemon` v0.2 / `crates/daemon/src/numa.rs`；单元用例 8 + 集成用例 4——假 sysfs/proc/cgroup 上验证"任务线程的 tid 落进该工作区节点的 `cgroup.threads`"、降级矩阵、任务照常执行） |
| P4-a | 工作集分区（N>1）落地时：帧区间按集独立分配；`NumaBinder` 的组准备改由**写线程创建方**调用 | **帧区间 ✅ 已实现（2026-10-05）**——每分区独立分配 + 帧页缓冲惰性分配（首次触碰 = 装页线程）；"组准备由写线程创建方调用"待 P4-b（写线程本身还没按分区存在） |
| P4-b | 每工作区线程化：A 的"任务级绑定"升级为"线程创建即入组"；写线程/LGWR 同组 | 🚧 待实现（随"写线程按分区绑定"；依赖 §5.10 的分区写线程） |
| P4-c | 重绑定状态机（Draining → Rebinding） | ✅ **已实现（2026-10-05）**：池侧 `drain_partition`/`drop_clean_frames`（Draining ①②，脏/钉住即 `DrainBlocked`）+ `NumaBinder::rebind`（Rebinding：组按需准备、映射更新、**世代号自增**失效线程缓存）；端到端用例（假 cgroup + 真缓冲池）钉住 Draining → Rebinding → 重新装入次序。切片表本身的"状态机对象"（显式 `Bound/Draining/Rebinding` 三态）留到有常驻驱动方（分区写线程）时成型——当前是**调用方驱动**的两步协议 |

### 实现注记（阶段 A 落地，2026-10-05）

- 模块：`crates/daemon/src/numa.rs`（纯函数：`parse_cpulist` /
  `detect_cgroup_version` / `find_cpuset_mount`；`Topology::discover`；
  `NumaBinder::{start, bind_tid, bind_current_thread}`；`NumaConfig::validate`）；
- 绑定点：`Supervisor::submit` 的任务闭包——运行 task 前调用
  `bind_current_thread(workspace)`（线程级缓存去抖；失败只记
  `failures/last_error`，**任务照常执行**）；
- 诊断：`Supervisor::numa_status()` → `Disabled / Degraded{reason} / Enabled{...}`；
- 权限最小化默认：`BindMode::AttachExisting`（预置树，不写控制文件）；
  `Provision` 供测试与自有权限环境；
- **零 unsafe**：`/proc/thread-self` 取 tid、cgroup 文件写绑定、sysfs 读拓扑；
- 测试：单元（解析/探测/树准备/降级矩阵）+ 集成（`tests/numa.rs`：
  绑定落点 tid 一致性、未分配不写、降级不失败、默认关闭）。

## 12. 引用的既有设计

- §5.10（工作集/分区、P4 并发路线、NUMA 摘要）：`doc/arch/05-页.md`；
- §11.5.5（每工作区日志缓冲的独立性——"整个日志缓冲可绑节点"的前提）；
- 证据包 `doc/evidence/buffercache-mech-20261005/`（raw 47–52：
  `kcbwds.PROC_GROUP`/`START_BUF#/END_BUF#`/`DBWR_NUM`、`_DB_BLOCK_NUMA`、
  `_NUMA_INSTANCE_MAPPING`、`X$KSMNIM/X$KSMNS`、Linux 本地优先分配）。
