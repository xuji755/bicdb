//! Offline verification for a persisted recovery-isolation scope.

use std::path::{Path, PathBuf};

use bicdb_common::seq::CommitSeq;
use bicdb_storage::controlfile::ControlFile;
use bicdb_storage::datafile::DataFile;
use bicdb_storage::recovery_journal::{
    read_records, RecoveryJournal, RecoveryScope, RecoveryState,
};
use bicdb_workspace::io::OsFileIo;

use crate::boot::{CF_A, CF_B};
use crate::config::InstanceParams;
use crate::lock::{InstanceLock, LockMode};

/// Exact scope accepted by the offline administrator command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyScope {
    /// Physical page.
    Page { file_id: u16, block_id: u32 },
    /// Catalog object and its physical segment.
    Object { object_id: u32 },
}

/// Verify durable media and append the exact `Verified` record. The instance
/// lock guarantees that neither daemon nor direct SQL can race this operation.
pub fn verify(params: &InstanceParams, scope: VerifyScope) -> Result<String, String> {
    let io = OsFileIo::new();
    let root = &params.db_root;
    let _lock = InstanceLock::acquire(root, LockMode::Direct, &params.socket_path())
        .map_err(|error| format!("离线验证要求实例已停止：{error}"))?;
    let meta_path = crate::boot::find_meta_file(root).ok_or("工作区缺少元数据文件")?;
    let mut catalog = bicdb_catalog::Catalog::open(&io, &meta_path)
        .map_err(|error| format!("打开目录失败：{error}"))?;
    let workspace = catalog.workspace();
    let audit_path = root.join("recovery.audit");
    let records = match read_records(&io, &audit_path, workspace) {
        Ok(records) => records,
        Err(error) => {
            let _ = catalog.close();
            return Err(format!("读取恢复审计失败：{error}"));
        }
    };
    let audit_scope = match scope {
        VerifyScope::Page { file_id, block_id } => RecoveryScope::Page { file_id, block_id },
        VerifyScope::Object { object_id } => RecoveryScope::Object {
            object_id: u64::from(object_id),
        },
    };
    if !scope_is_active(&records, audit_scope) {
        let _ = catalog.close();
        return Err(format!("{audit_scope:?} 不在未清除的恢复隔离清单中"));
    }

    let detail = match scope {
        VerifyScope::Page { file_id, block_id } => {
            catalog
                .close()
                .map_err(|error| format!("关闭目录失败：{error}"))?;
            verify_page(&io, root, workspace, file_id, block_id)?;
            format!("离线管理员验证页面 {file_id}:{block_id} 的校验和、地址与工作区身份通过")
        }
        VerifyScope::Object { object_id } => {
            let snapshot = CommitSeq::from_raw((1u64 << 48) - 1).expect("48 位最大提交序号");
            let checked = (|| {
                let object = catalog
                    .resolve_by_obj(snapshot, object_id)
                    .map_err(|error| format!("对象目录验证失败：{error}"))?;
                if object.status == 0 || object.dataobj == 0 {
                    return Err(format!(
                        "对象 {object_id} 不是活动持久段（status={}，dataobj={}）",
                        object.status, object.dataobj
                    ));
                }
                let blocks = catalog
                    .verify_segment_media(object.obj, object.dataobj)
                    .map_err(|error| format!("对象介质验证失败：{error}"))?;
                Ok::<_, String>((object.name, blocks))
            })();
            let close = catalog
                .close()
                .map_err(|error| format!("关闭目录失败：{error}"));
            let (name, blocks) = checked?;
            close?;
            format!("离线管理员验证对象 {object_id}（{name}）的目录、段头及 {blocks} 个数据页通过")
        }
    };
    let mut journal = RecoveryJournal::open(&io, &audit_path, workspace)
        .map_err(|error| format!("打开恢复审计失败：{error}"))?;
    journal
        .append_scoped(
            RecoveryState::Verified,
            0,
            audit_scope,
            "bicdb/offline-admin",
            &detail,
        )
        .map_err(|error| format!("Verified 审计落盘失败：{error}"))?;
    drop(journal);
    Ok(detail)
}

/// Archive the complete validated audit and atomically replace the live file
/// with a compact journal containing only the last unverified state per exact
/// scope. The byte-for-byte archive is synced before the replacement.
pub fn archive(params: &InstanceParams) -> Result<(PathBuf, usize), String> {
    use std::io::{Read, Write};

    let io = OsFileIo::new();
    let root = &params.db_root;
    let _lock = InstanceLock::acquire(root, LockMode::Direct, &params.socket_path())
        .map_err(|error| format!("离线归档要求实例已停止：{error}"))?;
    let meta_path = crate::boot::find_meta_file(root).ok_or("工作区缺少元数据文件")?;
    let catalog = bicdb_catalog::Catalog::open(&io, &meta_path)
        .map_err(|error| format!("打开目录失败：{error}"))?;
    let workspace = catalog.workspace();
    catalog
        .close()
        .map_err(|error| format!("关闭目录失败：{error}"))?;
    let audit_path = root.join("recovery.audit");
    let records = read_records(&io, &audit_path, workspace)
        .map_err(|error| format!("读取恢复审计失败：{error}"))?;
    if records.is_empty() {
        return Err("恢复审计为空，无需归档".into());
    }
    let last_sequence = records.last().expect("非空").sequence;
    let archive_path = root.join(format!(
        "recovery.audit.archive.{last_sequence:020}.{}",
        std::process::id()
    ));
    let mut source =
        std::fs::File::open(&audit_path).map_err(|error| format!("打开待归档审计失败：{error}"))?;
    let mut archive_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&archive_path)
        .map_err(|error| format!("创建归档 {} 失败：{error}", archive_path.display()))?;
    let mut bytes = Vec::new();
    source
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取待归档审计失败：{error}"))?;
    archive_file
        .write_all(&bytes)
        .and_then(|()| archive_file.sync_all())
        .map_err(|error| format!("写入归档失败：{error}"))?;
    sync_parent(root)?;

    let mut latest: Vec<&bicdb_storage::recovery_journal::RecoveryRecord> = Vec::new();
    for record in &records {
        if let Some(slot) = latest.iter_mut().find(|saved| saved.scope == record.scope) {
            *slot = record;
        } else {
            latest.push(record);
        }
    }
    latest.retain(|record| record.state != RecoveryState::Verified);
    latest.sort_by_key(|record| record.sequence);

    let temp_path = root.join(format!(".recovery.audit.compact.{}", std::process::id()));
    if temp_path.exists() {
        return Err(format!("临时归档文件已存在：{}", temp_path.display()));
    }
    let build = (|| {
        let mut compact = RecoveryJournal::open(&io, &temp_path, workspace)
            .map_err(|error| format!("创建压缩审计失败：{error}"))?;
        for record in &latest {
            let mut detail = format!(
                "归档后保留原 sequence {}：{}",
                record.sequence, record.detail
            );
            if detail.len() > 4096 {
                let mut end = 4096;
                while !detail.is_char_boundary(end) {
                    end -= 1;
                }
                detail.truncate(end);
            }
            compact
                .append_scoped(
                    record.state,
                    record.recovery_lsn,
                    record.scope,
                    &record.actor,
                    &detail,
                )
                .map_err(|error| format!("重建活动恢复范围失败：{error}"))?;
        }
        drop(compact);
        std::fs::rename(&temp_path, &audit_path)
            .map_err(|error| format!("原子发布压缩审计失败：{error}"))?;
        sync_parent(root)?;
        Ok::<_, String>(())
    })();
    if build.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    build?;
    Ok((archive_path, latest.len()))
}

fn sync_parent(root: &Path) -> Result<(), String> {
    std::fs::File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("同步工作区目录失败：{error}"))
}

fn scope_is_active(
    records: &[bicdb_storage::recovery_journal::RecoveryRecord],
    wanted: RecoveryScope,
) -> bool {
    records.iter().fold(false, |active, record| {
        if record.scope == wanted {
            record.state != RecoveryState::Verified
        } else {
            active
        }
    })
}

fn verify_page(
    io: &OsFileIo,
    root: &Path,
    workspace: [u8; 8],
    file_id: u16,
    block_id: u32,
) -> Result<(), String> {
    let cf = ControlFile::open(io, &root.join(CF_A), &root.join(CF_B))
        .map_err(|error| format!("打开控制文件失败：{error}"))?;
    let records = cf
        .data_file_records()
        .map_err(|error| format!("读取数据文件清单失败：{error}"))?;
    cf.close()
        .map_err(|error| format!("关闭控制文件失败：{error}"))?;
    let record = records
        .iter()
        .find(|record| record.file_id == file_id && record.status != 0)
        .ok_or_else(|| format!("文件号 {file_id} 不在活动数据文件清单中"))?;
    if record.role == 2 {
        return Err("临时文件不进入恢复审计，拒绝将其作为 PAGE 修复目标".into());
    }
    let path = PathBuf::from(
        std::str::from_utf8(record.path())
            .map_err(|_| format!("文件号 {file_id} 的路径不是 UTF-8"))?,
    );
    let file = DataFile::open(io, &path).map_err(|error| format!("打开数据文件失败：{error}"))?;
    if file.file_id() != file_id || file.workspace_ref() != workspace {
        let _ = file.close();
        return Err(format!("文件号 {file_id} 的文件头身份与工作区不符"));
    }
    let checked = (|| {
        let page = file
            .read_page(block_id)
            .map_err(|error| format!("读取页面 {file_id}:{block_id} 失败：{error}"))?;
        let header = page
            .header()
            .ok_or_else(|| format!("页面 {file_id}:{block_id} 没有有效页头"))?;
        if header.file_id != file_id
            || header.block_id != block_id
            || header.workspace_ref != workspace
        {
            return Err(format!("页面 {file_id}:{block_id} 的页头身份不符"));
        }
        Ok::<_, String>(())
    })();
    let close = file
        .close()
        .map_err(|error| format!("关闭数据文件失败：{error}"));
    checked?;
    close?;
    Ok(())
}
