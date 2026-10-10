//! Instance-wide background ownership; workspace log/checkpoint state stays local.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

type Engine = bicdb_txn::engine::Engine<'static, 'static, 'static, 'static>;
type Workspaces =
    Arc<RwLock<BTreeMap<[u8; 8], (&'static Engine, Arc<bicdb_wal::group::WalShared<'static>>)>>>;

/// Last verified checkpoint state in one workspace's own LSN domain.
#[derive(Clone, Copy, Default)]
pub struct CheckpointState {
    /// Attempts, including normal deferrals.
    pub attempts: u64,
    /// Busy boundaries, pending redo or no new safe recovery position.
    pub deferred: u64,
    /// Last successfully published progress; never aggregate LSNs across streams.
    pub progress: Option<bicdb_storage::controlfile::CheckpointProgress>,
}

/// Instance-wide bounded LGWR, CKPT and undo maintenance pools.
pub struct Background {
    worker_counts: (usize, usize, usize),
    workspaces: Workspaces,
    signal: Arc<(Mutex<u64>, Condvar)>,
    stop: Arc<AtomicBool>,
    handles: Vec<std::thread::JoinHandle<()>>,
    dbwr: Option<Arc<bicdb_storage::dbwr::DbwrGroup>>,
    errors: Arc<Mutex<BTreeMap<([u8; 8], &'static str), String>>>,
    checkpoints: Arc<Mutex<BTreeMap<[u8; 8], CheckpointState>>>,
}
impl Background {
    /// Start once after PUBLIC recovery; registration never starts new threads.
    pub fn start(
        pool: &'static bicdb_storage::buffer::BufferPool<'static>,
        router: Option<Arc<bicdb_storage::wal_router::WorkspaceWalRouter>>,
        lgwr_threads: usize,
        checkpoint_threads: usize,
        undo_threads: usize,
        undo_tick: Duration,
        log_tick: Duration,
        checkpoint_tick: Duration,
        dbwr_tick: Duration,
    ) -> std::io::Result<Self> {
        if lgwr_threads > 64
            || checkpoint_threads == 0
            || checkpoint_threads > 64
            || undo_threads == 0
            || undo_threads > 64
            || undo_tick.is_zero()
            || lgwr_threads == 0
            || log_tick.is_zero()
            || checkpoint_tick.is_zero()
            || dbwr_tick.is_zero()
        {
            return Err(std::io::Error::other(
                "background counts/intervals must be positive",
            ));
        }
        let mut this = Self {
            worker_counts: (lgwr_threads, checkpoint_threads, undo_threads),
            workspaces: Arc::new(RwLock::new(BTreeMap::new())),
            signal: Arc::new((Mutex::new(0), Condvar::new())),
            stop: Arc::new(AtomicBool::new(false)),
            handles: Vec::new(),
            dbwr: Some(Arc::new(bicdb_storage::dbwr::DbwrGroup::start_borrowed(
                pool, dbwr_tick,
            ))),
            errors: Arc::new(Mutex::new(BTreeMap::new())),
            checkpoints: Arc::new(Mutex::new(BTreeMap::new())),
        };
        let requests = Arc::new(Mutex::new(
            BTreeMap::<[u8; 8], bicdb_common::seq::Lsn>::new(),
        ));
        if let Some(router) = router {
            let requests = Arc::clone(&requests);
            let signal = Arc::clone(&this.signal);
            let stop = Arc::clone(&this.stop);
            router.set_flush_request_handler(Arc::new(move |ws, target| {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let mut targets = requests.lock().unwrap_or_else(|e| e.into_inner());
                if targets.get(&ws).is_some_and(|current| *current >= target) {
                    return;
                }
                targets.insert(ws, target);
                drop(targets);
                let mut generation = signal.0.lock().unwrap_or_else(|e| e.into_inner());
                *generation = generation.wrapping_add(1);
                signal.1.notify_all();
            }))?;
        }
        for worker in 0..(lgwr_threads + checkpoint_threads + undo_threads) {
            let checkpoint = worker >= lgwr_threads && worker < lgwr_threads + checkpoint_threads;
            let undo = worker >= lgwr_threads + checkpoint_threads;
            let (role, tick, shard, count) = if undo {
                (
                    "undo",
                    undo_tick,
                    worker - lgwr_threads - checkpoint_threads,
                    undo_threads,
                )
            } else if checkpoint {
                (
                    "ckpt",
                    checkpoint_tick,
                    worker - lgwr_threads,
                    checkpoint_threads,
                )
            } else {
                ("lgwr", log_tick, worker, lgwr_threads)
            };
            let workspaces = Arc::clone(&this.workspaces);
            let signal = Arc::clone(&this.signal);
            let stop = Arc::clone(&this.stop);
            let errors = Arc::clone(&this.errors);
            let checkpoints = Arc::clone(&this.checkpoints);
            let requests = Arc::clone(&requests);
            let writers = Arc::clone(this.dbwr.as_ref().expect("instance DBWR"));
            let handle = std::thread::Builder::new()
                .name(format!("bicdb-{role}-{worker}"))
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        let generation = *signal.0.lock().unwrap_or_else(|e| e.into_inner());
                        let snapshot: Vec<_> = workspaces
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .iter()
                            .map(|(ws, (engine, wal))| (*ws, *engine, Arc::clone(wal)))
                            .collect();
                        for (index, (ws, engine, wal)) in snapshot.into_iter().enumerate() {
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                            if pool.workspace_fault(ws).is_some() { continue; }
                            if index % count != shard {
                                continue;
                            }
                            let result = if undo {
                                match engine.repair_undo_slots(ws) {
                                    Ok(Some(true)) => { writers.wake(); Ok(()) },
                                    Ok(_) => Ok(()),
                                    Err(error) => Err(error.to_string()),
                                }
                            } else if checkpoint {
                                let outcome = engine.checkpoint_incremental(ws);
                                let mut states = checkpoints.lock().unwrap_or_else(|e| e.into_inner());
                                let state = states.entry(ws).or_default();
                                state.attempts += 1;
                                match outcome {
                                    Ok(Some(report)) => { state.progress = Some(report.progress); Ok(()) }
                                    Ok(None) => {
                                        state.deferred += 1;
                                        drop(states);
                                        if pool.dirty_len(ws) != 0 { writers.wake(); }
                                        Ok(())
                                    }
                                    Err(error) => Err(error.to_string()),
                                }
                            } else {
                                let target = requests.lock().unwrap_or_else(|e| e.into_inner())
                                    .get(&ws).copied().unwrap_or_else(|| wal.appended_lsn())
                                    .max(wal.appended_lsn());
                                let before = bicdb_storage::buffer::WalGuard::durable_lsn(&*wal);
                                match wal.flush_to(target) {
                                    Ok(_) => {
                                        let durable = bicdb_storage::buffer::WalGuard::durable_lsn(&*wal);
                                        if durable < target {
                                            Err(format!("LGWR 未达到工作区目标 LSN: {durable:?} < {target:?}"))
                                        } else {
                                            let mut targets = requests.lock().unwrap_or_else(|e| e.into_inner());
                                            if targets.get(&ws).is_some_and(|target| *target <= durable) {
                                                targets.remove(&ws);
                                            }
                                            drop(targets);
                                            if durable > before { writers.wake(); }
                                            Ok(())
                                        }
                                    }
                                    Err(error) => Err(error.to_string()),
                                }
                            };
                            let mut state = errors.lock().unwrap_or_else(|e| e.into_inner());
                            match result {
                                Ok(()) => {
                                    state.remove(&(ws, role));
                                }
                                Err(error) => {
                                    pool.quarantine_workspace(ws, format!("{role}: {error}"));
                                    state.insert((ws, role), error);
                                }
                            }
                        }
                        park_worker(&signal, &stop, tick, generation, checkpoint || undo);
                    }
                })?;
            this.handles.push(handle);
        }
        Ok(this)
    }
    /// Publish a recovered workspace to the existing background scheduler.
    pub fn register(&self, ws: [u8; 8], engine: &'static Engine) {
        self.workspaces
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ws, (engine, engine.wal_shared()));
        let mut generation = self.signal.0.lock().unwrap_or_else(|e| e.into_inner());
        *generation = generation.wrapping_add(1);
        self.signal.1.notify_all();
    }
    /// Background failure count, visible through STATUS rather than silently ignored.
    pub fn failures(&self) -> usize {
        self.errors.lock().unwrap_or_else(|e| e.into_inner()).len()
            + self.dbwr.as_ref().map_or(0, |writers| {
                usize::from(writers.stats().last_error.is_some())
            })
    }
    /// Instance-wide DBWR statistics, including temporary skip reasons.
    pub fn dbwr_stats(&self) -> bicdb_storage::dbwr::DbwrStats {
        self.dbwr
            .as_ref()
            .map_or_else(Default::default, |writers| writers.stats())
    }
    /// Actual fixed instance-wide thread limits (LGWR, CKPT, undo).
    pub fn worker_counts(&self) -> (usize, usize, usize) {
        self.worker_counts
    }

    /// Writer count follows the shared cache's working-set partition count.
    pub fn dbwr_writers(&self) -> usize {
        self.dbwr.as_ref().map_or(0, |writers| writers.writers())
    }
    /// Selected workspace state, without acquiring its SQL/undo/WAL locks.
    pub fn checkpoint_state(&self, ws: [u8; 8]) -> CheckpointState {
        self.checkpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ws)
            .copied()
            .unwrap_or_default()
    }
}
impl Drop for Background {
    fn drop(&mut self) {
        // Synchronize predicate with waiters; a notification cannot be lost.
        {
            let _guard = self.signal.0.lock().unwrap_or_else(|e| e.into_inner());
            self.stop.store(true, Ordering::Release);
            self.signal.1.notify_all();
        }
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
        if let Some(writers) = self.dbwr.take() {
            drop(writers); // last Arc drops and joins each partition writer
        }
    }
}

// Periodic maintenance is not awakened by unrelated LGWR notifications. The
// predicate is checked under the notification mutex, preventing lost wakeups.
fn park_worker(
    signal: &(Mutex<u64>, Condvar),
    stop: &AtomicBool,
    tick: Duration,
    generation: u64,
    timer_only: bool,
) {
    let guard = signal.0.lock().unwrap_or_else(|e| e.into_inner());
    let _ = signal.1.wait_timeout_while(guard, tick, |current| {
        !stop.load(Ordering::Acquire) && (timer_only || *current == generation)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maintenance_sleeps_through_log_notifications_and_shutdown_wakes_it() {
        let signal = Arc::new((Mutex::new(0), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = {
            let signal = Arc::clone(&signal);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                park_worker(&signal, &stop, Duration::from_secs(5), 0, true);
                tx.send(()).unwrap();
            })
        };
        {
            let mut generation = signal.0.lock().unwrap();
            *generation += 1;
            signal.1.notify_all();
        }
        assert!(rx.recv_timeout(Duration::from_millis(25)).is_err());
        {
            let _guard = signal.0.lock().unwrap();
            stop.store(true, Ordering::Release);
            signal.1.notify_all();
        }
        rx.recv_timeout(Duration::from_secs(1))
            .expect("shutdown must wake parked maintenance");
        handle.join().unwrap();
    }
    #[test]
    fn log_writer_observes_a_notification_arriving_before_park() {
        let signal = (Mutex::new(1), Condvar::new());
        let stop = AtomicBool::new(false);
        let start = std::time::Instant::now();
        park_worker(&signal, &stop, Duration::from_secs(5), 0, false);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
