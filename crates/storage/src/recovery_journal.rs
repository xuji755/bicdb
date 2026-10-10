//! Durable, append-only workspace recovery audit. It is independent of redo:
//! corruption must never be interpreted as a successful or empty recovery.
use bicdb_common::{crc32c, PAGE_SIZE};
use bicdb_workspace::io::{FileHandle, FileIo, OpenOptions};
use std::io;
use std::path::Path;

const MAGIC_V1: &[u8; 8] = b"BICREC01";
const MAGIC_V2: &[u8; 8] = b"BICREC02";
const MAX_RECORDS: u64 = 4096;
const V1_TEXT_START: usize = 64;
const V2_TEXT_START: usize = 88;

/// A recovery transition, without any implied permission to bypass checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecoveryState {
    /// Recovery has started; interruption requires recovery again.
    Recovering = 1,
    /// Full recovery, undo validation and durable checkpoint succeeded.
    Verified = 2,
    /// Recovery failed; the workspace has not been admitted.
    Failed = 3,
    /// Runtime durability failed and the workspace was quarantined.
    RuntimeFault = 4,
    /// Explicit administrator override; never equivalent to verified recovery.
    Forced = 5,
}

/// Machine-readable range affected by a recovery or durability event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecoveryScope {
    /// The entire workspace, including legacy version-1 audit records.
    #[default]
    Workspace,
    /// One physical page.
    Page {
        /// Physical data-file number.
        file_id: u16,
        /// Physical block number within that file.
        block_id: u32,
    },
    /// One catalog object.
    Object {
        /// Workspace-local catalog object identifier.
        object_id: u64,
    },
    /// One transaction identifier in its workspace-local undo domain.
    Transaction {
        /// Encoded workspace-local transaction identifier.
        txn_id: u64,
    },
    /// Half-open workspace-local redo range `[start_lsn, end_lsn)`.
    RedoRange {
        /// Inclusive range start.
        start_lsn: u64,
        /// Exclusive range end.
        end_lsn: u64,
    },
}

/// One retained transition in a workspace's own recovery domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryRecord {
    /// Monotonic audit sequence, unrelated to transaction SCN or LSN.
    pub sequence: u64,
    /// Transition classification.
    pub state: RecoveryState,
    /// Unix timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Recovery position in this workspace's log stream.
    pub recovery_lsn: u64,
    /// Structured affected range; details remain explanatory text only.
    pub scope: RecoveryScope,
    /// Operator or subsystem recording this event.
    pub actor: String,
    /// Error, skipped scope or validation result; scope defaults to workspace.
    pub detail: String,
}

/// Validated audit file; all append operations require exclusive ownership.
/// The instance lifecycle lock and workspace controller serialize ownership.
pub struct RecoveryJournal<'a> {
    io: &'a dyn FileIo,
    handle: FileHandle,
    workspace: [u8; 8],
    records: Vec<RecoveryRecord>,
    failed: bool,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Read and validate an existing recovery audit without creating or modifying it.
///
/// This is the only entry point intended for SQL fixed-table inspection.  A
/// missing journal remains missing, and even an error path closes the handle.
pub fn read_records(
    io: &dyn FileIo,
    path: &Path,
    workspace: [u8; 8],
) -> io::Result<Vec<RecoveryRecord>> {
    let handle = io.open(path, OpenOptions::new().read(true))?;
    let result = read_records_from_handle(io, handle, workspace);
    let close = io.close(handle);
    match (result, close) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(records), Ok(())) => Ok(records),
    }
}

fn read_records_from_handle(
    io: &dyn FileIo,
    handle: FileHandle,
    workspace: [u8; 8],
) -> io::Result<Vec<RecoveryRecord>> {
    let length = io.size(handle)?;
    if length % PAGE_SIZE as u64 != 0 || length / PAGE_SIZE as u64 > MAX_RECORDS {
        return Err(invalid(
            "recovery audit has a torn tail or exceeds its capacity",
        ));
    }
    let mut records = Vec::new();
    for index in 0..length / PAGE_SIZE as u64 {
        let mut page = [0u8; PAGE_SIZE];
        io.read_exact_at(handle, &mut page, index * PAGE_SIZE as u64)?;
        records.push(decode(&page, workspace, index + 1)?);
    }
    Ok(records)
}

impl<'a> RecoveryJournal<'a> {
    /// Open and validate the full bounded history. Create only a missing file;
    /// existing corruption, torn tails and foreign identities are hard errors.
    pub fn open(io: &'a dyn FileIo, path: &Path, workspace: [u8; 8]) -> io::Result<Self> {
        let opts = OpenOptions::new().read(true).write(true);
        let (handle, created) = match io.open(path, opts) {
            Ok(handle) => (handle, false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                (io.open(path, opts.create_new(true))?, true)
            }
            Err(error) => return Err(error),
        };
        let result = (|| {
            if created {
                io.sync_all(handle)?;
                let directory = io.open_dir(
                    path.parent()
                        .ok_or_else(|| invalid("audit path has no parent"))?,
                )?;
                let synced = io.sync_dir(directory);
                let closed = io.close(directory);
                synced?;
                closed?;
            }
            let records = read_records_from_handle(io, handle, workspace)?;
            Ok(Self {
                io,
                handle,
                workspace,
                records,
                failed: false,
            })
        })();
        if result.is_err() {
            let _ = io.close(handle);
        }
        result
    }

    /// Retained history, including faults before subsequent verified recovery.
    pub fn records(&self) -> &[RecoveryRecord] {
        &self.records
    }

    /// Append and fsync one event. On write/sync failure this handle is poisoned:
    /// no retry may silently overwrite a partially durable audit event.
    pub fn append(
        &mut self,
        state: RecoveryState,
        recovery_lsn: u64,
        actor: &str,
        detail: &str,
    ) -> io::Result<&RecoveryRecord> {
        self.append_scoped(state, recovery_lsn, RecoveryScope::Workspace, actor, detail)
    }

    /// Append an event with a machine-readable affected range.
    pub fn append_scoped(
        &mut self,
        state: RecoveryState,
        recovery_lsn: u64,
        scope: RecoveryScope,
        actor: &str,
        detail: &str,
    ) -> io::Result<&RecoveryRecord> {
        if self.failed {
            return Err(invalid(
                "recovery audit requires reopen/repair after I/O failure",
            ));
        }
        if self.records.len() as u64 >= MAX_RECORDS {
            return Err(invalid(
                "recovery audit is full; explicit archival is required",
            ));
        }
        if actor.len() > 256 || detail.len() > 4096 {
            return Err(invalid("recovery audit text exceeds bounds"));
        }
        if matches!(
            scope,
            RecoveryScope::RedoRange { start_lsn, end_lsn } if end_lsn < start_lsn
        ) {
            return Err(invalid("recovery audit redo range is reversed"));
        }
        let sequence = self.records.len() as u64 + 1;
        let record = RecoveryRecord {
            sequence,
            state,
            recovery_lsn,
            scope,
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| invalid("recovery audit clock precedes epoch"))?
                .as_millis()
                .try_into()
                .map_err(|_| invalid("recovery audit timestamp overflow"))?,
            actor: actor.into(),
            detail: detail.into(),
        };
        let page = encode(&record, self.workspace);
        // Detect a stale handle before writing, in addition to lifecycle locking.
        if self.io.size(self.handle)? != (sequence - 1) * PAGE_SIZE as u64 {
            self.failed = true;
            return Err(invalid("recovery audit changed outside its owner"));
        }
        if let Err(error) = self
            .io
            .write_at(self.handle, &page, (sequence - 1) * PAGE_SIZE as u64)
            .and_then(|()| self.io.sync_data(self.handle))
        {
            self.failed = true;
            return Err(error);
        }
        self.records.push(record);
        Ok(self.records.last().unwrap())
    }
}
impl Drop for RecoveryJournal<'_> {
    fn drop(&mut self) {
        let _ = self.io.close(self.handle);
    }
}
fn encode(record: &RecoveryRecord, workspace: [u8; 8]) -> [u8; PAGE_SIZE] {
    let mut page = [0u8; PAGE_SIZE];
    page[..8].copy_from_slice(MAGIC_V2);
    page[8..16].copy_from_slice(&workspace);
    page[16..24].copy_from_slice(&record.sequence.to_le_bytes());
    page[24..32].copy_from_slice(&record.timestamp_ms.to_le_bytes());
    page[32..40].copy_from_slice(&record.recovery_lsn.to_le_bytes());
    page[40] = record.state as u8;
    page[42..44].copy_from_slice(&(record.actor.len() as u16).to_le_bytes());
    page[44..46].copy_from_slice(&(record.detail.len() as u16).to_le_bytes());
    let (scope_kind, scope_a, scope_b) = match record.scope {
        RecoveryScope::Workspace => (0, 0, 0),
        RecoveryScope::Page { file_id, block_id } => (1, u64::from(file_id), u64::from(block_id)),
        RecoveryScope::Object { object_id } => (2, object_id, 0),
        RecoveryScope::Transaction { txn_id } => (3, txn_id, 0),
        RecoveryScope::RedoRange { start_lsn, end_lsn } => (4, start_lsn, end_lsn),
    };
    page[41] = scope_kind;
    page[56..64].copy_from_slice(&scope_a.to_le_bytes());
    page[64..72].copy_from_slice(&scope_b.to_le_bytes());
    let split = V2_TEXT_START + record.actor.len();
    page[V2_TEXT_START..split].copy_from_slice(record.actor.as_bytes());
    page[split..split + record.detail.len()].copy_from_slice(record.detail.as_bytes());
    let checksum = crc32c(&page);
    page[48..52].copy_from_slice(&checksum.to_le_bytes());
    page
}
fn decode(page: &[u8; PAGE_SIZE], workspace: [u8; 8], sequence: u64) -> io::Result<RecoveryRecord> {
    let version = if &page[..8] == MAGIC_V1 {
        1
    } else if &page[..8] == MAGIC_V2 {
        2
    } else {
        return Err(invalid("recovery audit version/workspace mismatch"));
    };
    if page[8..16] != workspace {
        return Err(invalid("recovery audit version/workspace mismatch"));
    }
    let mut checked = *page;
    checked[48..52].fill(0);
    if crc32c(&checked) != u32::from_le_bytes(page[48..52].try_into().unwrap()) {
        return Err(invalid("recovery audit checksum mismatch"));
    }
    let actual = u64::from_le_bytes(page[16..24].try_into().unwrap());
    if actual != sequence {
        return Err(invalid("recovery audit sequence mismatch"));
    }
    let state = match page[40] {
        1 => RecoveryState::Recovering,
        2 => RecoveryState::Verified,
        3 => RecoveryState::Failed,
        4 => RecoveryState::RuntimeFault,
        5 => RecoveryState::Forced,
        _ => return Err(invalid("unknown recovery audit state")),
    };
    let actor_length = u16::from_le_bytes(page[42..44].try_into().unwrap()) as usize;
    let detail_length = u16::from_le_bytes(page[44..46].try_into().unwrap()) as usize;
    let text_start = if version == 1 {
        V1_TEXT_START
    } else {
        V2_TEXT_START
    };
    let scope = match (version, page[41]) {
        (1, 0) | (2, 0) => RecoveryScope::Workspace,
        (2, 1) => RecoveryScope::Page {
            file_id: u64::from_le_bytes(page[56..64].try_into().unwrap())
                .try_into()
                .map_err(|_| invalid("recovery audit page file id overflow"))?,
            block_id: u64::from_le_bytes(page[64..72].try_into().unwrap())
                .try_into()
                .map_err(|_| invalid("recovery audit page block id overflow"))?,
        },
        (2, 2) => RecoveryScope::Object {
            object_id: u64::from_le_bytes(page[56..64].try_into().unwrap()),
        },
        (2, 3) => RecoveryScope::Transaction {
            txn_id: u64::from_le_bytes(page[56..64].try_into().unwrap()),
        },
        (2, 4) => {
            let start_lsn = u64::from_le_bytes(page[56..64].try_into().unwrap());
            let end_lsn = u64::from_le_bytes(page[64..72].try_into().unwrap());
            if end_lsn < start_lsn {
                return Err(invalid("recovery audit redo range is reversed"));
            }
            RecoveryScope::RedoRange { start_lsn, end_lsn }
        }
        _ => return Err(invalid("unknown recovery audit scope")),
    };
    if actor_length > 256
        || detail_length > 4096
        || page[46..48] != [0; 2]
        || (version == 1 && page[52..V1_TEXT_START].iter().any(|byte| *byte != 0))
        || (version == 2
            && (page[52..56].iter().any(|byte| *byte != 0)
                || page[72..V2_TEXT_START].iter().any(|byte| *byte != 0)))
        || page[text_start + actor_length + detail_length..]
            .iter()
            .any(|byte| *byte != 0)
    {
        return Err(invalid("recovery audit has invalid lengths/reserved bytes"));
    }
    let split = text_start + actor_length;
    Ok(RecoveryRecord {
        sequence,
        state,
        timestamp_ms: u64::from_le_bytes(page[24..32].try_into().unwrap()),
        recovery_lsn: u64::from_le_bytes(page[32..40].try_into().unwrap()),
        scope,
        actor: std::str::from_utf8(&page[text_start..split])
            .map_err(|_| invalid("invalid audit actor UTF-8"))?
            .into(),
        detail: std::str::from_utf8(&page[split..split + detail_length])
            .map_err(|_| invalid("invalid audit detail UTF-8"))?
            .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bicdb_workspace::io::{FaultInjecting, FaultOp, FaultRule, MemFileIo};
    const WS: [u8; 8] = [11; 8];
    const PATH: &str = "/mem/recovery.audit";
    fn memory() -> MemFileIo {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        io
    }
    #[test]
    fn retains_fault_history_and_workspace_local_recovery_positions() {
        let io = memory();
        let mut journal = RecoveryJournal::open(&io, Path::new(PATH), WS).unwrap();
        journal
            .append(RecoveryState::Failed, 81, "recovery", "undo 槽损坏")
            .unwrap();
        journal
            .append(RecoveryState::Forced, 82, "administrator", "跳过对象 17")
            .unwrap();
        journal
            .append(RecoveryState::Recovering, 80, "recovery", "重新校验")
            .unwrap();
        journal
            .append(RecoveryState::Verified, 99, "recovery", "全部通过")
            .unwrap();
        drop(journal);
        let journal = RecoveryJournal::open(&io, Path::new(PATH), WS).unwrap();
        assert_eq!(journal.records().len(), 4);
        assert_eq!(journal.records()[0].detail, "undo 槽损坏");
        assert_eq!(journal.records()[1].state, RecoveryState::Forced);
        assert_eq!(journal.records()[3].recovery_lsn, 99);
        assert!(RecoveryJournal::open(&io, Path::new(PATH), [12; 8]).is_err());
    }

    #[test]
    fn readonly_inspection_never_creates_a_missing_journal_or_leaks_a_handle() {
        let io = memory();
        let error = read_records(&io, Path::new(PATH), WS).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(io.open_handle_count(), 0);
        assert!(io
            .open(Path::new(PATH), OpenOptions::new().read(true))
            .is_err());

        let mut journal = RecoveryJournal::open(&io, Path::new(PATH), WS).unwrap();
        journal
            .append(RecoveryState::RuntimeFault, 42, "dbwr", "write failed")
            .unwrap();
        drop(journal);
        let records = read_records(&io, Path::new(PATH), WS).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, RecoveryState::RuntimeFault);
        assert_eq!(io.open_handle_count(), 0);

        assert!(read_records(&io, Path::new(PATH), [12; 8]).is_err());
        assert_eq!(io.open_handle_count(), 0);
    }

    #[test]
    fn structured_scopes_roundtrip_and_reversed_redo_is_never_written() {
        let io = memory();
        let mut journal = RecoveryJournal::open(&io, Path::new(PATH), WS).unwrap();
        let scopes = [
            RecoveryScope::Page {
                file_id: 3,
                block_id: 99,
            },
            RecoveryScope::Object { object_id: 7001 },
            RecoveryScope::Transaction {
                txn_id: 0x0102_0304,
            },
            RecoveryScope::RedoRange {
                start_lsn: 500,
                end_lsn: 900,
            },
        ];
        for scope in scopes {
            journal
                .append_scoped(RecoveryState::Failed, 900, scope, "test", "affected range")
                .unwrap();
        }
        assert!(journal
            .append_scoped(
                RecoveryState::Failed,
                900,
                RecoveryScope::RedoRange {
                    start_lsn: 901,
                    end_lsn: 900,
                },
                "test",
                "bad range",
            )
            .is_err());
        assert_eq!(journal.records().len(), scopes.len());
        drop(journal);
        let records = read_records(&io, Path::new(PATH), WS).unwrap();
        assert_eq!(records.iter().map(|r| r.scope).collect::<Vec<_>>(), scopes);
    }
    #[test]
    fn torn_tail_is_rejected_and_never_replaced_by_an_empty_audit() {
        let io = FaultInjecting::new(memory());
        let mut journal = RecoveryJournal::open(&io, Path::new(PATH), WS).unwrap();
        journal
            .append(RecoveryState::Recovering, 0, "recovery", "start")
            .unwrap();
        io.add_rule(FaultRule::torn_write(2, 77, io::ErrorKind::Other));
        assert!(journal
            .append(RecoveryState::Failed, 1, "recovery", "failure")
            .is_err());
        assert!(journal
            .append(RecoveryState::Verified, 2, "recovery", "false success")
            .is_err());
        drop(journal);
        assert!(RecoveryJournal::open(&io, Path::new(PATH), WS).is_err());
        let handle = io
            .open(Path::new(PATH), OpenOptions::new().read(true))
            .unwrap();
        assert_eq!(io.size(handle).unwrap(), PAGE_SIZE as u64 + 77);
        io.close(handle).unwrap();
    }
    #[test]
    fn full_record_corruption_is_rejected() {
        let io = memory();
        let mut journal = RecoveryJournal::open(&io, Path::new(PATH), WS).unwrap();
        journal
            .append(RecoveryState::Verified, 7, "recovery", "verified")
            .unwrap();
        io.write_at(journal.handle, b"!", 70).unwrap();
        drop(journal);
        assert!(RecoveryJournal::open(&io, Path::new(PATH), WS).is_err());
    }
    #[test]
    fn sync_failure_cannot_be_followed_by_a_successful_append() {
        let io = FaultInjecting::new(memory());
        let mut journal = RecoveryJournal::open(&io, Path::new(PATH), WS).unwrap();
        io.add_rule(FaultRule::once(FaultOp::SyncData, 1, io::ErrorKind::Other));
        assert!(journal
            .append(RecoveryState::Failed, 1, "recovery", "disk failure")
            .is_err());
        assert!(journal
            .append(RecoveryState::Verified, 2, "recovery", "false success")
            .is_err());
        assert!(journal.records().is_empty());
    }
    #[test]
    fn stale_owner_and_oversized_input_cannot_overwrite_history() {
        let io = memory();
        let mut first = RecoveryJournal::open(&io, Path::new(PATH), WS).unwrap();
        let mut stale = RecoveryJournal::open(&io, Path::new(PATH), WS).unwrap();
        assert!(first
            .append(RecoveryState::Recovering, 0, &"x".repeat(257), "start")
            .is_err());
        assert!(first
            .append(RecoveryState::Recovering, 0, "open", &"文".repeat(1366))
            .is_err());
        assert_eq!(io.size(first.handle).unwrap(), 0);
        first
            .append(RecoveryState::Failed, 11, "open", "actual failure")
            .unwrap();
        assert!(stale
            .append(RecoveryState::Verified, 12, "open", "stale success")
            .is_err());
        drop(first);
        drop(stale);
        let journal = RecoveryJournal::open(&io, Path::new(PATH), WS).unwrap();
        assert_eq!(journal.records().len(), 1);
        assert_eq!(journal.records()[0].state, RecoveryState::Failed);
    }

    #[test]
    fn valid_checksum_does_not_bypass_structural_validation() {
        let record = RecoveryRecord {
            sequence: 1,
            state: RecoveryState::Recovering,
            timestamp_ms: 1,
            recovery_lsn: 0,
            scope: RecoveryScope::Workspace,
            actor: "open".into(),
            detail: "start".into(),
        };
        let valid = encode(&record, WS);
        for (offset, value) in [(40, 99), (41, 99), (16, 2), (43, 255), (46, 1), (72, 255)] {
            let mut page = valid;
            page[offset] = value;
            page[48..52].fill(0);
            let checksum = crc32c(&page);
            page[48..52].copy_from_slice(&checksum.to_le_bytes());
            assert!(decode(&page, WS, 1).is_err(), "invalid field at {offset}");
        }
    }

    #[test]
    fn legacy_v1_workspace_records_remain_readable() {
        let record = RecoveryRecord {
            sequence: 1,
            state: RecoveryState::Verified,
            timestamp_ms: 3,
            recovery_lsn: 9,
            scope: RecoveryScope::Workspace,
            actor: "old".into(),
            detail: "version one".into(),
        };
        let mut page = encode(&record, WS);
        page[..8].copy_from_slice(MAGIC_V1);
        page[41] = 0;
        page[V1_TEXT_START..V1_TEXT_START + record.actor.len()]
            .copy_from_slice(record.actor.as_bytes());
        let detail_start = V1_TEXT_START + record.actor.len();
        page[detail_start..detail_start + record.detail.len()]
            .copy_from_slice(record.detail.as_bytes());
        page[V2_TEXT_START..V2_TEXT_START + record.actor.len() + record.detail.len()].fill(0);
        page[48..52].fill(0);
        let checksum = crc32c(&page);
        page[48..52].copy_from_slice(&checksum.to_le_bytes());
        let decoded = decode(&page, WS, 1).unwrap();
        assert_eq!(decoded.scope, RecoveryScope::Workspace);
        assert_eq!(decoded.actor, "old");
        assert_eq!(decoded.detail, "version one");
    }

    #[test]
    fn failed_directory_sync_prevents_recovery_from_starting() {
        let io = FaultInjecting::new(memory());
        io.add_rule(FaultRule::once(FaultOp::SyncDir, 1, io::ErrorKind::Other));
        assert!(RecoveryJournal::open(&io, Path::new(PATH), WS).is_err());
    }
}
