use super::*;

#[test]
fn private_inconsistency_can_enter_read_only_but_cannot_silently_become_writable() {
    assert!(workspace_open_uses_narrow_force(
        bicdb_sql::WorkspaceOpenMode::ReadOnly
    ));
    assert!(workspace_open_uses_narrow_force(
        bicdb_sql::WorkspaceOpenMode::ReadWriteForce
    ));
    assert!(!workspace_open_uses_narrow_force(
        bicdb_sql::WorkspaceOpenMode::ReadWrite
    ));
    assert!(
        validate_forced_write_transition(true, bicdb_sql::WorkspaceOpenMode::ReadWrite).is_err()
    );
    assert!(
        validate_forced_write_transition(true, bicdb_sql::WorkspaceOpenMode::ReadWriteForce)
            .is_ok()
    );
    assert!(
        validate_forced_write_transition(false, bicdb_sql::WorkspaceOpenMode::ReadWrite).is_ok()
    );
}
use bicdb_catalog::Catalog;
use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_storage::buffer::{BufferPool, CacheConfig, SystemClock, WalGuard};
use bicdb_storage::controlfile::{
    ArchiveMode, ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry,
};
use bicdb_storage::datafile::DataFile;
use bicdb_storage::undo::{create_undo_segment, UndoChain};
use bicdb_txn::engine::Engine;
use bicdb_wal::group::{GroupSpec, GroupWriter};
use bicdb_workspace::io::MemFileIo;
use bicdb_workspace::WorkspaceId;
const WS: [u8; 8] = [23; 8];
fn seq(v: u64) -> CommitSeq {
    CommitSeq::from_raw(v).unwrap()
}
fn lsn(v: u64) -> Lsn {
    Lsn::from_raw(v).unwrap()
}
struct FakeWal;
impl WalGuard for FakeWal {
    fn durable_lsn(&self) -> Lsn {
        Lsn::from_raw(u64::MAX >> 16).unwrap()
    }
    fn ensure_durable(&self, _t: Lsn) -> std::io::Result<()> {
        Ok(())
    }
}

/// 一个工作区（file 0 + 池 + 日志 + 引擎）。
struct Ws {
    pool: &'static BufferPool<'static>,
    engine: &'static Engine<'static, 'static, 'static, 'static>,
    path: String,
}

fn workspace(io: &'static MemFileIo, tag: &str) -> Ws {
    let undo_path = format!("/mem/{tag}_undo.dat");
    let data_path = format!(
        "/mem/{tag}/data/{}",
        crate::boot::data_file_name(WS, "meta")
    );
    io.add_dir(format!("/mem/{tag}"));
    io.add_dir(format!("/mem/{tag}/data"));
    let undo_file: &'static mut DataFile<'static> = Box::leak(Box::new(
        DataFile::create(io, Path::new(&undo_path), 1, 1, WS, 512).unwrap(),
    ));
    let undo_handle = undo_file.handle();
    let undo_seg = create_undo_segment(undo_file, 2, 3, 4).unwrap();
    let mut file0 = DataFile::create(
        io,
        Path::new(&data_path),
        0,
        bicdb_storage::bitmap::META_ROLE,
        WS,
        bicdb_storage::bitmap::FileLayout::meta().min_file_blocks() + 512,
    )
    .unwrap();
    let built = bicdb_catalog::create_dictionary(&mut file0, WS, false).unwrap();
    let mut cat = Catalog::from_entries(file0, built.entries.clone()).unwrap();
    cat.seed_own_dictionary(&built).unwrap();
    drop(cat);
    let handle = DataFile::open(io, Path::new(&data_path)).unwrap().handle();
    let pool: &'static BufferPool<'static> = Box::leak(Box::new(
        BufferPool::with_config(
            io,
            64,
            move |_ws, r| match r.file_id() {
                0 => Some((handle, r.block_id())),
                1 => Some((undo_handle, r.block_id())),
                _ => None,
            },
            FakeWal,
            SystemClock,
            CacheConfig::for_capacity(64),
        )
        .unwrap(),
    ));
    let cf_a = format!("/mem/{tag}_cf_a");
    let cf_b = format!("/mem/{tag}_cf_b");
    let wal = format!("/mem/{tag}_wal");
    let cf: &'static mut ControlFile<'static> = Box::leak(Box::new(
        ControlFile::format(
            io,
            Path::new(&cf_a),
            Path::new(&cf_b),
            &WorkspaceEntry {
                workspace_id: WorkspaceId::from_raw(1).unwrap(),
                created_at: 0,
                derived_from: None,
                derived_at_seq: seq(0),
            },
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::new(ArchiveMode::NoArchive),
        )
        .unwrap(),
    ));
    let spec = GroupSpec::new(2, 1, 8192).unwrap();
    let writer = GroupWriter::create(io, cf, Path::new(&wal), spec, lsn(0)).unwrap();
    let engine: &'static Engine<'static, 'static, 'static, 'static> =
        Box::leak(Box::new(Engine::new(
            pool,
            writer,
            UndoChain::open(undo_seg).with_pool(pool),
            seq(0),
        )));
    Ws {
        pool,
        engine,
        path: data_path,
    }
}

fn submit(
    work: &mpsc::Sender<WorkerRequest>,
    runtime: &WorkspaceExecution,
    id: u64,
    state: SessionState,
    sql: &str,
) -> mpsc::Receiver<WireReply> {
    let (reply, receive) = mpsc::sync_channel(1);
    work.send(WorkerRequest {
        cancelled: Arc::new(AtomicBool::new(false)),
        id,
        workspace: WS,
        runtime: runtime.clone(),
        state,
        action: WorkerAction::Sql {
            sql: sql.into(),
            params: vec![],
        },
        reply,
    })
    .unwrap();
    receive
}
fn completed(events: &mpsc::Receiver<ServiceEvent>, id: u64) -> SessionState {
    match events.recv_timeout(Duration::from_secs(5)).unwrap() {
        ServiceEvent::Completed {
            id: actual,
            state,
            error,
            ..
        } => {
            assert_eq!(actual, id);
            assert!(error.is_none(), "worker error: {error:?}");
            state
        }
        _ => panic!("expected worker completion"),
    }
}

fn fixture_worker(
    tag: &str,
) -> (
    WorkspaceExecution,
    mpsc::Sender<WorkerRequest>,
    mpsc::Receiver<ServiceEvent>,
    std::thread::JoinHandle<()>,
) {
    let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
    io.add_dir("/mem");
    let ws = workspace(io, tag);
    let runtime = WorkspaceExecution {
        pool: ws.pool,
        engine: ws.engine,
        io,
        dir: PathBuf::from(format!("/mem/{tag}")),
        workspace: WS,
    };
    assert!(ws.path.ends_with(&crate::boot::data_file_name(WS, "meta")));
    let mut catalog = runtime.catalog().unwrap();
    bicdb_catalog::ddl::init_dictionary_tables(&mut catalog, ws.engine).unwrap();
    catalog.close().unwrap();
    let (work, receive) = mpsc::channel();
    let (events, event_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        run_sql_worker(
            crate::config::InstanceParams::default(),
            Arc::new(Mutex::new(receive)),
            events,
        )
    });
    (runtime, work, event_rx, handle)
}

#[test]
fn completed_worker_requests_release_catalog_file_handles() {
    let (runtime, work, events, handle) = fixture_worker("worker-handle-lifetime");
    let baseline = runtime.io.open_handle_count().unwrap();
    let mut state = SessionState::new(runtime.engine.current_seq());
    for id in 1..=64 {
        let response = submit(&work, &runtime, id, state, "SELECT * FROM obj$");
        state = completed(&events, id);
        assert_eq!(response.recv().unwrap().status, "OK");
        assert_eq!(
            runtime.io.open_handle_count().unwrap(),
            baseline,
            "request {id} leaked a FileIo handle"
        );
    }
    drop(work);
    handle.join().unwrap();
}

#[test]
fn recovery_required_registry_keeps_the_first_cause_and_isolates_workspaces() {
    let mut registry = RecoveryRequiredRegistry::default();
    let a = [1; 8];
    let b = [2; 8];
    assert_eq!(registry.record(a, "first".into()), "first");
    assert_eq!(registry.record(a, "later".into()), "first");
    assert_eq!(registry.record(b, "independent".into()), "independent");
    assert_eq!(registry.cause(&a), Some("first"));
    assert_eq!(registry.cause(&b), Some("independent"));
    assert_eq!(registry.len(), 2);
}

#[test]
fn sole_worker_suspends_waiter_then_commits_owner_and_resumes_original_batch() {
    let (runtime, work, event_rx, handle) = fixture_worker("worker-enq");
    let ws = Ws {
        pool: runtime.pool,
        engine: runtime.engine,
        path: String::new(),
    };
    let response = submit(&work, &runtime, 1, SessionState::new(0),
        "CREATE TABLE t (id NUMBER, tag VARCHAR2(30)); INSERT INTO t VALUES (1,'old'); BEGIN; UPDATE t SET tag='owner' WHERE id=1");
    let owner = completed(&event_rx, 1);
    assert_eq!(response.recv().unwrap().status, "OK");
    assert!(owner.in_transaction());
    let mut scheduler = crate::scheduler::Scheduler::new(crate::scheduler::Limits {
        workers: 1,
        instance_queue: 1,
        active_per_workspace: 1,
        queue_per_workspace: 1,
    })
    .unwrap();
    let (reply, waiting_reply) = mpsc::sync_channel(1);
    scheduler.enqueue(WS, WorkerRequest { cancelled: Arc::new(AtomicBool::new(false)), id: 2, workspace: WS, runtime: runtime.clone(),
        state: SessionState::new(ws.engine.current_seq()),
        action: WorkerAction::Sql { sql: "INSERT INTO t VALUES (2,'once'); UPDATE t SET tag='retry' WHERE id=1; SELECT * FROM t ORDER BY id".into(), params: vec![] }, reply,
    }).unwrap_or_else(|_| panic!("admission"));
    dispatch_ready(&mut scheduler, &work).unwrap();
    let (request, wait) = match event_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        ServiceEvent::Suspended { request, wait, .. } => (request, wait),
        _ => panic!("expected cooperative suspension"),
    };
    assert!(
        waiting_reply.try_recv().is_err(),
        "no early client response"
    );
    let slot = scheduler.suspend(WS).unwrap();
    let mut parked = BTreeMap::new();
    parked.insert(
        2,
        ParkedRequest {
            request,
            wait,
            slot,
        },
    );
    assert_eq!(scheduler.metrics().active, 0);
    let (reply, owner_reply) = mpsc::sync_channel(1);
    let action = WorkerAction::Sql {
        sql: "COMMIT".into(),
        params: vec![],
    };
    assert!(is_transaction_end(&action));
    scheduler
        .enqueue_transaction_end(
            WS,
            WorkerRequest {
                cancelled: Arc::new(AtomicBool::new(false)),
                id: 1,
                workspace: WS,
                runtime: runtime.clone(),
                state: owner,
                action,
                reply,
            },
            1,
        )
        .unwrap_or_else(|_| panic!("reserved COMMIT must be admitted"));
    dispatch_ready(&mut scheduler, &work).unwrap();
    completed(&event_rx, 1);
    assert_eq!(owner_reply.recv().unwrap().status, "OK");
    assert!(scheduler.complete(WS));
    resume_row_waits(&mut parked, &mut scheduler, &work, false).unwrap();
    assert!(parked.is_empty());
    let result = completed(&event_rx, 2);
    assert!(!result.in_transaction());
    assert_eq!(waiting_reply.recv().unwrap().status, "OK");
    assert!(scheduler.complete(WS));
    assert_eq!(scheduler.metrics().parked, 0);
    let mut cat = runtime.catalog().unwrap();
    let result = Session::new(ws.pool, ws.engine, &mut cat, ws.engine.current_seq())
        .execute("SELECT * FROM t ORDER BY id")
        .unwrap();
    match &result[0] {
        bicdb_sql::session::QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2, "completed INSERT must not replay");
            assert_eq!(rows[0][1], bicdb_exec::Value::Bytes(b"retry".to_vec()));
        }
        _ => panic!("expected rows"),
    }
    drop(work);
    handle.join().unwrap();
}

#[test]
fn only_a_single_transaction_end_uses_reserved_admission() {
    for sql in ["COMMIT", "rollback;", "/* release */ COMMIT"] {
        assert!(is_transaction_end(&WorkerAction::Sql {
            sql: sql.into(),
            params: vec![]
        }));
    }
    for sql in ["BEGIN", "COMMIT; SELECT 1", "SELECT 'COMMIT'", "nonsense"] {
        assert!(!is_transaction_end(&WorkerAction::Sql {
            sql: sql.into(),
            params: vec![]
        }));
    }
}

#[test]
fn connection_detects_disconnect_while_its_sql_reply_is_pending() {
    let (mut client, server) = std::os::unix::net::UnixStream::pair().unwrap();
    let (events, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || serve_connection(42, server, events));
    frame::write_frame_bytes(&mut client, "SQL", b"waiting").unwrap();
    let _pending_reply = match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
        ServiceEvent::Request { reply, .. } => reply,
        _ => panic!("expected request"),
    };
    drop(client);
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        ServiceEvent::Closed(42)
    ));
    handle.join().unwrap();
}

#[test]
fn disconnect_probe_does_not_consume_pipelined_input() {
    use std::io::Read;
    let (mut client, mut server) = std::os::unix::net::UnixStream::pair().unwrap();
    assert!(!peer_disconnected(&server));
    client.write_all(b"x").unwrap();
    assert!(!peer_disconnected(&server));
    let mut byte = [0];
    server.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [b'x']);
    drop(client);
    assert!(peer_disconnected(&server));
}

#[test]
fn cancelled_queued_request_rolls_back_without_executing_its_sql() {
    let (runtime, work, events, handle) = fixture_worker("worker-disconnect");
    let response = submit(&work, &runtime, 1, SessionState::new(0),
        "CREATE TABLE t (id NUMBER, tag VARCHAR2(30)); INSERT INTO t VALUES (1,'old'); BEGIN; UPDATE t SET tag='uncommitted' WHERE id=1");
    let state = completed(&events, 1);
    assert_eq!(response.recv().unwrap().status, "OK");
    let (reply, response) = mpsc::sync_channel(1);
    work.send(WorkerRequest {
        cancelled: Arc::new(AtomicBool::new(true)),
        id: 1,
        workspace: WS,
        runtime: runtime.clone(),
        state,
        action: WorkerAction::Sql {
            sql: "COMMIT; INSERT INTO t VALUES (2,'unwanted')".into(),
            params: vec![],
        },
        reply,
    })
    .unwrap();
    match events.recv_timeout(Duration::from_secs(5)).unwrap() {
        ServiceEvent::Completed { state, error, .. } => {
            assert!(!state.in_transaction());
            assert!(error.unwrap().contains("连接已断开"));
        }
        _ => panic!("cancel must complete, not suspend"),
    }
    assert_eq!(response.recv().unwrap().status, "ERR");
    let mut catalog = runtime.catalog().unwrap();
    let result = Session::new(
        runtime.pool,
        runtime.engine,
        &mut catalog,
        runtime.engine.current_seq(),
    )
    .execute("SELECT * FROM t")
    .unwrap();
    match &result[0] {
        bicdb_sql::session::QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][1], bicdb_exec::Value::Bytes(b"old".to_vec()));
        }
        _ => panic!("expected rows"),
    }
    drop(work);
    handle.join().unwrap();
}

#[test]
fn fast_stop_drains_parked_auto_transaction_through_a_worker() {
    let (runtime, work, events, handle) = fixture_worker("worker-stop");
    let response = submit(&work, &runtime, 1, SessionState::new(0),
        "CREATE TABLE t (id NUMBER, tag VARCHAR2(30)); INSERT INTO t VALUES (1,'old'); BEGIN; UPDATE t SET tag='owner' WHERE id=1");
    let owner = completed(&events, 1);
    assert_eq!(response.recv().unwrap().status, "OK");
    let mut scheduler = crate::scheduler::Scheduler::new(crate::scheduler::Limits {
        workers: 1,
        instance_queue: 1,
        active_per_workspace: 1,
        queue_per_workspace: 1,
    })
    .unwrap();
    let (reply, response) = mpsc::sync_channel(1);
    scheduler
        .enqueue(
            WS,
            WorkerRequest {
                cancelled: Arc::new(AtomicBool::new(false)),
                id: 2,
                workspace: WS,
                runtime: runtime.clone(),
                state: SessionState::new(runtime.engine.current_seq()),
                action: WorkerAction::Sql {
                    sql: "UPDATE t SET tag='waiting' WHERE id=1".into(),
                    params: vec![],
                },
                reply,
            },
        )
        .unwrap_or_else(|_| panic!("admit waiter"));
    dispatch_ready(&mut scheduler, &work).unwrap();
    let (request, wait) = match events.recv_timeout(Duration::from_secs(5)).unwrap() {
        ServiceEvent::Suspended { request, wait, .. } => (request, wait),
        _ => panic!("expected row wait"),
    };
    let slot = scheduler.suspend(WS).unwrap();
    let mut parked = BTreeMap::new();
    parked.insert(
        2,
        ParkedRequest {
            request,
            wait,
            slot,
        },
    );
    resume_row_waits(&mut parked, &mut scheduler, &work, true).unwrap();
    match events.recv_timeout(Duration::from_secs(5)).unwrap() {
        ServiceEvent::Completed { state, error, .. } => {
            assert!(!state.in_transaction());
            assert!(error.unwrap().contains("实例正在停止"));
        }
        _ => panic!("stopping waiter must finish cleanup"),
    }
    assert_eq!(response.recv().unwrap().status, "ERR");
    assert!(scheduler.complete(WS));
    assert_eq!(scheduler.metrics().parked, 0);
    assert_eq!(scheduler.metrics().active, 0);
    assert_eq!(scheduler.metrics().queued, 0);
    let response = submit(&work, &runtime, 1, owner, "ROLLBACK");
    completed(&events, 1);
    assert_eq!(response.recv().unwrap().status, "OK");
    let response = submit(
        &work,
        &runtime,
        3,
        SessionState::new(runtime.engine.current_seq()),
        "UPDATE t SET tag='usable' WHERE id=1",
    );
    completed(&events, 3);
    assert_eq!(response.recv().unwrap().status, "OK", "no leaked row locks");
    drop(work);
    handle.join().unwrap();
}

#[test]
fn workspace_bind_limit_counts_open_workspaces_not_cache_capacity() {
    assert!(bound_workspace_capacity(0, 1, true).is_ok());
    assert!(bound_workspace_capacity(0, 1, false).is_err());
    assert!(bound_workspace_capacity(2, 4, false).is_ok());
    assert!(bound_workspace_capacity(3, 4, false).is_err());
    assert!(bound_workspace_capacity(4, 4, true).is_ok());
}

#[test]
fn parameter_or_lock_errors_do_not_become_recovery_required() {
    let mut registry = RecoveryRequiredRegistry::default();
    remember_recovery_failure(
        &mut registry,
        WS,
        &crate::boot::BootError::Config("bad setting".into()),
    );
    remember_recovery_failure(
        &mut registry,
        WS,
        &crate::boot::BootError::Occupied("busy".into()),
    );
    assert_eq!(registry.len(), 0);
    remember_recovery_failure(
        &mut registry,
        WS,
        &crate::boot::BootError::Catalog("recovery validation failed".into()),
    );
    assert_eq!(registry.len(), 1);
}
