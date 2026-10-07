//! **实例注册表**（全局控制文件）与 `bicdb init` 的接线（`doc/全局控制文件设计_v0.1.md`）。
//!
//! 钉住三件事：
//! ① 在 home 下建工作区 ⇒ **登记**（`<home>/control/` 双副本，记录含工作区号与根目录）；
//! ② 工作区号**单调分配**（public = 1、第二个 = 2）；
//! ③ **路径式**（不在 home 下、或没有 home）⇒ 不登记，且**不建** `<home>/control/`。

use std::path::Path;

use bicdb_cli::boot::{create_instance, open_global_ctl};
use bicdb_cli::config::InstanceParams;
use bicdb_cli::home::{Home, HomeSource};

/// 简易临时目录（e2e.rs 同款：不引第三方依赖）。
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bicdb-registry-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn params_for(dir: &Path) -> InstanceParams {
    InstanceParams::for_init(dir, None, &[]).expect("参数")
}

fn home_of(root: &Path) -> Home {
    Home {
        root: root.to_path_buf(),
        source: HomeSource::Env,
    }
}

#[test]
fn init_under_home_registers_the_workspace() {
    let home_dir = TempDir::new("home");
    let home = home_of(home_dir.path());

    // ① public：编号 1，登记后 `list` 能看见。
    let public = home.public_dir();
    let mut inst = create_instance(&params_for(&public), Some(&home)).expect("建 public");
    inst.shutdown().expect("关");

    // **PUBLIC = 管理面**：它带 `user$`/`ws$`/`fs$` 三张自举表（引导条目 23）；
    // 普通工作区没有（15）。
    assert!(inst.catalog.is_public(), "`<home>/public` 必须建成管理面");
    assert_eq!(
        inst.ws_ref,
        bicdb_workspace::workspace_ref(bicdb_workspace::WorkspaceId::from_raw(1).unwrap()),
        "文件标识 = SHA-256(工作区号) 前 8 字节"
    );
    assert!(
        inst.dir
            .join("data")
            .join("7c9fa136d4413fa6_meta")
            .is_file(),
        "数据文件按 workspace_ref 命名"
    );

    let [a, b] = home.global_ctl_paths();
    assert!(a.is_file(), "全局控制文件 A 副本：{}", a.display());
    assert!(b.is_file(), "全局控制文件 B 副本：{}", b.display());

    let gcf = open_global_ctl(&home).expect("打开注册表");
    let listed = gcf.workspaces().expect("在册清单");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].workspace_id.as_raw(), 1);
    let canon = std::fs::canonicalize(&public).expect("规范路径");
    assert_eq!(
        listed[0].root.as_slice(),
        canon.display().to_string().as_bytes()
    );
    gcf.close().expect("关");

    // ② 第二个工作区：编号 2（单调），两条都在册。
    let shop = home.root.join("shop");
    let mut inst = create_instance(&params_for(&shop), Some(&home)).expect("建 shop");
    assert!(!inst.catalog.is_public(), "普通工作区不是管理面");
    inst.shutdown().expect("关");
    let gcf = open_global_ctl(&home).expect("打开注册表");
    let listed = gcf.workspaces().expect("在册清单");
    assert_eq!(listed.len(), 2);
    assert_eq!(
        listed
            .iter()
            .map(|r| r.workspace_id.as_raw())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    gcf.close().expect("关");
}

#[test]
fn path_form_outside_any_home_is_not_registered() {
    let home_dir = TempDir::new("home2");
    let home = home_of(home_dir.path());

    // 没有 home（路径式用法）⇒ 不登记、也不建 <home>/control/。
    let elsewhere = TempDir::new("elsewhere");
    let mut inst = create_instance(&params_for(elsewhere.path()), None).expect("建区");
    inst.shutdown().expect("关");
    assert!(!home.control_dir().exists(), "不该在别人的 home 下建注册表");

    // 有 home，但工作区在 home 之外 ⇒ 同样不登记（记档：安装布局 §5）。
    let outside = TempDir::new("outside");
    let mut inst = create_instance(&params_for(outside.path()), Some(&home)).expect("建区");
    inst.shutdown().expect("关");
    assert!(!home.control_dir().exists());
}
