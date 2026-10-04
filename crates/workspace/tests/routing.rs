//! 身份路由用例（P1 验收：**接入层不能凭请求 user_id 路由**；
//! `ISO` REQ-ISO-002 / REQ-ISO-006）。
//!
//! 场景：两个用户 + `public`。断言的核心不是"能路由"，而是
//! **失败也安全**：他人的工作区与不存在的名字给出同一结果。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bicdb_workspace::identity::AuthenticatedSubject;
use bicdb_workspace::registry::{
    Routed, RoutingError, WorkspaceEntry, WorkspaceRegistry, PUBLIC_WORKSPACE,
};
use bicdb_workspace::{Quota, RootName, UserId, WorkspaceId, WorkspaceRoot};

const ALICE: u64 = 1;
const BOB: u64 = 2;

fn unique_base(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟应晚于 UNIX_EPOCH")
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "bicdb-route-test-{tag}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&base).expect("创建测试基目录");
    base
}

fn owned(id: u64, owner: u64, name: Option<&str>, base: &Path) -> WorkspaceEntry {
    let wsid = WorkspaceId::from_raw(id).unwrap();
    let user = UserId::from_raw(owner).unwrap();
    WorkspaceEntry::new(
        wsid,
        user,
        name.map(str::to_owned),
        WorkspaceRoot::new(base, RootName::for_workspace(wsid)),
        Quota::new(1 << 30, 1 << 28, 1 << 28, 100 << 30),
    )
}

#[test]
fn routing_is_a_function_of_identity_not_of_request_data() {
    let base = unique_base("routes");
    let mut reg = WorkspaceRegistry::new();
    reg.register_public(WorkspaceRoot::new(&base, RootName::public()))
        .unwrap();
    reg.register(owned(1, ALICE, Some("main"), &base)).unwrap();
    reg.register(owned(2, ALICE, Some("clone"), &base)).unwrap(); // 一个主体多个工作区（REQ-ISO-012）
    reg.register(owned(3, BOB, Some("main"), &base)).unwrap(); // 跨属主同名：合法
    reg.register(owned(4, BOB, None, &base)).unwrap(); // 未命名槽位

    let alice = AuthenticatedSubject::new(UserId::from_raw(ALICE).unwrap());
    let bob = AuthenticatedSubject::new(UserId::from_raw(BOB).unwrap());

    // 同名 "main" 路由到各自不同的根。
    let a_main = reg.route(&alice, Some("main")).unwrap();
    let b_main = reg.route(&bob, Some("main")).unwrap();
    assert_ne!(a_main.root().path(), b_main.root().path(), "同名不同根");
    assert!(
        a_main.root().path().ends_with("w-000000000001"),
        "alice.main 的根：{:?}",
        a_main.root().path()
    );
    assert!(b_main.root().path().ends_with("w-000000000003"));
    // 属主身份与工作区身份都随路由结果给出。
    match a_main {
        Routed::Owned(entry) => {
            assert_eq!(entry.id(), WorkspaceId::from_raw(1).unwrap());
            assert_eq!(entry.owner(), UserId::from_raw(ALICE).unwrap());
            assert_eq!(entry.name(), Some("main"));
        }
        Routed::Public(_) => panic!("main 不是 public"),
    }

    // 多个工作区都在本人范围内可达；未命名槽位只对属主可见。
    assert!(reg.route(&alice, Some("clone")).is_ok());
    assert!(reg.route(&bob, None).is_ok());
    assert_eq!(reg.route(&alice, None).err(), Some(RoutingError::NotFound));

    // public 对两个主体都可达，且是同一个根。
    let a_public = reg.route(&alice, Some(PUBLIC_WORKSPACE)).unwrap();
    let b_public = reg.route(&bob, Some(PUBLIC_WORKSPACE)).unwrap();
    assert_eq!(a_public.root().path(), b_public.root().path());
    assert!(matches!(a_public, Routed::Public(_)));

    // ---- 失败面：一切"不属于自己"都报不存在，且不可区分 ----
    let foreign = reg.route(&alice, Some("bobs-only"));
    let missing = reg.route(&alice, Some("never-existed"));
    assert_eq!(
        foreign.as_ref().err(),
        missing.as_ref().err(),
        "两种失败必须不可区分"
    );
    assert_eq!(foreign.err(), Some(RoutingError::NotFound));
    // 用 bob 的身份去要 alice 的工作区：同样不存在。
    assert_eq!(
        reg.route(&bob, Some("clone")).err(),
        Some(RoutingError::NotFound)
    );

    // public 未登记时不泄露"实例上有没有 public"。
    let empty = WorkspaceRegistry::new();
    assert_eq!(
        empty.route(&alice, Some(PUBLIC_WORKSPACE)).err(),
        Some(RoutingError::NotFound)
    );

    fs::remove_dir_all(&base).expect("清理测试目录");
}
