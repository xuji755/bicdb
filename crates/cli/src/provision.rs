//! **工作区文件面的供给方**（`bicdb-sql` 的 `WorkspaceProvisioner` 端口的 CLI 真件）。
//!
//! `CREATE WORKSPACE` / `DROP WORKSPACE` 的**第①/③步**（建/删文件面）落在这里——
//! 它要参数文件、日志布局与实例装配（`boot`），这些是 CLI 层的事；
//! DCL 的语义与"两处写"的顺序在 `bicdb-sql::dcl_exec` 里。
//!
//! # 建的文件面（与 `bicdb init` 同一套）
//!
//! ```text
//! <root>/bicdb.ini        参数文件（**种子 = 实例的 public/bicdb.ini**；日志指到 <home>/log/）
//! <root>/control/          控制文件双副本
//! <root>/wal/              日志组
//! <root>/data/<ref>_meta    file 0（字典；**普通工作区**：不带 user$/ws$/fs$/wq$）
//! <root>/data/<ref>_undo    file 1（撤销段）
//! ```
//!
//! **不给 `stat$`/`seq$` 留尾巴**：`create_instance` 的收尾会建它们（建区期的
//! DDL 事务），建完即 `shutdown`（完全检查点）——之后这个目录就能独立 `open`。

use std::path::Path;

use bicdb_sql::dcl_exec::{ProvisionRequest, WorkspaceProvisioner};
use bicdb_workspace::WorkspaceId;

use crate::boot::{self, CreateOptions};
use crate::config::InstanceParams;
use crate::home::Home;

/// CLI 侧真件。
///
/// 无状态：实例根从环境（`BICDB_HOME`）取，工作区号/名字/日志落点都由
/// [`ProvisionRequest`] 给——**登记不由这里做**（DCL 执行层按三步协议最后写）。
#[derive(Debug, Default, Clone, Copy)]
pub struct CliProvisioner;

impl CliProvisioner {
    /// 新建。
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// **进程级一份**（无状态；`Box::leak` 成 `'static`——与实例同生命周期）。
    ///
    /// 会话要借一个 `&'a dyn WorkspaceProvisioner`，而 `'a` 是会话的借用参数，
    /// 由 `attach_dcl` 的 `'static` 满足（与 I/O 的做法同源）。
    #[must_use]
    pub fn new_static() -> &'static Self {
        static P: std::sync::OnceLock<&'static CliProvisioner> = std::sync::OnceLock::new();
        P.get_or_init(|| Box::leak(Box::new(CliProvisioner)))
    }
}

impl WorkspaceProvisioner for CliProvisioner {
    fn provision(&self, req: &ProvisionRequest) -> Result<(), String> {
        let ws_id = WorkspaceId::from_raw(req.workspace_id)
            .ok_or_else(|| format!("工作区号 {} 越界（48 位、0 保留）", req.workspace_id))?;
        // 参数：**以实例的参数文件为种子**（同实例同口径），日志统一落 `<home>/log/`。
        let overrides = vec![("service.log".to_owned(), req.log_path.display().to_string())];
        let params = InstanceParams::for_init(&req.root, Some(&req.seed_ini), &overrides)
            .map_err(|e| e.to_string())?;
        // 文件面：建 → 建区收尾（stat$/seq$）→ **完全检查点后关掉**。
        // **不登记**（`register = false`）：注册表由 DCL 在 `ws$` 之后写。
        let home = Home::locate().ok();
        let mut inst = boot::create_instance_with(
            &params,
            home.as_ref(),
            &CreateOptions {
                workspace_id: Some(ws_id),
                register: false,
            },
        )
        .map_err(|e| e.to_string())?;
        inst.shutdown().map_err(|e| e.to_string())?;
        Ok(())
    }

    fn check_deprovisionable(&self, root: &Path) -> Result<(), String> {
        // **活实例占着就不删**（`bicdb.pid` 活着 ⇒ 有人正开它。
        // 单写者纪律：删别人正在写的库是撕字典）。
        if let Some(info) = crate::lock::read_lock(root) {
            if crate::lock::is_live(&info) {
                return Err(format!(
                    "{} 被 pid {}（{}）占着——先 `bicdb stop -p {}` 再删",
                    root.display(),
                    info.pid,
                    match info.mode {
                        crate::lock::LockMode::Service => "服务模式",
                        crate::lock::LockMode::Direct => "直连模式",
                    },
                    root.display()
                ));
            }
        }
        Ok(())
    }

    fn deprovision(&self, root: &Path) -> Result<(), String> {
        if !root.exists() {
            return Ok(()); // 幂等：已经不在了
        }
        std::fs::remove_dir_all(root).map_err(|e| format!("删 {} 失败：{e}", root.display()))
    }
}
