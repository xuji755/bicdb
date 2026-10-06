//! 监督器用例（P1 验收对齐：**启动恢复**、**资源拒绝**；`NFR` REQ-RES-001/003/004）。
//!
//! 场景取自 §17："以两个用户及 public 验证独立根目录、进程文件权限、
//! 启动恢复与资源拒绝"。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bicdb_daemon::{
    ActivateError, BootError, InstanceLimits, Supervisor, SupervisorConfig, TaskRejected,
};
use bicdb_workspace::registry::{WorkspaceEntry, WorkspaceRegistry};
use bicdb_workspace::{AuthenticatedSubject, Quota, RootName, UserId, WorkspaceId, WorkspaceRoot};

const ALICE: u64 = 1;
const BOB: u64 = 2;

fn unique_base(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟应晚于 UNIX_EPOCH")
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "bicdb-sup-test-{tag}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&base).expect("创建测试基目录");
    base
}

fn owned(id: u64, owner: u64, name: Option<&str>, base: &Path) -> WorkspaceEntry {
    let wsid = WorkspaceId::from_raw(id).unwrap();
    WorkspaceEntry::new(
        wsid,
        UserId::from_raw(owner).unwrap(),
        name.map(str::to_owned),
        WorkspaceRoot::new(base, RootName::for_workspace(wsid)),
        Quota::new(1 << 30, 1 << 28, 1 << 28, 100 << 30),
    )
}

fn registry_with_alice_bob_public(base: &Path) -> WorkspaceRegistry {
    let mut registry = WorkspaceRegistry::new();
    registry
        .register_public(WorkspaceRoot::new(base, RootName::public()))
        .unwrap();
    registry
        .register(owned(1, ALICE, Some("main"), base))
        .unwrap();
    registry
        .register(owned(2, ALICE, Some("clone"), base))
        .unwrap();
    registry
        .register(owned(3, BOB, Some("main"), base))
        .unwrap();
    registry
}

fn small_limits(max_active: usize, threads: usize, queue: usize) -> InstanceLimits {
    InstanceLimits {
        execution_threads: threads,
        max_active_workspaces: max_active,
        max_task_queue_per_workspace: queue,
        ..InstanceLimits::p0_defaults()
    }
}

#[test]
fn boot_verifies_layouts_and_is_idempotent() {
    let base = unique_base("boot");
    let registry = registry_with_alice_bob_public(&base);
    let supervisor = Supervisor::boot(SupervisorConfig::default(), registry).expect("首次启动");
    assert_eq!(supervisor.active_count(), 0);

    // 三个根与布局都已就位、权限 0700。
    for name in [
        "w-000000000001",
        "w-000000000002",
        "w-000000000003",
        "public",
    ] {
        let root = base.join(name);
        assert!(root.is_dir(), "{name} 根存在");
        assert!(root.join("data").is_dir(), "{name}/data 存在");
    }
    let mode = fs::symlink_metadata(base.join("public"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700, "进程文件权限");
    supervisor.shutdown();

    // 再次启动（第二次自检）：幂等。
    let registry = registry_with_alice_bob_public(&base);
    let supervisor = Supervisor::boot(SupervisorConfig::default(), registry).expect("重复启动");
    supervisor.shutdown();

    fs::remove_dir_all(&base).expect("清理测试目录");
}

#[test]
fn boot_fails_closed_on_tampered_root() {
    let base = unique_base("tamper");
    let registry = registry_with_alice_bob_public(&base);
    let supervisor = Supervisor::boot(SupervisorConfig::default(), registry).expect("首次启动");
    supervisor.shutdown();

    // 把某工作区的 data 子目录换成指向工作区外的符号链接（外部篡改）。
    let data = base.join("w-000000000001").join("data");
    fs::remove_dir_all(&data).unwrap();
    std::os::unix::fs::symlink(&base, &data).unwrap();

    let registry = registry_with_alice_bob_public(&base);
    let err = Supervisor::boot(SupervisorConfig::default(), registry)
        .expect_err("篡改后必须 fail closed");
    match err {
        BootError::WorkspaceNotOpenable { workspace, .. } => {
            assert_eq!(workspace, WorkspaceId::from_raw(1).unwrap());
        }
        other => panic!("应为工作区自检失败：{other:?}"),
    }

    fs::remove_dir_all(&base).expect("清理测试目录");
}

#[test]
fn activation_is_routed_by_identity_and_bounded_by_admission() {
    let base = unique_base("activate");
    let registry = registry_with_alice_bob_public(&base);
    let limits = small_limits(2, 2, 8); // 只容纳 2 个活跃工作区
    let mut supervisor = Supervisor::boot(
        SupervisorConfig {
            limits,
            ..SupervisorConfig::default()
        },
        registry,
    )
    .expect("启动");

    let alice = AuthenticatedSubject::new(UserId::from_raw(ALICE).unwrap());
    let bob = AuthenticatedSubject::new(UserId::from_raw(BOB).unwrap());

    let a_main = supervisor.activate(&alice, Some("main")).unwrap();
    assert_eq!(a_main.workspace(), WorkspaceId::from_raw(1).unwrap());
    assert!(a_main.root().path().ends_with("w-000000000001"));
    // 幂等：重复激活同一工作区不占新额度。
    supervisor.activate(&alice, Some("main")).unwrap();
    assert_eq!(supervisor.active_count(), 1);

    let a_clone = supervisor.activate(&alice, Some("clone")).unwrap();
    assert_eq!(a_clone.workspace(), WorkspaceId::from_raw(2).unwrap());
    assert_eq!(supervisor.active_count(), 2);

    // 触顶：bob 被资源拒绝（可判定：哪个额度、当前值、上限）。
    assert_eq!(
        supervisor.activate(&bob, Some("main")).unwrap_err(),
        ActivateError::ResourceLimit {
            quota: "max_active_workspaces",
            current: 2,
            limit: 2,
        }
    );

    // 空闲进程可退出：释放一个后 bob 可进入。
    assert!(supervisor.deactivate(WorkspaceId::from_raw(2).unwrap()));
    supervisor.activate(&bob, Some("main")).unwrap();
    assert_eq!(supervisor.active_count(), 2);

    // 失败面：他人的工作区与不存在的名字不可区分；public 不占额度。
    assert_eq!(
        supervisor.activate(&bob, Some("clone")).unwrap_err(),
        ActivateError::NotFound
    );
    assert_eq!(
        supervisor
            .activate(&bob, Some("never-existed"))
            .unwrap_err(),
        ActivateError::NotFound
    );
    assert_eq!(
        supervisor.activate(&bob, Some("public")).unwrap_err(),
        ActivateError::PublicNotActivated
    );

    supervisor.shutdown();
    fs::remove_dir_all(&base).expect("清理测试目录");
}

#[test]
fn workspace_queue_limit_rejects_without_stealing_others_quota() {
    let base = unique_base("queue");
    let registry = registry_with_alice_bob_public(&base);
    // 单线程 + 每工作区排队额度 2；活跃工作区额度放到 4（本用例测的是队列，不是准入）。
    let limits = small_limits(4, 1, 2);
    let mut supervisor = Supervisor::boot(
        SupervisorConfig {
            limits,
            ..SupervisorConfig::default()
        },
        registry,
    )
    .expect("启动");

    let alice = AuthenticatedSubject::new(UserId::from_raw(ALICE).unwrap());
    let bob = AuthenticatedSubject::new(UserId::from_raw(BOB).unwrap());
    let alice_main = supervisor.activate(&alice, Some("main")).unwrap().clone();
    let alice_clone = supervisor.activate(&alice, Some("clone")).unwrap().clone();
    let bob_main = supervisor.activate(&bob, Some("main")).unwrap().clone();
    let alice_main_ws = alice_main.workspace();

    // 占住唯一执行线程。
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
    let holder = Arc::clone(&release_rx);
    supervisor
        .submit(&alice_main, move || {
            let _ = holder.lock().unwrap_or_else(|e| e.into_inner()).recv();
        })
        .unwrap();

    // alice/main 的额度是 2（**在途 + 排队合计**）：阻塞任务已占 1，再排 1 个即满。
    supervisor.submit(&alice_main, || {}).unwrap();
    // 第 3 个超限 → 资源拒绝，且指明额度与上限。
    assert_eq!(
        supervisor.submit(&alice_main, || {}).unwrap_err(),
        TaskRejected::WorkspaceQueueFull {
            workspace: alice_main_ws,
            limit: 2,
        }
    );

    // 其他工作区不受影响（额度按工作区，各自有界）。
    supervisor.submit(&alice_clone, || {}).unwrap();
    supervisor.submit(&bob_main, || {}).unwrap();
    assert_eq!(
        supervisor.in_flight(alice_main_ws),
        Some(2),
        "1 在跑 + 1 排队"
    );

    // 上下文与监督器实例绑定：另一实例（未激活该工作区）拒绝该上下文的提交。
    let registry_only = registry_with_alice_bob_public(&base);
    let other = Supervisor::boot(SupervisorConfig::default(), registry_only).unwrap();
    assert_eq!(
        other.submit(&alice_main, || {}).unwrap_err(),
        TaskRejected::NotActive(alice_main_ws),
        "上下文由监督器生成、随实例生效（不串区）"
    );
    other.shutdown();

    // 放行并等待排空：额度正确归还。
    release_tx.send(()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while supervisor.in_flight(alice_main_ws) != Some(0) {
        assert!(std::time::Instant::now() < deadline, "任务应在限时内排空");
        std::thread::sleep(Duration::from_millis(5));
    }
    supervisor.submit(&alice_main, || {}).unwrap();

    supervisor.shutdown();
    fs::remove_dir_all(&base).expect("清理测试目录");
}

#[test]
fn maintenance_is_not_starved_by_the_execution_pool() {
    let base = unique_base("maint");
    let registry = registry_with_alice_bob_public(&base);
    let limits = small_limits(2, 1, 4); // 唯一执行线程
    let mut supervisor = Supervisor::boot(
        SupervisorConfig {
            limits,
            ..SupervisorConfig::default()
        },
        registry,
    )
    .expect("启动");

    let alice = AuthenticatedSubject::new(UserId::from_raw(ALICE).unwrap());
    let ctx = supervisor.activate(&alice, Some("main")).unwrap().clone();

    // 打满执行池：一个阻塞任务 + 占满队列。
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
    for _ in 0..4 {
        let rx = Arc::clone(&release_rx);
        let _ = supervisor.submit(&ctx, move || {
            let _ = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
        });
    }
    // 池已满：普通请求被拒绝（有界），但**维护任务照常执行**。
    assert!(matches!(
        supervisor.submit(&ctx, || {}),
        Err(TaskRejected::WorkspaceQueueFull { .. } | TaskRejected::InstanceQueueFull { .. })
    ));

    let ran = Arc::new(AtomicUsize::new(0));
    let flag = Arc::clone(&ran);
    let before = supervisor.maintenance().completed();
    supervisor
        .maintenance()
        .schedule(move || {
            flag.fetch_add(1, Ordering::SeqCst);
        })
        .expect("维护队列可用");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while ran.load(Ordering::SeqCst) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "维护任务必须在执行池打满时仍能完成（REQ-RES-003）"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(supervisor.maintenance().completed(), before + 1);

    // 放行执行池中的全部阻塞任务（每个任务各需一次释放；优雅关闭会等待在途任务）。
    for _ in 0..4 {
        release_tx.send(()).unwrap();
    }
    supervisor.shutdown();
    fs::remove_dir_all(&base).expect("清理测试目录");
}
