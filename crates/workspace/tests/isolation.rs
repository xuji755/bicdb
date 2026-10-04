//! 用户隔离用例（P1 验收：**两个用户及 `public` 的独立根目录与进程文件权限**）。
//!
//! 对应根 `tests/README.md` 的 `tests/isolation/`（P1 起）——当前以本 crate 的
//! 集成测试形态落地；测试工作者（fixture）只经公开 API 操作，验证：
//! 根目录互不重叠、权限为 `0700`、符号链接被拒绝、创建幂等。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use bicdb_workspace::{RootName, WorkspaceDir, WorkspaceId, WorkspaceRoot};

/// 独立临时目录（不引入第三方依赖）。
fn unique_test_base(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟应晚于 UNIX_EPOCH")
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "bicdb-ws-test-{tag}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&base).expect("创建测试基目录");
    base
}

fn mode_of(path: &std::path::Path) -> u32 {
    fs::symlink_metadata(path)
        .expect("路径应可访问")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn two_users_and_public_have_disjoint_private_roots() {
    let base = unique_test_base("roots");

    let alice = WorkspaceRoot::new(
        &base,
        RootName::for_workspace(WorkspaceId::from_raw(1).unwrap()),
    );
    let bob = WorkspaceRoot::new(
        &base,
        RootName::for_workspace(WorkspaceId::from_raw(2).unwrap()),
    );
    let public = WorkspaceRoot::new(&base, RootName::public());

    for root in [&alice, &bob, &public] {
        root.create_layout().expect("工作区目录应创建成功");
        assert_eq!(mode_of(root.path()), 0o700, "根目录权限应为 0700");
    }

    // 三个根互不重合，也不互为前缀。
    assert_ne!(alice.path(), bob.path());
    assert_ne!(alice.path(), public.path());
    assert!(!bob.path().starts_with(alice.path()));
    assert!(!alice.path().starts_with(bob.path()));

    // 每个根下恰好是十个固定子目录，权限同为 0700。
    for root in [&alice, &bob, &public] {
        for dir in WorkspaceDir::ALL {
            let p = dir.path_in(root.path());
            assert!(p.is_dir(), "{} 应为目录", p.display());
            assert_eq!(mode_of(&p), 0o700, "{} 权限应为 0700", p.display());
        }
        let count = fs::read_dir(root.path())
            .expect("根目录可枚举")
            .filter(|e| e.as_ref().map(|e| e.path().is_dir()).unwrap_or(false))
            .count();
        assert_eq!(count, WorkspaceDir::ALL.len());
    }

    fs::remove_dir_all(&base).expect("清理测试目录");
}

#[test]
fn symlinked_root_is_refused() {
    let base = unique_test_base("symlink");

    // 先造一个真实目录，再用符号链接冒充工作区根。
    let target = base.join("elsewhere");
    fs::create_dir_all(&target).unwrap();
    let name = RootName::for_workspace(WorkspaceId::from_raw(9).unwrap());
    let link = base.join(name.as_str());
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let root = WorkspaceRoot::new(&base, name);
    let err = root
        .create_layout()
        .expect_err("符号链接冒充的根目录必须拒绝");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    fs::remove_dir_all(&base).expect("清理测试目录");
}

#[test]
fn symlinked_subdir_is_refused_and_creation_is_idempotent() {
    let base = unique_test_base("subdir");
    let root = WorkspaceRoot::new(&base, RootName::public());
    root.create_layout().expect("首次创建");

    // 幂等：再次调用成功。
    root.create_layout().expect("重复创建应幂等");

    // 把某个子目录替换成符号链接后，必须拒绝。
    let catalog = WorkspaceDir::Catalog.path_in(root.path());
    fs::remove_dir_all(&catalog).unwrap();
    std::os::unix::fs::symlink(&base, &catalog).unwrap();
    let err = root.create_layout().expect_err("符号链接子目录必须拒绝");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    fs::remove_dir_all(&base).expect("清理测试目录");
}

#[test]
fn regular_file_in_place_of_subdir_is_refused() {
    let base = unique_test_base("file");
    let root = WorkspaceRoot::new(
        &base,
        RootName::for_workspace(WorkspaceId::from_raw(5).unwrap()),
    );
    let root_path = root.path().to_path_buf();
    fs::create_dir_all(&root_path).unwrap();
    fs::write(WorkspaceDir::Data.path_in(&root_path), b"not a dir").unwrap();

    let err = root.create_layout().expect_err("普通文件占位必须拒绝");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    fs::remove_dir_all(&base).expect("清理测试目录");
}
