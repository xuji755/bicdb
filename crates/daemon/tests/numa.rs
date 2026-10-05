//! NUMA 第二级绑定集成用例（详设 `doc/numa绑定设计_v0.1.md`）。
//!
//! 在**假 sysfs / 假 proc / 假 cgroup 根**上验证端到端：
//! `Supervisor::submit` 的任务执行线程被写进该工作区节点的
//! `cgroup.threads`（v2 threaded 叶）；降级路径任务照常执行。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use bicdb_daemon::numa::{BindMode, NumaStatus};
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
