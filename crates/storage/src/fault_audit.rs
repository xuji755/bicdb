//! One sleeping instance audit writer. The fault delivery path performs no I/O.
use crate::recovery_journal::{RecoveryJournal, RecoveryScope, RecoveryState};
use bicdb_workspace::io::FileIo;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

/// Runtime fault audit progress. Failed persistence never removes quarantine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditStatus {
    /// No fault has been delivered.
    Healthy,
    /// First fault is queued or being written.
    Pending,
    /// The fault has been appended and synced.
    Persisted,
    /// Persistence/delivery failed; administrator attention is required.
    Failed(String),
}
struct Target {
    path: PathBuf,
    status: AuditStatus,
}
struct State {
    targets: BTreeMap<[u8; 8], Target>,
    queue: VecDeque<AuditTask>,
    delivery_errors: BTreeMap<[u8; 8], String>,
    stopping: bool,
}
struct AuditTask {
    workspace: [u8; 8],
    state: RecoveryState,
    recovery_lsn: u64,
    scope: RecoveryScope,
    actor: String,
    detail: String,
    runtime_fault: bool,
    reply: Option<std::sync::mpsc::SyncSender<Result<(), String>>>,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// Cheap clonable fault sink, bounded to one pending event per workspace.
#[derive(Clone)]
pub struct FaultAuditSink {
    shared: Arc<Shared>,
}
impl FaultAuditSink {
    /// Register a recovered workspace before its background or SQL admission.
    /// Registration is metadata only; it never opens a file.
    pub fn register(&self, workspace: [u8; 8], path: &Path) -> io::Result<()> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopping {
            return Err(io::Error::other("fault audit writer is stopping"));
        }
        if let Some(target) = state.targets.get(&workspace) {
            return if target.path == path {
                Ok(())
            } else {
                Err(io::Error::other(
                    "fault audit workspace already registered at another path",
                ))
            };
        }
        if state.delivery_errors.contains_key(&workspace) {
            return Err(io::Error::other(
                "workspace fault was delivered before registration",
            ));
        }
        state.targets.insert(
            workspace,
            Target {
                path: path.into(),
                status: AuditStatus::Healthy,
            },
        );
        Ok(())
    }
    /// Deliver the first fault. No disk I/O or waiting for the audit worker.
    /// Further faults are coalesced because quarantine preserves the first cause.
    pub fn record(&self, workspace: [u8; 8], mut reason: String) {
        if reason.len() > 4096 {
            let mut end = 4096;
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason.truncate(end);
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .targets
            .get(&workspace)
            .is_some_and(|target| target.status != AuditStatus::Healthy)
        {
            return;
        }
        if state.stopping {
            state
                .delivery_errors
                .entry(workspace)
                .or_insert_with(|| "fault delivered after audit shutdown".into());
            return;
        }
        let Some(target) = state.targets.get_mut(&workspace) else {
            state
                .delivery_errors
                .entry(workspace)
                .or_insert_with(|| "fault audit workspace is not registered".into());
            return;
        };
        if target.status != AuditStatus::Healthy {
            return;
        }
        target.status = AuditStatus::Pending;
        state.queue.push_back(AuditTask {
            workspace,
            state: RecoveryState::RuntimeFault,
            recovery_lsn: 0,
            scope: RecoveryScope::Workspace,
            actor: "bicdb/runtime".into(),
            detail: reason,
            runtime_fault: true,
            reply: None,
        });
        self.shared.changed.notify_one();
    }
    /// Serialize and fsync one administrator-verified recovery scope through
    /// the instance audit writer. This never clears an in-memory gate by itself.
    pub fn append_verified(
        &self,
        workspace: [u8; 8],
        recovery_lsn: u64,
        scope: RecoveryScope,
        actor: &str,
        detail: &str,
    ) -> io::Result<()> {
        if actor.len() > 256 || detail.len() > 4096 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "recovery audit text exceeds bounds",
            ));
        }
        let (reply, receive) = std::sync::mpsc::sync_channel(0);
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.stopping {
                return Err(io::Error::other("fault audit writer is stopping"));
            }
            if !state.targets.contains_key(&workspace) {
                return Err(io::Error::other(
                    "recovery audit workspace is not registered",
                ));
            }
            state.queue.push_back(AuditTask {
                workspace,
                state: RecoveryState::Verified,
                recovery_lsn,
                scope,
                actor: actor.into(),
                detail: detail.into(),
                runtime_fault: false,
                reply: Some(reply),
            });
            self.shared.changed.notify_one();
        }
        receive
            .recv()
            .map_err(|_| io::Error::other("recovery audit writer stopped before reply"))?
            .map_err(io::Error::other)
    }
    /// Snapshot without file access; suitable for STATUS/control requests.
    pub fn status(&self, workspace: [u8; 8]) -> AuditStatus {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(error) = state.delivery_errors.get(&workspace) {
            return AuditStatus::Failed(error.clone());
        }
        state
            .targets
            .get(&workspace)
            .map_or(AuditStatus::Healthy, |target| target.status.clone())
    }
    /// Count sticky failures, including delivery to unknown/stopped targets.
    pub fn failures(&self) -> usize {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.delivery_errors.len()
            + state
                .targets
                .values()
                .filter(|t| matches!(t.status, AuditStatus::Failed(_)))
                .count()
    }
}

/// Instance-wide owner of the single audit writer. Idle waits have no timer.
pub struct FaultAuditWriter {
    sink: FaultAuditSink,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl FaultAuditWriter {
    /// Start one audit writer; each workspace has its own append-only file.
    pub fn start(io: &'static dyn FileIo) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                targets: BTreeMap::new(),
                queue: VecDeque::new(),
                delivery_errors: BTreeMap::new(),
                stopping: false,
            }),
            changed: Condvar::new(),
        });
        let sink = FaultAuditSink {
            shared: Arc::clone(&shared),
        };
        let thread = std::thread::Builder::new()
            .name("bicdb-fault-audit".into())
            .spawn(move || {
                loop {
                    let (task, path) = {
                        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                        while state.queue.is_empty() && !state.stopping {
                            state = shared
                                .changed
                                .wait(state)
                                .unwrap_or_else(|e| e.into_inner());
                        }
                        let Some(task) = state.queue.pop_front() else {
                            break;
                        };
                        let path = state.targets[&task.workspace].path.clone();
                        (task, path)
                    };
                    // The state mutex and producer path stay free throughout I/O.
                    let result = (|| {
                        let mut journal = RecoveryJournal::open(io, &path, task.workspace)?;
                        journal.append_scoped(
                            task.state,
                            task.recovery_lsn,
                            task.scope,
                            &task.actor,
                            &task.detail,
                        )?;
                        Ok::<_, io::Error>(())
                    })();
                    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                    if task.runtime_fault {
                        state.targets.get_mut(&task.workspace).unwrap().status = match &result {
                            Ok(()) => AuditStatus::Persisted,
                            Err(error) => AuditStatus::Failed(error.to_string()),
                        };
                    }
                    if let Some(reply) = task.reply {
                        let _ =
                            reply.send(result.as_ref().map(|_| ()).map_err(ToString::to_string));
                    }
                    shared.changed.notify_all();
                }
            })?;
        Ok(Self {
            sink,
            thread: Some(thread),
        })
    }
    /// Fault producers can retain this handle; it does not own a thread.
    pub fn sink(&self) -> FaultAuditSink {
        self.sink.clone()
    }
    /// Stop accepting faults, drain accepted events, join and report failures.
    /// Call after all background producers have stopped, except on immediate exit.
    pub fn finish(mut self) -> io::Result<()> {
        self.stop()?;
        let failures = self.sink.failures();
        if failures != 0 {
            return Err(io::Error::other(format!(
                "{failures} runtime fault audit failures"
            )));
        }
        Ok(())
    }
    fn stop(&mut self) -> io::Result<()> {
        {
            let mut state = self
                .sink
                .shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.stopping = true;
            self.sink.shared.changed.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| io::Error::other("fault audit writer panicked"))?;
        }
        Ok(())
    }
}
impl Drop for FaultAuditWriter {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bicdb_workspace::io::{FaultInjecting, FaultOp, FaultRule, MemFileIo};
    const WS: [u8; 8] = [17; 8];
    #[test]
    fn coalesces_first_fault_and_drains_before_shutdown() {
        let io = Box::leak(Box::new(MemFileIo::new()));
        io.add_dir("/mem");
        let writer = FaultAuditWriter::start(io).unwrap();
        let sink = writer.sink();
        sink.register(WS, Path::new("/mem/recovery.audit")).unwrap();
        sink.record(WS, "first failure".into());
        for _ in 0..100 {
            sink.record(WS, "later failure".into());
        }
        writer.finish().unwrap();
        assert_eq!(sink.status(WS), AuditStatus::Persisted);
        let journal = RecoveryJournal::open(io, Path::new("/mem/recovery.audit"), WS).unwrap();
        assert_eq!(journal.records().len(), 1);
        assert_eq!(journal.records()[0].detail, "first failure");
        assert_eq!(journal.records()[0].state, RecoveryState::RuntimeFault);
    }
    #[test]
    fn verified_scope_is_serialized_and_acknowledged_after_fsync() {
        let io = Box::leak(Box::new(MemFileIo::new()));
        io.add_dir("/mem");
        let writer = FaultAuditWriter::start(io).unwrap();
        let sink = writer.sink();
        sink.register(WS, Path::new("/mem/recovery.audit")).unwrap();
        sink.append_verified(
            WS,
            77,
            RecoveryScope::Page {
                file_id: 3,
                block_id: 9,
            },
            "admin/verify",
            "checksum and identity verified",
        )
        .unwrap();
        writer.finish().unwrap();
        let records =
            crate::recovery_journal::read_records(io, Path::new("/mem/recovery.audit"), WS)
                .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, RecoveryState::Verified);
        assert_eq!(
            records[0].scope,
            RecoveryScope::Page {
                file_id: 3,
                block_id: 9
            }
        );
        assert_eq!(sink.status(WS), AuditStatus::Healthy);
    }
    #[test]
    fn persistence_failure_is_sticky_and_other_workspace_still_persists() {
        let memory = MemFileIo::new();
        memory.add_dir("/mem");
        let io = Box::leak(Box::new(FaultInjecting::new(memory)));
        io.add_rule(FaultRule::once(FaultOp::SyncData, 1, io::ErrorKind::Other));
        let writer = FaultAuditWriter::start(io).unwrap();
        let sink = writer.sink();
        sink.register(WS, Path::new("/mem/failed.audit")).unwrap();
        sink.register([18; 8], Path::new("/mem/healthy.audit"))
            .unwrap();
        sink.record(WS, "first disk fault".into());
        sink.record([18; 8], "another workspace fault".into());
        assert!(writer.finish().is_err());
        assert!(matches!(sink.status(WS), AuditStatus::Failed(_)));
        assert_eq!(sink.status([18; 8]), AuditStatus::Persisted);
        assert_eq!(sink.failures(), 1);
        let original_failure = sink.status(WS);
        sink.record(WS, "retry must not erase failure".into());
        assert_eq!(sink.status(WS), original_failure);
        assert_eq!(sink.failures(), 1);
        let journal = RecoveryJournal::open(io, Path::new("/mem/healthy.audit"), [18; 8]).unwrap();
        assert_eq!(journal.records()[0].detail, "another workspace fault");
    }

    #[test]
    fn unknown_workspace_delivery_cannot_be_reported_as_persisted() {
        let memory = MemFileIo::new();
        memory.add_dir("/mem");
        let io = Box::leak(Box::new(memory));
        let writer = FaultAuditWriter::start(io).unwrap();
        let sink = writer.sink();
        sink.record(WS, "unregistered fault".into());
        assert!(matches!(sink.status(WS), AuditStatus::Failed(_)));
        assert!(sink.register(WS, Path::new("/mem/audit")).is_err());
        assert!(writer.finish().is_err());
    }
    struct BlockedIo {
        memory: MemFileIo,
        gate: (Mutex<bool>, Condvar),
        entered: std::sync::mpsc::Sender<()>,
    }
    impl BlockedIo {
        fn release(&self) {
            *self.gate.0.lock().unwrap() = true;
            self.gate.1.notify_all();
        }
    }
    struct ReleaseOnDrop(&'static BlockedIo);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    impl FileIo for BlockedIo {
        fn open(
            &self,
            path: &Path,
            opts: bicdb_workspace::io::OpenOptions,
        ) -> io::Result<bicdb_workspace::io::FileHandle> {
            self.entered.send(()).unwrap();
            let mut released = self.gate.0.lock().unwrap();
            while !*released {
                released = self.gate.1.wait(released).unwrap();
            }
            self.memory.open(path, opts)
        }
        fn open_dir(&self, path: &Path) -> io::Result<bicdb_workspace::io::FileHandle> {
            self.memory.open_dir(path)
        }
        fn read_at(
            &self,
            handle: bicdb_workspace::io::FileHandle,
            bytes: &mut [u8],
            offset: u64,
        ) -> io::Result<usize> {
            self.memory.read_at(handle, bytes, offset)
        }
        fn write_at(
            &self,
            handle: bicdb_workspace::io::FileHandle,
            bytes: &[u8],
            offset: u64,
        ) -> io::Result<()> {
            self.memory.write_at(handle, bytes, offset)
        }
        fn size(&self, handle: bicdb_workspace::io::FileHandle) -> io::Result<u64> {
            self.memory.size(handle)
        }
        fn set_len(&self, handle: bicdb_workspace::io::FileHandle, len: u64) -> io::Result<()> {
            self.memory.set_len(handle, len)
        }
        fn sync_data(&self, handle: bicdb_workspace::io::FileHandle) -> io::Result<()> {
            self.memory.sync_data(handle)
        }
        fn sync_all(&self, handle: bicdb_workspace::io::FileHandle) -> io::Result<()> {
            self.memory.sync_all(handle)
        }
        fn sync_dir(&self, handle: bicdb_workspace::io::FileHandle) -> io::Result<()> {
            self.memory.sync_dir(handle)
        }
        fn close(&self, handle: bicdb_workspace::io::FileHandle) -> io::Result<()> {
            self.memory.close(handle)
        }
    }
    #[test]
    fn blocked_audit_io_does_not_block_fault_producers_or_status() {
        let memory = MemFileIo::new();
        memory.add_dir("/mem");
        let (entered, received) = std::sync::mpsc::channel();
        let io = Box::leak(Box::new(BlockedIo {
            memory,
            gate: (Mutex::new(false), Condvar::new()),
            entered,
        }));
        let writer = FaultAuditWriter::start(io).unwrap();
        let _release = ReleaseOnDrop(io);
        let sink = writer.sink();
        sink.register(WS, Path::new("/mem/first.audit")).unwrap();
        sink.register([18; 8], Path::new("/mem/second.audit"))
            .unwrap();
        for _ in 0..10 {
            let guard = sink.shared.state.lock().unwrap();
            sink.shared.changed.notify_all();
            drop(guard);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            received.try_recv().is_err(),
            "idle/spurious wakeups must not perform I/O"
        );
        sink.record(WS, "first".into());
        received
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("audit worker must enter I/O");
        let start = std::time::Instant::now();
        sink.record([18; 8], "second".into());
        for _ in 0..100 {
            sink.record(WS, "duplicate".into());
        }
        assert_eq!(sink.status(WS), AuditStatus::Pending);
        assert_eq!(sink.status([18; 8]), AuditStatus::Pending);
        assert_eq!(sink.failures(), 0);
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        io.release();
        writer.finish().unwrap();
        assert_eq!(sink.status(WS), AuditStatus::Persisted);
        assert_eq!(sink.status([18; 8]), AuditStatus::Persisted);
    }
}
