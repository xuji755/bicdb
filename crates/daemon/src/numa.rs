//! **NUMA 第二级绑定**（详设：`doc/numa绑定设计_v0.1.md`；摘要见 §5.10）。
//!
//! 目标链路：工作区 → 工作集 → NUMA 节点；**承载该工作区会话的执行线程
//! 被放进该节点的 cpuset**（本切片），帧区间与日志缓冲的内存放置随 P4
//! 分片/线程化落地（详设 §6）。
//!
//! # 纪律
//!
//! - **本地性是优化，不是正确性**：未配置/探测失败/绑定失败 ⇒ 退化为普通
//!   调度，**结果不变**；绑定失败不得让任务失败；
//! - **零 unsafe**：全部经 sysfs / proc / cgroup 的**文件读写**完成
//!   （不用 `mbind`/`set_mempolicy`/`sched_setaffinity`）；
//! - **可注入**：拓扑根、cgroup 根、`/proc/self/cgroup` 与 `/proc/mounts`
//!   的路径都可注入——测试用假目录/假文件，CI 不需要真 cgroup 权限；
//! - **具名降级**（详设 §8）：每一种不可用都有名字，绝不静默。

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use bicdb_workspace::WorkspaceId;

/// cgroup 版本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupVersion {
    /// cgroup v1（`/proc/self/cgroup` 形如 `N:cpuset:/path`；线程级绑定 = `tasks`）。
    V1,
    /// cgroup v2（`0::/path`；线程级绑定需 **threaded 子树**，详设 §3.3）。
    V2,
}

/// 绑定模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BindMode {
    /// **使用预置树**（默认；权限最小化）：只检查 `<root>/bicdb-node<N>` 可用，
    /// 不写任何控制文件。
    #[default]
    AttachExisting,
    /// **自建树**：写 `cgroup.subtree_control` / `cgroup.type` 等控制文件——
    /// 需要相应权限，失败即降级（详设 §3.3 的步骤依次具名）。
    Provision,
}

/// NUMA 绑定错误（全部具名；`Display` 即降级原因文案）。
#[derive(Debug)]
pub enum NumaError {
    /// 环境不支持（非 Linux / 无 cpuset / 无 threaded 子树权限等）。
    Unsupported(&'static str),
    /// 非 NUMA 机器（无 `nodeN` 目录）——绑定无意义。
    NoTopology,
    /// 拓扑或配置文件损坏。
    BadTopology(&'static str),
    /// 文件读写失败（含"哪一步/哪个路径"）。
    Io {
        /// 出错的路径。
        path: PathBuf,
        /// 底层错误。
        source: io::Error,
    },
    /// `AttachExisting` 模式下节点组未预置或不可用。
    NotPrepared {
        /// 节点号。
        node: u32,
        /// 原因（缺哪个文件/不可读）。
        reason: &'static str,
    },
}

impl std::fmt::Display for NumaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NumaError::Unsupported(s) => write!(f, "环境不支持 NUMA 绑定：{s}"),
            NumaError::NoTopology => f.write_str("非 NUMA 机器（无 NUMA 节点）"),
            NumaError::BadTopology(s) => write!(f, "NUMA 拓扑/配置损坏：{s}"),
            NumaError::Io { path, source } => {
                write!(f, "NUMA 绑定 I/O 失败（{}）：{source}", path.display())
            }
            NumaError::NotPrepared { node, reason } => {
                write!(f, "节点 {node} 的 cgroup 组未预置/不可用：{reason}")
            }
        }
    }
}

impl std::error::Error for NumaError {}

impl NumaError {
    fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        NumaError::Io {
            path: path.into(),
            source,
        }
    }
}

/// **纯函数**：解析 Linux cpulist（`"0-3,8"`）→ 升序 CPU 列表。
/// 非法（空段、倒序、非数字、超界）⇒ `None`。上限 8192 个 CPU（防御）。
#[must_use]
pub fn parse_cpulist(s: &str) -> Option<Vec<u32>> {
    const MAX_CPUS: usize = 8192;
    let mut out = Vec::new();
    for part in s.trim().split(',') {
        if part.is_empty() {
            return None;
        }
        let (lo, hi) = match part.split_once('-') {
            Some((a, b)) => (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?),
            None => {
                let v = part.parse::<u32>().ok()?;
                (v, v)
            }
        };
        if hi < lo {
            return None;
        }
        for c in lo..=hi {
            out.push(c);
            if out.len() > MAX_CPUS {
                return None;
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    Some(out)
}

/// **纯函数**：由 `/proc/self/cgroup` 的内容判定 cgroup 版本。
///
/// v2：任一行形如 `0::/path`；v1：任一行含 `cpuset` 控制器
/// （`N:cpuset,...:/path`）。都没有 ⇒ `None`（环境无 cpuset ⇒ 降级）。
#[must_use]
pub fn detect_cgroup_version(content: &str) -> Option<CgroupVersion> {
    let mut v1_cpuset = false;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.splitn(3, ':');
        let _id = it.next()?;
        let controllers = it.next()?;
        let _path = it.next()?;
        if controllers.is_empty() {
            return Some(CgroupVersion::V2);
        }
        if controllers.split(',').any(|c| c == "cpuset") {
            v1_cpuset = true;
        }
    }
    v1_cpuset.then_some(CgroupVersion::V1)
}

/// **纯函数**：由 `/proc/mounts` 的内容找 cpuset 的挂载点。
///
/// v2：`cgroup2` 类型行；v1：`cgroup` 类型且 super options 含 `cpuset` 的行。
#[must_use]
pub fn find_cpuset_mount(mounts: &str, version: CgroupVersion) -> Option<PathBuf> {
    for line in mounts.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        let (mount_point, fstype, options) = (fields[1], fields[2], fields[3]);
        match version {
            CgroupVersion::V2 if fstype == "cgroup2" => {
                return Some(PathBuf::from(mount_point));
            }
            CgroupVersion::V1
                if fstype == "cgroup" && options.split(',').any(|o| o == "cpuset") =>
            {
                return Some(PathBuf::from(mount_point));
            }
            _ => {}
        }
    }
    None
}

/// 一个 NUMA 节点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    /// 节点号。
    pub node: u32,
    /// 该节点的 CPU 列表（升序）。
    pub cpus: Vec<u32>,
}

/// NUMA 拓扑（由 sysfs 探测）。
#[derive(Debug, Clone, Default)]
pub struct Topology {
    nodes: Vec<NodeInfo>,
}

impl Topology {
    /// 探测 `sysfs`（默认 `/sys/devices/system/node`）下的 `nodeN/cpulist`。
    ///
    /// **无 `nodeN` ⇒ [`NumaError::NoTopology`]**（非 NUMA 机器，绑定无意义）。
    pub fn discover(sysfs: &Path) -> Result<Self, NumaError> {
        let entries = match fs::read_dir(sysfs) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(NumaError::NoTopology),
            Err(e) => return Err(NumaError::io(sysfs, e)),
        };
        let mut nodes = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| NumaError::io(sysfs, e))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(n) = name
                .strip_prefix("node")
                .and_then(|d| d.parse::<u32>().ok())
            else {
                continue;
            };
            let cpulist_path = entry.path().join("cpulist");
            let content = match fs::read_to_string(&cpulist_path) {
                Ok(c) => c,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue, // 离线/不完整
                Err(e) => return Err(NumaError::io(&cpulist_path, e)),
            };
            let cpus = parse_cpulist(&content)
                .filter(|c| !c.is_empty())
                .ok_or(NumaError::BadTopology("cpulist 非法"))?;
            nodes.push(NodeInfo { node: n, cpus });
        }
        if nodes.is_empty() {
            return Err(NumaError::NoTopology);
        }
        nodes.sort_by_key(|n| n.node);
        Ok(Self { nodes })
    }

    /// 节点列表。
    #[must_use]
    pub fn nodes(&self) -> &[NodeInfo] {
        &self.nodes
    }

    /// 按节点号取信息。
    #[must_use]
    pub fn node(&self, node: u32) -> Option<&NodeInfo> {
        self.nodes.iter().find(|n| n.node == node)
    }
}

/// `/proc` 探测输入（可注入——测试指向假文件）。
#[derive(Debug, Clone)]
pub struct Probe {
    /// `/proc/self/cgroup`。
    pub proc_self_cgroup: PathBuf,
    /// `/proc/mounts`。
    pub proc_mounts: PathBuf,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            proc_self_cgroup: PathBuf::from("/proc/self/cgroup"),
            proc_mounts: PathBuf::from("/proc/mounts"),
        }
    }
}

/// NUMA 绑定配置（默认**全关**）。
#[derive(Debug, Clone)]
pub struct NumaConfig {
    /// 总开关（默认 false——单节点机器毫无意义）。
    pub enabled: bool,
    /// 绑定模式（默认 `AttachExisting`，权限最小化）。
    pub mode: BindMode,
    /// 我们的 cgroup 树根（`<root>/bicdb-node<N>` 是每节点组）。
    pub cgroup_root: PathBuf,
    /// sysfs 节点根。
    pub sysfs_root: PathBuf,
    /// `/proc` 探测输入。
    pub probe: Probe,
    /// 工作区 → 节点 的显式映射（缺省的工作区 = 不绑定）。
    pub assignments: Vec<(WorkspaceId, u32)>,
}

impl Default for NumaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: BindMode::default(),
            cgroup_root: PathBuf::from("/sys/fs/cgroup/bicdb"),
            sysfs_root: PathBuf::from("/sys/devices/system/node"),
            probe: Probe::default(),
            assignments: Vec::new(),
        }
    }
}

impl NumaConfig {
    /// 校验（详设 §4）：开关打开时必须给出映射；节点不得重复映射；
    /// 同一工作区不得映射到两个节点。
    pub fn validate(&self) -> Result<(), NumaError> {
        if !self.enabled {
            return Ok(());
        }
        if self.assignments.is_empty() {
            return Err(NumaError::BadTopology("已启用但未给出工作区→节点映射"));
        }
        let mut seen: Vec<WorkspaceId> = Vec::new();
        for (ws, _) in &self.assignments {
            if seen.contains(ws) {
                return Err(NumaError::BadTopology("同一工作区映射到多个节点"));
            }
            seen.push(*ws);
        }
        Ok(())
    }
}

/// 一个已就绪的节点组。
#[derive(Debug, Clone)]
pub struct NodeGroup {
    /// 节点号。
    pub node: u32,
    /// 绑定的落点路径（v1 = 组目录；v2 = threaded 叶或组自身）。
    pub bind_dir: PathBuf,
    /// 绑定写入的文件名（v1 = `tasks`；v2 = `cgroup.threads`）。
    pub bind_file: &'static str,
    /// 该组约束的 cpuset.cpus（诊断/核对用）。
    pub cpus: String,
    /// 该组约束的 cpuset.mems。
    pub mems: String,
}

/// 绑定状态（诊断；详设 §9）。
#[derive(Debug, Clone)]
pub enum NumaStatus {
    /// 未启用。
    Disabled,
    /// 启用但降级（原因具名）。
    Degraded {
        /// 原因。
        reason: String,
    },
    /// 生效中。
    Enabled {
        /// 版本。
        version: CgroupVersion,
        /// 已就绪的节点组。
        groups: Vec<NodeGroup>,
        /// 生效的映射。
        assignments: Vec<(WorkspaceId, u32)>,
    },
}

/// 一次绑定的结果（诊断；不返回错误——**绑定失败不得让任务失败**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindOutcome {
    /// 已绑定（或线程级缓存命中，无需重写）。
    Bound,
    /// 该工作区未配置绑定。
    Unassigned,
    /// 绑定失败（已记录计数与最后原因）。
    Failed,
}

thread_local! {
    /// 线程级缓存：本线程当前所在的节点组（去抖——共享池线程连续跑同一
    /// 工作区的任务时不再重复写 cgroup）。
    static BOUND_NODE: std::cell::RefCell<Option<u32>> = const { std::cell::RefCell::new(None) };
}

/// 当前线程的 tid（零 unsafe：`/proc/thread-self` 是 Linux 3.17+ 的
/// 线程自指符号链，readlink 即得 `<pid>/task/<tid>`）。
fn current_tid() -> Result<u32, NumaError> {
    let link =
        fs::read_link("/proc/thread-self").map_err(|e| NumaError::io("/proc/thread-self", e))?;
    link.file_name()
        .and_then(|n| n.to_string_lossy().parse::<u32>().ok())
        .ok_or(NumaError::Unsupported("无法取得当前线程 tid"))
}

/// NUMA 绑定器（探测 + 组准备 + 线程绑定）。
#[derive(Debug)]
pub struct NumaBinder {
    version: CgroupVersion,
    groups: HashMap<u32, NodeGroup>,
    assignments: Vec<(WorkspaceId, u32)>,
    failures: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl NumaBinder {
    /// 探测环境并准备节点组（详设 §3/§4）。任何失败 ⇒ `Err`（调用方记
    /// 降级原因；**不 fail-closed**）。
    pub fn start(config: &NumaConfig) -> Result<Self, NumaError> {
        config.validate()?;
        let topology = Topology::discover(&config.sysfs_root)?;

        let cgroup_content = fs::read_to_string(&config.probe.proc_self_cgroup)
            .map_err(|e| NumaError::io(&config.probe.proc_self_cgroup, e))?;
        let version = detect_cgroup_version(&cgroup_content)
            .ok_or(NumaError::Unsupported("无 cpuset 控制器（v1/v2 都不是）"))?;
        let mounts_content = fs::read_to_string(&config.probe.proc_mounts)
            .map_err(|e| NumaError::io(&config.probe.proc_mounts, e))?;
        let _mount = find_cpuset_mount(&mounts_content, version)
            .ok_or(NumaError::Unsupported("找不到 cpuset/cgroup2 挂载点"))?;

        let mut groups = HashMap::new();
        for (_, node) in &config.assignments {
            if groups.contains_key(node) {
                continue;
            }
            let info = topology
                .node(*node)
                .ok_or(NumaError::BadTopology("映射到了不存在的节点"))?;
            let cpus = format_cpulist(&info.cpus);
            let mems = node.to_string();
            let group = match config.mode {
                BindMode::Provision => {
                    Self::provision_group(&config.cgroup_root, *node, &cpus, &mems, version)?
                }
                BindMode::AttachExisting => {
                    Self::attach_group(&config.cgroup_root, *node, &cpus, &mems, version)?
                }
            };
            groups.insert(*node, group);
        }
        Ok(Self {
            version,
            groups,
            assignments: config.assignments.clone(),
            failures: AtomicU64::new(0),
            last_error: Mutex::new(None),
        })
    }

    /// `Provision`：写控制文件建树（v2：`subtree_control` → 节点组 →
    /// `threads` 叶 → `cgroup.type=threaded` → `cpuset.cpus/mems`）。
    fn provision_group(
        root: &Path,
        node: u32,
        cpus: &str,
        mems: &str,
        version: CgroupVersion,
    ) -> Result<NodeGroup, NumaError> {
        fs::create_dir_all(root).map_err(|e| NumaError::io(root, e))?;
        let node_dir = root.join(format!("bicdb-node{node}"));
        fs::create_dir_all(&node_dir).map_err(|e| NumaError::io(&node_dir, e))?;
        if version == CgroupVersion::V2 {
            // 让子层能用 cpuset（父层的 subtree_control）。
            let root_subtree = root.join("cgroup.subtree_control");
            write_control(&root_subtree, "+cpuset")?;
            let node_subtree = node_dir.join("cgroup.subtree_control");
            write_control(&node_subtree, "+cpuset")?;
            // threaded 叶：v2 的线程级绑定落点。
            let leaf = node_dir.join("threads");
            fs::create_dir_all(&leaf).map_err(|e| NumaError::io(&leaf, e))?;
            let ty = leaf.join("cgroup.type");
            match fs::read_to_string(&ty) {
                Ok(t) if t.trim() == "threaded" => {}
                _ => write_control(&ty, "threaded")?,
            }
            write_control(&node_dir.join("cpuset.cpus"), cpus)?;
            write_control(&node_dir.join("cpuset.mems"), mems)?;
            Ok(NodeGroup {
                node,
                bind_dir: leaf,
                bind_file: "cgroup.threads",
                cpus: cpus.to_owned(),
                mems: mems.to_owned(),
            })
        } else {
            write_control(&node_dir.join("cpuset.cpus"), cpus)?;
            write_control(&node_dir.join("cpuset.mems"), mems)?;
            Ok(NodeGroup {
                node,
                bind_dir: node_dir,
                bind_file: "tasks",
                cpus: cpus.to_owned(),
                mems: mems.to_owned(),
            })
        }
    }

    /// `AttachExisting`：只检查预置树可用（不写任何控制文件）。
    fn attach_group(
        root: &Path,
        node: u32,
        cpus: &str,
        mems: &str,
        version: CgroupVersion,
    ) -> Result<NodeGroup, NumaError> {
        let node_dir = root.join(format!("bicdb-node{node}"));
        if !node_dir.is_dir() {
            return Err(NumaError::NotPrepared {
                node,
                reason: "节点组目录不存在（AttachExisting 模式要求预置）",
            });
        }
        match version {
            CgroupVersion::V1 => {
                if !node_dir.join("tasks").is_file() {
                    return Err(NumaError::NotPrepared {
                        node,
                        reason: "缺 tasks 文件",
                    });
                }
                Ok(NodeGroup {
                    node,
                    bind_dir: node_dir,
                    bind_file: "tasks",
                    cpus: cpus.to_owned(),
                    mems: mems.to_owned(),
                })
            }
            CgroupVersion::V2 => {
                // 优先 threaded 叶；否则组自身为 threaded 亦可用。
                let leaf = node_dir.join("threads");
                if leaf.join("cgroup.threads").is_file() {
                    return Ok(NodeGroup {
                        node,
                        bind_dir: leaf,
                        bind_file: "cgroup.threads",
                        cpus: cpus.to_owned(),
                        mems: mems.to_owned(),
                    });
                }
                if node_dir.join("cgroup.threads").is_file() {
                    let ty = fs::read_to_string(node_dir.join("cgroup.type")).unwrap_or_default();
                    if ty.trim() == "threaded" {
                        return Ok(NodeGroup {
                            node,
                            bind_dir: node_dir,
                            bind_file: "cgroup.threads",
                            cpus: cpus.to_owned(),
                            mems: mems.to_owned(),
                        });
                    }
                }
                Err(NumaError::NotPrepared {
                    node,
                    reason: "v2 需要 threaded 子树（<组>/threads）或组自身为 threaded",
                })
            }
        }
    }

    /// 版本。
    #[must_use]
    pub fn version(&self) -> CgroupVersion {
        self.version
    }

    /// 工作区 → 节点（未配置 ⇒ `None`）。
    #[must_use]
    pub fn node_of(&self, workspace: WorkspaceId) -> Option<u32> {
        self.assignments
            .iter()
            .find(|(w, _)| *w == workspace)
            .map(|(_, n)| *n)
    }

    /// 绑定计数与最后原因（诊断）。
    #[must_use]
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// 最后一次失败原因（诊断）。
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().map(|g| g.clone()).ok().flatten()
    }

    /// 状态快照（诊断）。
    #[must_use]
    pub fn status(&self) -> NumaStatus {
        let mut groups: Vec<NodeGroup> = self.groups.values().cloned().collect();
        groups.sort_by_key(|g| g.node);
        NumaStatus::Enabled {
            version: self.version,
            groups,
            assignments: self.assignments.clone(),
        }
    }

    /// 把 `tid` 绑到节点组（写 `tasks`（v1）或 `cgroup.threads`（v2））。
    pub fn bind_tid(&self, node: u32, tid: u32) -> Result<(), NumaError> {
        let group = self.groups.get(&node).ok_or(NumaError::NotPrepared {
            node,
            reason: "节点组未就绪",
        })?;
        let path = group.bind_dir.join(group.bind_file);
        let mut f = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .map_err(|e| NumaError::io(&path, e))?;
        f.write_all(tid.to_string().as_bytes())
            .map_err(|e| NumaError::io(&path, e))
    }

    /// **绑定点**：把**当前线程**绑到该工作区所属节点（`submit` 路径调用）。
    ///
    /// - 未配置/未就绪 ⇒ 相应 `Skipped` 结果，**不报错**；
    /// - 线程级缓存命中 ⇒ 不重复写；
    /// - 失败 ⇒ 记计数与原因，返回 [`BindOutcome::Failed`]——**调用方照常
    ///   执行任务**（本地性是优化，不是正确性）。
    pub fn bind_current_thread(&self, workspace: WorkspaceId) -> BindOutcome {
        let Some(node) = self.node_of(workspace) else {
            return BindOutcome::Unassigned;
        };
        if BOUND_NODE.with(|c| *c.borrow()) == Some(node) {
            return BindOutcome::Bound; // 缓存命中：本线程已在目标组
        }
        match current_tid().and_then(|tid| self.bind_tid(node, tid)) {
            Ok(()) => {
                BOUND_NODE.with(|c| *c.borrow_mut() = Some(node));
                BindOutcome::Bound
            }
            Err(e) => {
                self.failures.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut slot) = self.last_error.lock() {
                    *slot = Some(e.to_string());
                }
                BindOutcome::Failed
            }
        }
    }
}

/// 把 CPU 列表格式化为 cpulist（`[0,1,2,3,8]` → `"0-3,8"`）。
#[must_use]
pub fn format_cpulist(cpus: &[u32]) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    while i < cpus.len() {
        let start = cpus[i];
        let mut end = start;
        while i + 1 < cpus.len() && cpus[i + 1] == end + 1 {
            i += 1;
            end = cpus[i];
        }
        if !out.is_empty() {
            out.push(',');
        }
        if start == end {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{start}-{end}"));
        }
        i += 1;
    }
    out
}

/// 写一个控制文件（cgroup 文件的常规写法：**不 truncate**，直接 write）。
///
/// `create(true)` 只为测试的假文件系统（cgroupfs 上控制文件本就存在，
/// O_CREAT 对已存在文件是**无操作**）——真实环境不依赖它创建文件。
fn write_control(path: &Path, value: &str) -> Result<(), NumaError> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false) // cgroup 文件按"写一个值"解释；截断无意义
        .open(path)
        .map_err(|e| NumaError::io(path, e))?;
    f.write_all(value.as_bytes())
        .map_err(|e| NumaError::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("bicdb-numa-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ws(v: u64) -> WorkspaceId {
        WorkspaceId::from_raw(v).unwrap()
    }

    /// 假 sysfs：node0（CPU 0-3）、node1（CPU 4-7）。
    fn fake_sysfs(base: &Path) -> PathBuf {
        let sysfs = base.join("sysfs");
        fs::create_dir_all(sysfs.join("node0")).unwrap();
        fs::create_dir_all(sysfs.join("node1")).unwrap();
        fs::write(sysfs.join("node0/cpulist"), "0-3\n").unwrap();
        fs::write(sysfs.join("node1/cpulist"), "4-7\n").unwrap();
        sysfs
    }

    fn fake_probe(base: &Path, cgroup: &str, mounts: &str) -> Probe {
        let p = base.join("proc");
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("self-cgroup"), cgroup).unwrap();
        fs::write(p.join("mounts"), mounts).unwrap();
        Probe {
            proc_self_cgroup: p.join("self-cgroup"),
            proc_mounts: p.join("mounts"),
        }
    }

    #[test]
    fn cpulist_roundtrip_and_rejects() {
        assert_eq!(parse_cpulist("0-3,8"), Some(vec![0, 1, 2, 3, 8]));
        assert_eq!(parse_cpulist("5"), Some(vec![5]));
        assert_eq!(parse_cpulist("3-3"), Some(vec![3]));
        assert_eq!(parse_cpulist(""), None);
        assert_eq!(parse_cpulist("3-1"), None);
        assert_eq!(parse_cpulist("a"), None);
        assert_eq!(parse_cpulist("1,,2"), None);
        assert_eq!(format_cpulist(&[0, 1, 2, 3, 8]), "0-3,8");
        assert_eq!(format_cpulist(&[1]), "1");
        assert_eq!(format_cpulist(&[]), "");
    }

    #[test]
    fn cgroup_version_detection() {
        assert_eq!(detect_cgroup_version("0::/\n"), Some(CgroupVersion::V2));
        assert_eq!(
            detect_cgroup_version("11:cpuset,cpu:/\n"),
            Some(CgroupVersion::V1)
        );
        assert_eq!(detect_cgroup_version("11:cpu:/\n"), None, "无 cpuset");
        assert_eq!(detect_cgroup_version(""), None);
    }

    #[test]
    fn cpuset_mount_detection() {
        let mounts = "proc /proc proc rw 0 0\ncgroup2 /sys/fs/cgroup cgroup2 rw 0 0\n";
        assert_eq!(
            find_cpuset_mount(mounts, CgroupVersion::V2),
            Some(PathBuf::from("/sys/fs/cgroup"))
        );
        let mounts_v1 = "cgroup /sys/fs/cgroup/cpuset cgroup rw,cpuset 0 0\n";
        assert_eq!(
            find_cpuset_mount(mounts_v1, CgroupVersion::V1),
            Some(PathBuf::from("/sys/fs/cgroup/cpuset"))
        );
        assert_eq!(
            find_cpuset_mount("proc /proc proc rw 0 0\n", CgroupVersion::V2),
            None
        );
    }

    #[test]
    fn topology_discovery_and_no_numa() {
        let base = tmp("topo");
        let sysfs = fake_sysfs(&base);
        let t = Topology::discover(&sysfs).unwrap();
        assert_eq!(t.nodes().len(), 2);
        assert_eq!(t.node(1).unwrap().cpus, vec![4, 5, 6, 7]);

        let empty = base.join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(matches!(
            Topology::discover(&empty),
            Err(NumaError::NoTopology)
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn config_validation() {
        let mut cfg = NumaConfig::default();
        assert!(cfg.validate().is_ok(), "默认关闭：不校验");
        cfg.enabled = true;
        assert!(cfg.validate().is_err(), "启用却无映射");
        cfg.assignments = vec![(ws(1), 0)];
        assert!(cfg.validate().is_ok());
        cfg.assignments = vec![(ws(1), 0), (ws(1), 1)];
        assert!(cfg.validate().is_err(), "同一工作区映射两节点");
    }

    #[test]
    fn v2_attach_requires_threaded_subtree() {
        let base = tmp("v2attach");
        let sysfs = fake_sysfs(&base);
        let probe = fake_probe(&base, "0::/\n", "cgroup2 /sys/fs/cgroup cgroup2 rw 0 0\n");
        let root = base.join("cg");

        // ① 未预置 ⇒ NotPrepared。
        let mut cfg = NumaConfig {
            enabled: true,
            mode: BindMode::AttachExisting,
            cgroup_root: root.clone(),
            sysfs_root: sysfs.clone(),
            probe: probe.clone(),
            assignments: vec![(ws(1), 0)],
        };
        assert!(matches!(
            NumaBinder::start(&cfg),
            Err(NumaError::NotPrepared { node: 0, .. })
        ));

        // ② 预置 threaded 叶 ⇒ 就绪，绑定写入 cgroup.threads。
        let leaf = root.join("bicdb-node0/threads");
        fs::create_dir_all(&leaf).unwrap();
        fs::write(leaf.join("cgroup.threads"), "").unwrap();
        let binder = NumaBinder::start(&cfg).unwrap();
        binder.bind_tid(0, 4242).unwrap();
        assert_eq!(
            fs::read_to_string(leaf.join("cgroup.threads")).unwrap(),
            "4242"
        );
        assert_eq!(binder.node_of(ws(1)), Some(0));
        assert_eq!(binder.node_of(ws(2)), None);
        assert_eq!(binder.failures(), 0);

        // ③ Provision 模式建树（假 fs 上写控制文件）。
        cfg.mode = BindMode::Provision;
        cfg.cgroup_root = base.join("cg2");
        let binder = NumaBinder::start(&cfg).unwrap();
        // cgroupfs 里 cgroup.threads 由内核提供；假 fs 上补一个空文件。
        fs::write(base.join("cg2/bicdb-node0/threads/cgroup.threads"), "").unwrap();
        let g = match binder.status() {
            NumaStatus::Enabled { groups, .. } => groups,
            other => panic!("应为 Enabled：{other:?}"),
        };
        assert_eq!(g[0].cpus, "0-3");
        assert_eq!(g[0].mems, "0");
        assert_eq!(
            fs::read_to_string(base.join("cg2/bicdb-node0/threads/cgroup.type")).unwrap(),
            "threaded"
        );
        assert_eq!(
            fs::read_to_string(base.join("cg2/bicdb-node0/cpuset.cpus")).unwrap(),
            "0-3"
        );
        binder.bind_tid(0, 77).unwrap();
        assert_eq!(
            fs::read_to_string(base.join("cg2/bicdb-node0/threads/cgroup.threads")).unwrap(),
            "77"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn v1_attach_uses_tasks_file() {
        let base = tmp("v1");
        let sysfs = fake_sysfs(&base);
        let probe = fake_probe(
            &base,
            "11:cpuset,cpu:/\n",
            "cgroup /sys/fs/cgroup/cpuset cgroup rw,cpuset 0 0\n",
        );
        let root = base.join("cgv1");
        let node_dir = root.join("bicdb-node1");
        fs::create_dir_all(&node_dir).unwrap();
        fs::write(node_dir.join("tasks"), "").unwrap();
        let cfg = NumaConfig {
            enabled: true,
            mode: BindMode::AttachExisting,
            cgroup_root: root.clone(),
            sysfs_root: sysfs,
            probe,
            assignments: vec![(ws(7), 1)],
        };
        let binder = NumaBinder::start(&cfg).unwrap();
        assert_eq!(binder.version(), CgroupVersion::V1);
        binder.bind_tid(1, 99).unwrap();
        assert_eq!(fs::read_to_string(node_dir.join("tasks")).unwrap(), "99");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn degraded_matrix() {
        let base = tmp("degraded");
        let sysfs = fake_sysfs(&base);
        // ① 非 NUMA 机器。
        let no_numa = base.join("nonuma");
        fs::create_dir_all(&no_numa).unwrap();
        let cfg = NumaConfig {
            enabled: true,
            mode: BindMode::Provision,
            cgroup_root: base.join("cg"),
            sysfs_root: no_numa,
            probe: fake_probe(&base, "0::/\n", "cgroup2 /sys/fs/cgroup cgroup2 rw 0 0\n"),
            assignments: vec![(ws(1), 0)],
        };
        assert!(matches!(
            NumaBinder::start(&cfg),
            Err(NumaError::NoTopology)
        ));

        // ② 无 cpuset 控制器。
        let cfg2 = NumaConfig {
            sysfs_root: sysfs.clone(),
            probe: fake_probe(
                &base,
                "11:cpu:/\n",
                "cgroup2 /sys/fs/cgroup cgroup2 rw 0 0\n",
            ),
            ..cfg.clone()
        };
        assert!(matches!(
            NumaBinder::start(&cfg2),
            Err(NumaError::Unsupported(_))
        ));

        // ③ 找不到挂载点。
        let cfg3 = NumaConfig {
            sysfs_root: sysfs.clone(),
            probe: fake_probe(&base, "0::/\n", "proc /proc proc rw 0 0\n"),
            ..cfg.clone()
        };
        assert!(matches!(
            NumaBinder::start(&cfg3),
            Err(NumaError::Unsupported(_))
        ));

        // ④ 映射到不存在的节点。
        let cfg4 = NumaConfig {
            sysfs_root: sysfs,
            probe: fake_probe(&base, "0::/\n", "cgroup2 /sys/fs/cgroup cgroup2 rw 0 0\n"),
            assignments: vec![(ws(1), 9)],
            ..cfg.clone()
        };
        assert!(matches!(
            NumaBinder::start(&cfg4),
            Err(NumaError::BadTopology(_))
        ));

        // ⑤ 绑定失败被记录、不 panic（节点组未就绪）。
        let cfg5 = NumaConfig {
            mode: BindMode::Provision,
            cgroup_root: base.join("cg5"),
            ..cfg4.clone()
        };
        let cfg5 = NumaConfig {
            assignments: vec![(ws(1), 0)],
            ..cfg5
        };
        let binder = NumaBinder::start(&cfg5).unwrap();
        assert!(matches!(
            binder.bind_tid(3, 1),
            Err(NumaError::NotPrepared { node: 3, .. })
        ));
        fs::remove_dir_all(&base).unwrap();
    }
}
