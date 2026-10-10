//! **固定表**（`arch/03` §3.1.4；C3 的 `file$` 一片）。
//!
//! ```text
//! 固定表 = 行由引擎**在查询时即时产生**，不来自任何段、不入 obj$、不落盘。
//! ```
//!
//! **它为什么必须是固定表**（`arch/03` §3.1.4 的两条判据）：① 权威已经在
//! 文件或内存里（文件清单的权威是**控制文件**——纪律 7）；② 它需要被 SQL 查询。
//! 再补一条实现差异：**固定表不需要统计信息**——它在产生行之前就知道自己
//! 有多少行（[`FixedTable::cardinality`]）。
//!
//! **`file$` 的列形状（本模块定案，C3 落地时冻入 `目录详设` §6）**：
//!
//! | 列 | 类型 | 来源 |
//! | --- | --- | --- |
//! | `file#` | NUMBER | [`DataFileRecord::file_id`] |
//! | `role` | NUMBER | `role` |
//! | `status` | NUMBER | `status` |
//! | `flags` | NUMBER | `flags` |
//! | `creation_blocks` | NUMBER | `creation_blocks`（创建时大小；当前大小以文件头自述为准） |
//! | `created_at` | NUMBER | `created_at`（墙钟时戳；标量） |
//! | `path` | VARCHAR2 | `path()`（OS 字节；UTF-8 可解则文本，否则字节串） |
//!
//! **只读**：固定表没有写入口（`spec/SQL.md` 的固定表命名空间口径）——要改
//! 文件清单只能去改控制文件（DDL/管理操作），下次查询自然反映。

use bicdb_storage::controlfile::DataFileRecord;
use bicdb_storage::recovery_journal::{RecoveryRecord, RecoveryScope, RecoveryState};

use crate::dict::ColTypeCode;
use crate::row::DictValue;

/// 固定表的一列（名字 + 类型码；**没有**列号之外的定义——固定表的行是即时产生的）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedColumn {
    /// 列名。
    pub name: &'static str,
    /// 类型码（与字典表同一值域）。
    pub type_code: ColTypeCode,
}

/// 一张固定表的即时内容。
#[derive(Debug, Clone, PartialEq)]
pub struct FixedTable {
    /// 表名（保留名；`$` 结尾）。
    pub name: &'static str,
    /// 列定义。
    pub columns: &'static [FixedColumn],
    /// 行（值域与字典表同一 [`DictValue`]）。
    pub rows: Vec<Vec<DictValue>>,
}

impl FixedTable {
    /// **基数**（引擎直接给出——不靠统计信息，也不靠默认值）。
    #[must_use]
    pub fn cardinality(&self) -> usize {
        self.rows.len()
    }
}

/// `file$` 的列定义。
pub static FILE_COLUMNS: &[FixedColumn] = &[
    FixedColumn {
        name: "file#",
        type_code: ColTypeCode::Number,
    },
    FixedColumn {
        name: "role",
        type_code: ColTypeCode::Number,
    },
    FixedColumn {
        name: "status",
        type_code: ColTypeCode::Number,
    },
    FixedColumn {
        name: "flags",
        type_code: ColTypeCode::Number,
    },
    FixedColumn {
        name: "creation_blocks",
        type_code: ColTypeCode::Number,
    },
    FixedColumn {
        name: "created_at",
        type_code: ColTypeCode::Number,
    },
    FixedColumn {
        name: "path",
        type_code: ColTypeCode::Varchar2,
    },
];

/// `recovery$` columns, backed by the validated append-only audit journal.
pub static RECOVERY_COLUMNS: &[FixedColumn] = &[
    FixedColumn {
        name: "sequence",
        type_code: ColTypeCode::Number,
    },
    FixedColumn {
        name: "state",
        type_code: ColTypeCode::Varchar2,
    },
    FixedColumn {
        name: "timestamp_ms",
        type_code: ColTypeCode::Number,
    },
    FixedColumn {
        name: "recovery_lsn",
        type_code: ColTypeCode::Number,
    },
    FixedColumn {
        name: "scope_kind",
        type_code: ColTypeCode::Varchar2,
    },
    FixedColumn {
        name: "scope_file",
        type_code: ColTypeCode::Number,
    },
    FixedColumn {
        name: "scope_block",
        type_code: ColTypeCode::Number,
    },
    FixedColumn {
        name: "scope_object",
        type_code: ColTypeCode::Number,
    },
    FixedColumn {
        name: "scope_txn",
        type_code: ColTypeCode::Number,
    },
    FixedColumn {
        name: "scope_lsn_start",
        type_code: ColTypeCode::Number,
    },
    FixedColumn {
        name: "scope_lsn_end",
        type_code: ColTypeCode::Number,
    },
    FixedColumn {
        name: "actor",
        type_code: ColTypeCode::Varchar2,
    },
    FixedColumn {
        name: "detail",
        type_code: ColTypeCode::Varchar2,
    },
];

/// Current workspace recovery history. Rows remain ordered by audit sequence.
#[must_use]
pub fn recovery_table(records: &[RecoveryRecord]) -> FixedTable {
    let rows = records
        .iter()
        .map(|record| {
            let state = match record.state {
                RecoveryState::Recovering => "RECOVERING",
                RecoveryState::Verified => "VERIFIED",
                RecoveryState::Failed => "FAILED",
                RecoveryState::RuntimeFault => "RUNTIME_FAULT",
                RecoveryState::Forced => "FORCED",
            };
            let (scope_kind, scope_file, scope_block, scope_object, scope_txn, lsn_start, lsn_end) =
                match record.scope {
                    RecoveryScope::Workspace => ("WORKSPACE", None, None, None, None, None, None),
                    RecoveryScope::Page { file_id, block_id } => (
                        "PAGE",
                        Some(u64::from(file_id)),
                        Some(u64::from(block_id)),
                        None,
                        None,
                        None,
                        None,
                    ),
                    RecoveryScope::Object { object_id } => {
                        ("OBJECT", None, None, Some(object_id), None, None, None)
                    }
                    RecoveryScope::Transaction { txn_id } => {
                        ("TRANSACTION", None, None, None, Some(txn_id), None, None)
                    }
                    RecoveryScope::RedoRange { start_lsn, end_lsn } => (
                        "REDO_RANGE",
                        None,
                        None,
                        None,
                        None,
                        Some(start_lsn),
                        Some(end_lsn),
                    ),
                };
            let number = |value: Option<u64>| value.map_or(DictValue::Null, DictValue::Num);
            vec![
                DictValue::Num(record.sequence),
                DictValue::Text(state.into()),
                DictValue::Num(record.timestamp_ms),
                DictValue::Num(record.recovery_lsn),
                DictValue::Text(scope_kind.into()),
                number(scope_file),
                number(scope_block),
                number(scope_object),
                number(scope_txn),
                number(lsn_start),
                number(lsn_end),
                DictValue::Text(record.actor.clone()),
                DictValue::Text(record.detail.clone()),
            ]
        })
        .collect();
    FixedTable {
        name: "recovery$",
        columns: RECOVERY_COLUMNS,
        rows,
    }
}

/// **`file$`：由控制文件的数据文件记录产生行**（调用方给"控制文件的内存映像"）。
#[must_use]
pub fn file_table(records: &[DataFileRecord]) -> FixedTable {
    let rows = records
        .iter()
        .map(|r| {
            vec![
                DictValue::Num(u64::from(r.file_id)),
                DictValue::Num(u64::from(r.role)),
                DictValue::Num(u64::from(r.status)),
                DictValue::Num(u64::from(r.flags)),
                DictValue::Num(r.creation_blocks),
                DictValue::Num(r.created_at),
                // 路径是 OS 字节：UTF-8 可解即文本，否则原样字节串（不替换、不失真）。
                match std::str::from_utf8(r.path()) {
                    Ok(s) => DictValue::Text(s.to_owned()),
                    Err(_) => DictValue::Bytes(r.path().to_vec()),
                },
            ]
        })
        .collect();
    FixedTable {
        name: "file$",
        columns: FILE_COLUMNS,
        rows,
    }
}

/// **按名取固定表的列定义**（`None` = 不认识这张固定表）。
///
/// 与 [`table`] 分开：**列是静态的**，而表要控制文件的内存映像才产得出行
/// （绑定期只需要列——`sql::bind` 的 `fixed_columns` 就走这条）。
#[must_use]
pub fn columns(name: &str) -> Option<&'static [FixedColumn]> {
    match name {
        "file$" => Some(FILE_COLUMNS),
        "recovery$" => Some(RECOVERY_COLUMNS),
        _ => None,
    }
}

/// **按名取固定表**（V1.0 只有 `file$`；`session$`/`lock$` 随会话层）。
///
/// `records` = 控制文件内存映像里的已用数据文件记录（[`bicdb_storage::controlfile::ControlFile::data_file_records`]）。
#[must_use]
pub fn table(name: &str, records: &[DataFileRecord]) -> Option<FixedTable> {
    match name {
        "file$" => Some(file_table(records)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<DataFileRecord> {
        let mut a = DataFileRecord::new(0, 0);
        a.status = 1;
        a.creation_blocks = 1024;
        a.created_at = 11;
        a.set_path(b"/mnt/ws/file0.dat").unwrap();
        let mut b = DataFileRecord::new(3, 3);
        b.status = 1;
        b.creation_blocks = 65536;
        b.created_at = 12;
        b.set_path(b"/mnt/ws/data_03.dat").unwrap();
        // 非法 UTF-8 的路径：原样字节串。
        let mut c = DataFileRecord::new(4, 3);
        c.status = 1;
        c.set_path(b"/mnt/ws/\xff\xfe").unwrap();
        vec![a, b, c]
    }

    #[test]
    fn file_table_maps_every_record_column() {
        let rows = sample();
        let t = file_table(&rows);
        assert_eq!(t.name, "file$");
        assert_eq!(t.columns, FILE_COLUMNS);
        assert_eq!(t.cardinality(), 3, "基数直接来自记录数");
        assert_eq!(t.rows[1][0], DictValue::Num(3));
        assert_eq!(t.rows[1][1], DictValue::Num(3), "role = 数据文件");
        assert_eq!(t.rows[1][4], DictValue::Num(65536));
        assert_eq!(
            t.rows[0][6],
            DictValue::Text("/mnt/ws/file0.dat".to_owned())
        );
        assert_eq!(
            t.rows[2][6],
            DictValue::Bytes(b"/mnt/ws/\xff\xfe".to_vec()),
            "非 UTF-8 路径原样承载"
        );
    }

    #[test]
    fn only_file_is_known_and_rows_are_fresh_each_call() {
        let rows = sample();
        assert!(table("file$", &rows).is_some());
        assert!(table("session$", &rows).is_none(), "V1.0 只有 file$");
        // 即时产生：改控制文件映像 ⇒ 下次查询自然反映（"只有一个写入路径"）。
        let mut rows2 = rows.clone();
        rows2.push(DataFileRecord::new(5, 3));
        assert_eq!(table("file$", &rows).unwrap().cardinality(), 3);
        assert_eq!(table("file$", &rows2).unwrap().cardinality(), 4);
    }

    #[test]
    fn recovery_table_maps_audit_states_without_losing_workspace_lsn() {
        let records = vec![
            RecoveryRecord {
                sequence: 7,
                state: RecoveryState::RuntimeFault,
                timestamp_ms: 123,
                recovery_lsn: 456,
                scope: RecoveryScope::Page {
                    file_id: 3,
                    block_id: 99,
                },
                actor: "dbwr-2".into(),
                detail: "datafile write failed".into(),
            },
            RecoveryRecord {
                sequence: 8,
                state: RecoveryState::Forced,
                timestamp_ms: 124,
                recovery_lsn: 457,
                scope: RecoveryScope::Object { object_id: 17 },
                actor: "admin".into(),
                detail: "forced open".into(),
            },
        ];
        let table = recovery_table(&records);
        assert_eq!(table.name, "recovery$");
        assert_eq!(table.columns, RECOVERY_COLUMNS);
        assert_eq!(table.rows[0][0], DictValue::Num(7));
        assert_eq!(table.rows[0][1], DictValue::Text("RUNTIME_FAULT".into()));
        assert_eq!(table.rows[0][3], DictValue::Num(456));
        assert_eq!(table.rows[0][4], DictValue::Text("PAGE".into()));
        assert_eq!(table.rows[0][5], DictValue::Num(3));
        assert_eq!(table.rows[0][6], DictValue::Num(99));
        assert_eq!(table.rows[1][1], DictValue::Text("FORCED".into()));
        assert_eq!(table.rows[1][4], DictValue::Text("OBJECT".into()));
        assert_eq!(table.rows[1][7], DictValue::Num(17));
    }
}
