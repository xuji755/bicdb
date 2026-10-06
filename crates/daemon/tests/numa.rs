//! NUMA 第二级绑定集成用例（详设 `doc/numa绑定设计_v0.1.md`）。
//!
//! 在**假 sysfs / 假 proc / 假 cgroup 根**上验证端到端：
//! `Supervisor::submit` 的任务执行线程被写进该工作区节点的
//! `cgroup.threads`（v2 threaded 叶）；降级路径任务照常执行。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bicdb_common::seq::Lsn;
use bicdb_daemon::numa::{BindMode, BindOutcome, NumaStatus};
use bicdb_daemon::{InstanceLimits, NumaConfig, Supervisor, SupervisorConfig};
use bicdb_workspace::registry::{WorkspaceEntry, WorkspaceRegistry};
use bicdb_workspace::{AuthenticatedSubject, Quota, RootName, UserId, WorkspaceId, WorkspaceRoot};

const ALICE: u64 = 1;

fn unique_base(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟应晚于 UNIX_EPOCH")
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "bicdb-numa-it-{tag}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&base).expect("创建测试基目录");
    base
}

fn registry_with(base: &Path, entries: &[(u64, &str)]) -> WorkspaceRegistry {
    let mut registry = WorkspaceRegistry::new();
    for &(id, name) in entries {
        let wsid = WorkspaceId::from_raw(id).unwrap();
        registry
            .register(WorkspaceEntry::new(
                wsid,
                UserId::from_raw(ALICE).unwrap(),
                Some(name.to_owned()),
                WorkspaceRoot::new(base, RootName::for_workspace(wsid)),
                Quota::new(1 << 30, 1 << 28, 1 << 28, 100 << 30),
            ))
            .unwrap();
    }
    registry
}

/// 假 sysfs（node0: CPU 0-3）。
fn fake_sysfs(base: &Path) -> PathBuf {
    let sysfs = base.join("sysfs");
    fs::create_dir_all(sysfs.join("node0")).unwrap();
    fs::write(sysfs.join("node0/cpulist"), "0-3\n").unwrap();
    sysfs
}

/// 假 `/proc`（v2 + cgroup2 挂载）。
fn fake_probe(base: &Path) -> bicdb_daemon::numa::Probe {
    let p = base.join("proc");
    fs::create_dir_all(&p).unwrap();
    fs::write(p.join("self-cgroup"), "0::/\n").unwrap();
    fs::write(p.join("mounts"), "cgroup2 /sys/fs/cgroup cgroup2 rw 0 0\n").unwrap();
    bicdb_daemon::numa::Probe {
        proc_self_cgroup: p.join("self-cgroup"),
        proc_mounts: p.join("mounts"),
    }
}

/// 预置 v2 threaded 叶（cgroupfs 上由内核提供；假 fs 上手工建）。
fn preprovision(base: &Path, node: u32) {
    let leaf = base
        .join("cg")
        .join(format!("bicdb-node{node}"))
        .join("threads");
    fs::create_dir_all(&leaf).unwrap();
    fs::write(leaf.join("cgroup.threads"), "").unwrap();
}

/// 当前线程的 tid（与 numa 模块同法：/proc/thread-self）。
fn current_tid() -> u32 {
    fs::read_link("/proc/thread-self")
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .parse()
        .unwrap()
}

fn small_limits(threads: usize) -> InstanceLimits {
    InstanceLimits {
        execution_threads: threads, // 1 ⇒ 全部任务在同一工作线程上（确定性）
        max_active_workspaces: 4,
        max_task_queue_per_workspace: 8,
        ..InstanceLimits::p0_defaults()
    }
}

#[test]
fn submit_binds_the_executing_thread_to_the_workspace_node() {
    let base = unique_base("bind");
    preprovision(&base, 0);
    let numa = NumaConfig {
        enabled: true,
        mode: BindMode::AttachExisting,
        cgroup_root: base.join("cg"),
        sysfs_root: fake_sysfs(&base),
        probe: fake_probe(&base),
        assignments: vec![(WorkspaceId::from_raw(1).unwrap(), 0)],
    };
    let mut supervisor = Supervisor::boot(
        SupervisorConfig {
            limits: small_limits(1),
            numa,
        },
        registry_with(&base, &[(1, "main")]),
    )
    .expect("启动");
    assert!(
        matches!(supervisor.numa_status(), NumaStatus::Enabled { .. }),
        "应生效：{:?}",
        supervisor.numa_status()
    );

    let alice = AuthenticatedSubject::new(UserId::from_raw(ALICE).unwrap());
    let context = supervisor
        .activate(&alice, Some("main"))
        .expect("激活")
        .clone();

    let (tx, rx) = mpsc::channel();
    supervisor
        .submit(&context, move || {
            tx.send(current_tid()).expect("回传 tid");
        })
        .expect("提交");
    let task_tid = rx.recv().expect("任务执行了");

    // 任务**所在线程**的 tid 必须被写进该工作区节点的 cgroup.threads。
    let content = fs::read_to_string(base.join("cg/bicdb-node0/threads/cgroup.threads"))
        .expect("绑定落点文件");
    assert_eq!(content, task_tid.to_string(), "tid 与任务线程一致");

    supervisor.shutdown();
    fs::remove_dir_all(&base).expect("清理测试目录");
}

#[test]
fn unassigned_workspace_is_not_bound_and_runs_normally() {
    let base = unique_base("unassigned");
    preprovision(&base, 0);
    let numa = NumaConfig {
        enabled: true,
        mode: BindMode::AttachExisting,
        cgroup_root: base.join("cg"),
        sysfs_root: fake_sysfs(&base),
        probe: fake_probe(&base),
        assignments: vec![(WorkspaceId::from_raw(1).unwrap(), 0)],
    };
    let mut supervisor = Supervisor::boot(
        SupervisorConfig {
            limits: small_limits(1),
            numa,
        },
        registry_with(&base, &[(1, "main"), (2, "clone")]),
    )
    .expect("启动");
    let alice = AuthenticatedSubject::new(UserId::from_raw(ALICE).unwrap());

    // 先跑一个已分配工作区的任务（落在 cgroup.threads 上）。
    let ctx1 = supervisor
        .activate(&alice, Some("main"))
        .expect("激活")
        .clone();
    let (tx, rx) = mpsc::channel();
    supervisor
        .submit(&ctx1, move || {
            tx.send(()).unwrap();
        })
        .expect("提交");
    rx.recv().unwrap();
    let bound = fs::read_to_string(base.join("cg/bicdb-node0/threads/cgroup.threads")).unwrap();
    assert!(!bound.is_empty(), "工作区 1 已绑定");

    // 未分配的工作区 2：任务照常执行，绑定文件不变。
    let ctx2 = supervisor
        .activate(&alice, Some("clone"))
        .expect("激活工作区 2")
        .clone();
    let (tx, rx) = mpsc::channel();
    supervisor
        .submit(&ctx2, move || {
            tx.send(current_tid()).unwrap();
        })
        .expect("提交");
    let _tid2 = rx.recv().unwrap();
    let after = fs::read_to_string(base.join("cg/bicdb-node0/threads/cgroup.threads")).unwrap();
    assert_eq!(after, bound, "未分配的工作区不写绑定文件");

    supervisor.shutdown();
    fs::remove_dir_all(&base).expect("清理测试目录");
}

#[test]
fn degraded_binding_never_fails_the_task() {
    let base = unique_base("degraded");
    // **不预置**节点组 ⇒ AttachExisting 失败 ⇒ 降级。
    let numa = NumaConfig {
        enabled: true,
        mode: BindMode::AttachExisting,
        cgroup_root: base.join("cg-missing"),
        sysfs_root: fake_sysfs(&base),
        probe: fake_probe(&base),
        assignments: vec![(WorkspaceId::from_raw(1).unwrap(), 0)],
    };
    let mut supervisor = Supervisor::boot(
        SupervisorConfig {
            limits: small_limits(1),
            numa,
        },
        registry_with(&base, &[(1, "main")]),
    )
    .expect("降级不 fail-closed");
    match supervisor.numa_status() {
        NumaStatus::Degraded { reason } => {
            assert!(
                reason.contains("未预置") || reason.contains("不可用"),
                "{reason}"
            );
        }
        other => panic!("应为 Degraded：{other:?}"),
    }

    let alice = AuthenticatedSubject::new(UserId::from_raw(ALICE).unwrap());
    let context = supervisor
        .activate(&alice, Some("main"))
        .expect("激活")
        .clone();
    let (tx, rx) = mpsc::channel();
    supervisor
        .submit(&context, move || {
            tx.send(42u8).unwrap();
        })
        .expect("降级后任务仍可提交");
    assert_eq!(rx.recv().unwrap(), 42, "任务照常执行");

    supervisor.shutdown();
    fs::remove_dir_all(&base).expect("清理测试目录");
}

#[test]
fn disabled_by_default() {
    let base = unique_base("disabled");
    let supervisor = Supervisor::boot(
        SupervisorConfig {
            limits: small_limits(1),
            ..SupervisorConfig::default()
        },
        registry_with(&base, &[(1, "main")]),
    )
    .expect("启动");
    assert!(matches!(supervisor.numa_status(), NumaStatus::Disabled));
    supervisor.shutdown();
    fs::remove_dir_all(&base).expect("清理测试目录");
}

// -- 重绑定协议（详设 §7）：Draining（真缓冲池）→ Rebinding（cgroup）--------

/// 假 sysfs：node0（CPU 0-3）+ node1（CPU 4-7）。
fn fake_sysfs_two(base: &Path) -> PathBuf {
    let sysfs = base.join("sysfs");
    fs::create_dir_all(sysfs.join("node0")).unwrap();
    fs::create_dir_all(sysfs.join("node1")).unwrap();
    fs::write(sysfs.join("node0/cpulist"), "0-3\n").unwrap();
    fs::write(sysfs.join("node1/cpulist"), "4-7\n").unwrap();
    sysfs
}

/// 无 WAL 的守卫（重绑定用例只关心页落盘次序，不考 redo）。
struct NoWal;

impl bicdb_storage::buffer::WalGuard for NoWal {
    fn durable_lsn(&self) -> Lsn {
        Lsn::from_raw(0).expect("0 合法")
    }
    fn ensure_durable(&self, _target: Lsn) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn rebinding_drains_the_pool_then_moves_the_cgroup_binding() {
    // 端到端：改脏 → Draining（刷尽 + 丢净帧）→ Rebinding（改绑 node1）→
    // 再绑定落新组 → 下一次装入按新绑定重新分配（**不搬内存**）。
    use bicdb_storage::buffer::{BufferKey, BufferPool, CacheConfig, SystemClock};
    use bicdb_storage::page::{Page, PageType};
    use bicdb_storage::pagefile;
    use bicdb_storage::rowid::Rdba;
    use bicdb_workspace::io::{FileIo, MemFileIo, OpenOptions};

    let base = unique_base("rebind");
    preprovision(&base, 0);
    preprovision(&base, 1);
    let ws1 = WorkspaceId::from_raw(1).unwrap();
    let numa = NumaConfig {
        enabled: true,
        mode: BindMode::AttachExisting,
        cgroup_root: base.join("cg"),
        sysfs_root: fake_sysfs_two(&base),
        probe: fake_probe(&base),
        assignments: vec![(ws1, 0)],
    };
    let binder = bicdb_daemon::numa::NumaBinder::start(&numa).expect("绑定器就绪");
    assert_eq!(binder.bind_current_thread(ws1), BindOutcome::Bound);

    // 真缓冲池：工作区 1 的文件（file_id 7）一页，初始内容 0x00。
    let mem = MemFileIo::new();
    mem.add_dir("/mem");
    let f = mem
        .open(
            Path::new("/mem/rb.dat"),
            OpenOptions::new().read(true).write(true).create_new(true),
        )
        .unwrap();
    mem.set_len(f, bicdb_storage::page::PAGE_SIZE as u64)
        .unwrap();
    let ws_bytes = [1u8; 8];
    {
        let mut page = Page::new(PageType::HeapTable, ws_bytes, 7, 0);
        pagefile::write_page(&mem, f, 0, &mut page).unwrap();
    }
    let pool = BufferPool::with_partitions(
        &mem,
        1,
        4,
        move |ws, r| {
            if *ws == ws_bytes && r.file_id() == 7 {
                Some((f, r.block_id()))
            } else {
                None
            }
        },
        NoWal,
        SystemClock,
        CacheConfig::for_capacity(4),
    )
    .unwrap();
    let key = BufferKey::new(ws_bytes, Rdba::from_parts(7, 0).unwrap());
    {
        let mut g = pool.pin(key).unwrap();
        g.as_bytes_mut()[4096] = 0xAB;
        let mut h = g.header().unwrap();
        h.page_lsn = Lsn::from_raw(1).expect("1 合法");
        g.write_header(&h);
        g.mark_dirty(Lsn::from_raw(1).expect("1 合法"));
    }
    let p = pool.partition_of(&ws_bytes);
    assert_eq!(pool.allocated_frames(p), 1);

    // Draining：刷尽 + 丢净帧（帧缓冲释放——重绑定后按新节点重新分配）。
    let report = pool.drain_partition(p).unwrap();
    assert_eq!(report.pages_written, 1, "脏页写回");
    assert_eq!(report.frames_dropped, 1);
    assert_eq!(pool.allocated_frames(p), 0, "净帧的页缓冲已释放");
    let on_disk = pagefile::read_page_verified(&mem, f, 0).unwrap();
    assert_eq!(on_disk.as_bytes()[4096], 0xAB, "新内容已落盘");

    // Rebinding：改绑 node1（预置组；世代号前进 ⇒ 线程缓存失效）。
    assert_eq!(binder.rebind(ws1, 1).unwrap(), 1);
    fs::write(base.join("cg/bicdb-node1/threads/cgroup.threads"), "").unwrap();
    assert_eq!(binder.bind_current_thread(ws1), BindOutcome::Bound);
    assert!(
        !fs::read_to_string(base.join("cg/bicdb-node1/threads/cgroup.threads"))
            .unwrap()
            .is_empty(),
        "重绑定后线程落新节点组"
    );

    // 下一次装入：从盘重建（帧内存重新分配 = 新节点上的首次触碰）。
    let g = pool.pin(key).unwrap();
    assert_eq!(g.as_bytes()[4096], 0xAB, "内容从盘上重建");
    drop(g);
    assert_eq!(pool.allocated_frames(p), 1, "帧缓冲按需重新分配");

    fs::remove_dir_all(&base).expect("清理测试目录");
}

#[test]
fn writer_threads_bind_at_creation_through_the_hook() {
    // 详设 §5 阶段 B：**线程创建时即入组**——`DbwrGroup` 的 prestart 钩子在
    // 每条写线程体内、写回之前执行；创建方（这里 = 监督器的替身）在钩子里
    // 调 `NumaBinder::bind_to_node`。断言：两条写线程的 tid 都落进了该节点组。
    use bicdb_daemon::numa::NumaBinder;
    use bicdb_storage::buffer::{BufferPool, CacheConfig, SystemClock};
    use bicdb_storage::dbwr::DbwrGroup;
    use bicdb_workspace::io::MemFileIo;

    let base = unique_base("writer-bind");
    preprovision(&base, 0);
    let ws1 = WorkspaceId::from_raw(1).unwrap();
    let numa = NumaConfig {
        enabled: true,
        mode: BindMode::AttachExisting,
        cgroup_root: base.join("cg"),
        sysfs_root: fake_sysfs(&base),
        probe: fake_probe(&base),
        assignments: vec![(ws1, 0)],
    };
    let binder = std::sync::Arc::new(NumaBinder::start(&numa).expect("绑定器就绪"));

    // `'static` 池（生产上池随实例存在；测试里泄一个空 I/O）。
    let mem = MemFileIo::new();
    mem.add_dir("/mem");
    let io: &'static MemFileIo = Box::leak(Box::new(mem));
    let pool = std::sync::Arc::new(
        BufferPool::with_partitions(
            io,
            2,
            4,
            |_, _| None,
            NoWal,
            SystemClock,
            CacheConfig::for_capacity(4),
        )
        .unwrap(),
    );

    let tids: std::sync::Arc<std::sync::Mutex<Vec<u32>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let tids2 = std::sync::Arc::clone(&tids);
    let binder2 = std::sync::Arc::clone(&binder);
    let group = DbwrGroup::start(pool, Duration::from_secs(60), move |_p| {
        // 创建时绑定点：**本线程**（刚创建、缓存为空）绑到 node0 组。
        binder2.bind_to_node(0);
        tids2.lock().unwrap().push(current_tid());
    });

    let file = base.join("cg/bicdb-node0/threads/cgroup.threads");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while tids.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let tids = tids.lock().unwrap().clone();
    assert_eq!(tids.len(), 2, "两条写线程各跑一次钩子");
    let content = fs::read_to_string(&file).expect("绑定落点文件");
    let (a, b) = (tids[0].to_string(), tids[1].to_string());
    assert!(
        content == format!("{a}{b}") || content == format!("{b}{a}"),
        "两条写线程的 tid 都写进了节点组：{content} vs {tids:?}"
    );
    assert_eq!(binder.failures(), 0);

    group.shutdown();
    fs::remove_dir_all(&base).expect("清理测试目录");
}

#[test]
fn partition_plan_reconciles_node_and_binds_each_writer_to_its_partition_node() {
    // 详设 §5 阶段 B 的"同节点核对"：写线程创建方在起线程前核对
    // "工作区 → 分区 → 节点"——**同一分区跨节点即具名拒绝**；一致的计划
    // 交给 `dbwr_prestart` ⇒ 每条写线程落进**自己分区**的节点组
    // （区别于上面那条"两条线程都绑 node0"的旧形态用例）。
    use bicdb_daemon::numa::{NumaBinder, NumaError};
    use bicdb_storage::buffer::{BufferPool, CacheConfig, SystemClock};
    use bicdb_storage::dbwr::DbwrGroup;
    use bicdb_workspace::io::MemFileIo;
    use std::sync::{Arc, Mutex};

    let base = unique_base("partition-plan");
    let sysfs = fake_sysfs(&base); // node0（CPU 0-3）
    fs::create_dir_all(sysfs.join("node1")).unwrap();
    fs::write(sysfs.join("node1/cpulist"), "4-7\n").unwrap();
    preprovision(&base, 0);
    preprovision(&base, 1);

    let mem = MemFileIo::new();
    mem.add_dir("/mem");
    let io: &'static MemFileIo = Box::leak(Box::new(mem));
    let pool = Arc::new(
        BufferPool::with_partitions(
            io,
            2,
            4,
            |_, _| None,
            NoWal,
            SystemClock,
            CacheConfig::for_capacity(4),
        )
        .unwrap(),
    );

    // 工作区字节约定本测试内自洽即可（生产上由 workspace_ref 设计给出）。
    let bytes = |id: WorkspaceId| id.as_raw().to_le_bytes();
    let mut ws_in: [Option<WorkspaceId>; 2] = [None, None];
    for raw in 1..1000u64 {
        let id = WorkspaceId::from_raw(raw).unwrap();
        let p = pool.partition_of(&bytes(id));
        if ws_in[p].is_none() {
            ws_in[p] = Some(id);
        }
        if ws_in[0].is_some() && ws_in[1].is_some() {
            break;
        }
    }
    let ws0 = ws_in[0].expect("分区 0 的工作区");
    let ws1 = ws_in[1].expect("分区 1 的工作区");

    let config = |assignments: Vec<(WorkspaceId, u32)>| NumaConfig {
        enabled: true,
        mode: BindMode::AttachExisting,
        cgroup_root: base.join("cg"),
        sysfs_root: sysfs.clone(),
        probe: fake_probe(&base),
        assignments,
    };
    let binder = Arc::new(NumaBinder::start(&config(vec![(ws0, 0), (ws1, 1)])).expect("绑定器"));

    // 核对：分区 0 → node0、分区 1 → node1（各分区内部同节点）。
    let plan = binder
        .plan_partition_nodes(2, |ws| Some(pool.partition_of(&bytes(ws))))
        .expect("分区计划");
    assert_eq!(plan, vec![Some(0), Some(1)]);

    // 起写线程：计划经 prestart 生效；各线程回传（分区, tid）。
    let seen: Arc<Mutex<Vec<(usize, u32)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = Arc::clone(&seen);
    let pre = binder.dbwr_prestart(plan.clone());
    let group = DbwrGroup::start(Arc::clone(&pool), Duration::from_secs(60), move |p| {
        pre(p);
        seen2.lock().unwrap().push((p, current_tid()));
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while seen.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let entries = seen.lock().unwrap().clone();
    assert_eq!(entries.len(), 2, "两条写线程各跑一次钩子");
    for (p, tid) in entries {
        let node = plan[p].expect("涉及的分区有计划");
        let content =
            fs::read_to_string(base.join(format!("cg/bicdb-node{node}/threads/cgroup.threads")))
                .expect("绑定落点文件");
        assert!(
            content.contains(&tid.to_string()),
            "分区 {p} 的写线程 {tid} 应落 node{node}（实际：{content}）"
        );
    }
    assert_eq!(binder.failures(), 0);
    group.shutdown();

    // 核对失败：同一分区里的两个工作区被指派到不同节点 ⇒ 具名拒绝。
    let mut same_part = Vec::new();
    for raw in 1..1000u64 {
        let id = WorkspaceId::from_raw(raw).unwrap();
        if pool.partition_of(&bytes(id)) == pool.partition_of(&bytes(ws0)) {
            same_part.push(id);
            if same_part.len() == 2 {
                break;
            }
        }
    }
    let binder2 = Arc::new(
        NumaBinder::start(&config(vec![(same_part[0], 0), (same_part[1], 1)])).expect("绑定器"),
    );
    let err = binder2
        .plan_partition_nodes(2, |ws| Some(pool.partition_of(&bytes(ws))))
        .expect_err("同分区跨节点必须拒绝");
    assert!(
        matches!(err, NumaError::PartitionSpansNodes { .. }),
        "具名错误：{err}"
    );

    fs::remove_dir_all(&base).expect("清理测试目录");
}
