//! Instance-wide SQL admission and fair workspace scheduling.
//!
//! There is one queueing authority per database instance. Workspaces do not
//! own private executors: they contribute requests to the shared ready set and
//! receive a bounded share of active workers and queued slots. Control-plane
//! requests (`HELLO`, `STATUS`, `AUTH`, `ROUTE`) run on the control loop
//! and do not enter this scheduler. Idle-workspace maintenance shares workers
//! while reserving exclusive execution only in its own workspace.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Immutable limits loaded from the instance parameter file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Shared SQL worker count for the instance.
    pub workers: usize,
    /// Waiting requests across all workspaces.
    pub instance_queue: usize,
    /// Active requests attributed to one workspace.
    pub active_per_workspace: usize,
    /// Waiting requests attributed to one workspace.
    pub queue_per_workspace: usize,
}

impl Limits {
    /// Validate non-zero scheduler limits.
    pub fn validate(self) -> Result<Self, &'static str> {
        if self.workers == 0 {
            return Err("worker count must be positive");
        }
        if self.instance_queue == 0 {
            return Err("instance queue capacity must be positive");
        }
        if self.active_per_workspace == 0 {
            return Err("workspace active limit must be positive");
        }
        if self.queue_per_workspace == 0 {
            return Err("workspace queue capacity must be positive");
        }
        Ok(self)
    }
}

/// Admission rejection. The caller can report whether the instance or only
/// the selected workspace is saturated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueError {
    /// All instance waiting slots are occupied.
    InstanceQueueFull,
    /// This workspace has consumed its waiting-slot allowance.
    WorkspaceQueueFull,
}

/// Observable counters exported by service status and fixed diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metrics {
    /// Requests currently executing.
    pub active: usize,
    /// Requests waiting in the instance scheduler.
    pub queued: usize,
    /// Requests parked on row locks, consuming waiting capacity, not workers.
    pub parked: usize,
    /// Workspaces with at least one waiting request.
    pub ready_workspaces: usize,
}

/// A reserved waiting slot. It can only be resumed by its owning scheduler.
#[derive(Debug)]
pub struct WaitSlot<W> {
    owner: u64,
    id: u64,
    workspace: W,
}

/// Suspending an active request must preserve both worker and waiting bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendError {
    /// No active request is attributed to this workspace.
    NotActive,
    /// No waiting admission capacity remains. The caller still owns its worker.
    Full(EnqueueError),
}

/// A request selected for a worker.
#[derive(Debug, PartialEq, Eq)]
pub struct Dispatch<W, T> {
    /// Immutable workspace attribution used for completion accounting.
    pub workspace: W,
    /// Caller-owned request payload.
    pub request: T,
}

/// Fair instance scheduler with instance-wide and per-workspace bounds.
///
/// Round-robin selection prevents a busy workspace from monopolizing worker
/// assignment. Active accounting is separate from waiting accounting, so a
/// workspace at its active limit keeps its queue position without consuming a
/// worker that another workspace can use.
pub struct Scheduler<W, T>
where
    W: Copy + Ord,
{
    limits: Limits,
    queues: BTreeMap<W, VecDeque<(T, bool)>>,
    ready: VecDeque<W>,
    active_by_workspace: BTreeMap<W, usize>,
    exclusive: BTreeSet<W>,
    active: usize,
    queued: usize,
    end_queued: usize,
    parked_by_workspace: BTreeMap<W, usize>,
    parked_slots: BTreeMap<u64, W>,
    owner: u64,
    next_slot: u64,
}

impl<W, T> Scheduler<W, T>
where
    W: Copy + Ord,
{
    /// Construct an empty scheduler.
    ///
    /// # Errors
    /// Any zero limit is rejected.
    pub fn new(limits: Limits) -> Result<Self, &'static str> {
        static NEXT_OWNER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Ok(Self {
            owner: NEXT_OWNER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            next_slot: 1,
            parked_by_workspace: BTreeMap::new(),
            parked_slots: BTreeMap::new(),
            limits: limits.validate()?,
            queues: BTreeMap::new(),
            ready: VecDeque::new(),
            active_by_workspace: BTreeMap::new(),
            exclusive: BTreeSet::new(),
            active: 0,
            queued: 0,
            end_queued: 0,
        })
    }

    /// Admit one SQL request to the shared instance queue.
    pub fn enqueue(&mut self, workspace: W, request: T) -> Result<(), (EnqueueError, T)> {
        if self.queued - self.end_queued + self.parked_slots.len() >= self.limits.instance_queue {
            return Err((EnqueueError::InstanceQueueFull, request));
        }
        if self.normal_queued_in(workspace) + self.parked_in(workspace)
            >= self.limits.queue_per_workspace
        {
            return Err((EnqueueError::WorkspaceQueueFull, request));
        }
        self.push_reserved(workspace, request);
        Ok(())
    }

    fn push_reserved(&mut self, workspace: W, request: T) {
        let queue = self.queues.entry(workspace).or_default();
        if queue.is_empty() {
            self.ready.push_back(workspace);
        }
        queue.push_back((request, false));
        self.queued += 1;
    }

    /// Reserved admission for a validated sole COMMIT/ROLLBACK on an active
    /// transaction. The service bounds it by the configured connection limit.
    pub fn enqueue_transaction_end(
        &mut self,
        workspace: W,
        request: T,
        capacity: usize,
    ) -> Result<(), (EnqueueError, T)> {
        if self.end_queued >= capacity {
            return Err((EnqueueError::InstanceQueueFull, request));
        }
        self.queues
            .entry(workspace)
            .or_default()
            .push_front((request, true));
        self.ready.retain(|entry| *entry != workspace);
        self.ready.push_front(workspace);
        self.queued += 1;
        self.end_queued += 1;
        Ok(())
    }

    fn normal_queued_in(&self, workspace: W) -> usize {
        self.queues
            .get(&workspace)
            .map_or(0, |queue| queue.iter().filter(|(_, end)| !end).count())
    }

    /// Convert one active worker slot into bounded lock-wait capacity.
    /// Failure leaves active accounting unchanged so cancellation can run there.
    pub fn suspend(&mut self, workspace: W) -> Result<WaitSlot<W>, SuspendError> {
        if self.active_in(workspace) == 0 {
            return Err(SuspendError::NotActive);
        }
        if self.queued - self.end_queued + self.parked_slots.len() >= self.limits.instance_queue {
            return Err(SuspendError::Full(EnqueueError::InstanceQueueFull));
        }
        if self.normal_queued_in(workspace) + self.parked_in(workspace)
            >= self.limits.queue_per_workspace
        {
            return Err(SuspendError::Full(EnqueueError::WorkspaceQueueFull));
        }
        let id = self.next_slot;
        self.next_slot = self
            .next_slot
            .checked_add(1)
            .expect("waiting slot IDs exhausted");
        assert!(self.complete(workspace));
        self.parked_slots.insert(id, workspace);
        *self.parked_by_workspace.entry(workspace).or_default() += 1;
        Ok(WaitSlot {
            owner: self.owner,
            id,
            workspace,
        })
    }

    /// Resume (including cancellation cleanup) using its already reserved credit.
    /// New arrivals cannot consume the credit and strand an admitted transaction.
    pub fn resume(&mut self, slot: WaitSlot<W>, request: T) -> Result<(), (WaitSlot<W>, T)> {
        if slot.owner != self.owner || self.parked_slots.get(&slot.id) != Some(&slot.workspace) {
            return Err((slot, request));
        }
        self.parked_slots.remove(&slot.id);
        let count = self
            .parked_by_workspace
            .get_mut(&slot.workspace)
            .expect("waiting attribution");
        *count -= 1;
        if *count == 0 {
            self.parked_by_workspace.remove(&slot.workspace);
        }
        self.push_reserved(slot.workspace, request);
        Ok(())
    }

    /// Parked row waits attributed to one workspace.
    pub fn parked_in(&self, workspace: W) -> usize {
        self.parked_by_workspace
            .get(&workspace)
            .copied()
            .unwrap_or(0)
    }

    /// Select one eligible request for an available shared worker.
    pub fn dispatch(&mut self) -> Option<Dispatch<W, T>> {
        if self.active >= self.limits.workers {
            return None;
        }
        let candidates = self.ready.len();
        for _ in 0..candidates {
            let workspace = self.ready.pop_front()?;
            let workspace_active = self
                .active_by_workspace
                .get(&workspace)
                .copied()
                .unwrap_or(0);
            if self.exclusive.contains(&workspace)
                || workspace_active >= self.limits.active_per_workspace
            {
                self.ready.push_back(workspace);
                continue;
            }
            let queue = self.queues.get_mut(&workspace)?;
            let (request, end) = queue.pop_front()?;
            if end {
                self.end_queued -= 1;
            }
            self.queued -= 1;
            if queue.is_empty() {
                self.queues.remove(&workspace);
            } else {
                self.ready.push_back(workspace);
            }
            self.active += 1;
            *self.active_by_workspace.entry(workspace).or_default() += 1;
            return Some(Dispatch { workspace, request });
        }
        None
    }

    /// Reserve one shared worker for maintenance in an otherwise idle workspace.
    /// New SQL keeps normal queue admission but cannot dispatch in this workspace
    /// until completion. Queued or parked SQL is never overtaken by maintenance.
    pub fn reserve_exclusive(&mut self, workspace: W) -> bool {
        if self.active >= self.limits.workers || !self.idle_in(workspace) {
            return false;
        }
        self.exclusive.insert(workspace);
        self.active += 1;
        self.active_by_workspace.insert(workspace, 1);
        true
    }

    /// Active maintenance jobs, included in the shared active-worker count.
    pub fn exclusive_count(&self) -> usize {
        self.exclusive.len()
    }

    /// Release the active slot held by a completed worker request.
    /// Returns false for a duplicate or foreign completion.
    pub fn complete(&mut self, workspace: W) -> bool {
        let Some(count) = self.active_by_workspace.get_mut(&workspace) else {
            return false;
        };
        if *count == 0 || self.active == 0 {
            return false;
        }
        self.exclusive.remove(&workspace);
        *count -= 1;
        self.active -= 1;
        if *count == 0 {
            self.active_by_workspace.remove(&workspace);
        }
        true
    }

    /// Current instance counters.
    #[must_use]
    pub fn metrics(&self) -> Metrics {
        Metrics {
            active: self.active,
            queued: self.queued,
            parked: self.parked_slots.len(),
            ready_workspaces: self.ready.len(),
        }
    }

    /// Active requests for one workspace.
    #[must_use]
    pub fn active_in(&self, workspace: W) -> usize {
        self.active_by_workspace
            .get(&workspace)
            .copied()
            .unwrap_or(0)
    }

    /// Waiting requests for one workspace.
    #[must_use]
    pub fn queued_in(&self, workspace: W) -> usize {
        self.queues.get(&workspace).map_or(0, VecDeque::len)
    }

    /// Whether this workspace has no executing, queued, or row-lock-waiting
    /// request. Other workspaces deliberately do not affect this answer.
    #[must_use]
    pub fn idle_in(&self, workspace: W) -> bool {
        self.active_in(workspace) == 0
            && self.queued_in(workspace) == 0
            && self.parked_in(workspace) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            workers: 3,
            instance_queue: 6,
            active_per_workspace: 2,
            queue_per_workspace: 3,
        }
    }

    #[test]
    fn round_robin_prevents_one_workspace_from_monopolizing_dispatch() {
        let mut scheduler = Scheduler::new(limits()).unwrap();
        scheduler.enqueue(1, "a1").unwrap();
        scheduler.enqueue(1, "a2").unwrap();
        scheduler.enqueue(1, "a3").unwrap();
        scheduler.enqueue(2, "b1").unwrap();
        assert_eq!(scheduler.dispatch().unwrap().request, "a1");
        assert_eq!(scheduler.dispatch().unwrap().request, "b1");
        assert_eq!(scheduler.dispatch().unwrap().request, "a2");
        assert!(
            scheduler.dispatch().is_none(),
            "all instance workers are active"
        );
    }

    #[test]
    fn workspace_active_limit_leaves_workers_for_other_workspaces() {
        let mut scheduler = Scheduler::new(limits()).unwrap();
        for value in 1..=3 {
            scheduler.enqueue(1, value).unwrap();
        }
        scheduler.enqueue(2, 4).unwrap();
        assert_eq!(scheduler.dispatch().unwrap().workspace, 1);
        assert_eq!(scheduler.dispatch().unwrap().workspace, 2);
        assert_eq!(scheduler.dispatch().unwrap().workspace, 1);
        assert_eq!(scheduler.active_in(1), 2);
        assert_eq!(scheduler.queued_in(1), 1);
        assert!(scheduler.complete(2));
        assert!(
            scheduler.dispatch().is_none(),
            "workspace 1 remains at its limit"
        );
        assert!(scheduler.complete(1));
        assert_eq!(scheduler.dispatch().unwrap().request, 3);
    }

    #[test]
    fn workspace_idle_state_is_isolated_from_other_workspaces() {
        let mut scheduler = Scheduler::new(limits()).unwrap();
        scheduler.enqueue(2, "private").unwrap();
        assert!(scheduler.idle_in(1));
        assert!(!scheduler.idle_in(2));

        let dispatched = scheduler.dispatch().unwrap();
        assert!(scheduler.idle_in(1));
        assert!(!scheduler.idle_in(2));
        assert!(scheduler.complete(dispatched.workspace));
        assert!(scheduler.idle_in(2));
    }

    #[test]
    fn instance_and_workspace_queue_caps_are_independent() {
        let mut scheduler = Scheduler::new(limits()).unwrap();
        for value in 0..3 {
            scheduler.enqueue(1, value).unwrap();
        }
        assert!(matches!(
            scheduler.enqueue(1, 9),
            Err((EnqueueError::WorkspaceQueueFull, 9))
        ));
        for value in 3..6 {
            scheduler.enqueue(2, value).unwrap();
        }
        assert!(matches!(
            scheduler.enqueue(3, 9),
            Err((EnqueueError::InstanceQueueFull, 9))
        ));
        assert_eq!(
            scheduler.metrics(),
            Metrics {
                active: 0,
                queued: 6,
                parked: 0,
                ready_workspaces: 2,
            }
        );
    }

    #[test]
    fn duplicate_completion_cannot_corrupt_capacity() {
        let mut scheduler = Scheduler::new(limits()).unwrap();
        scheduler.enqueue(7, "work").unwrap();
        assert!(scheduler.dispatch().is_some());
        assert!(scheduler.complete(7));
        assert!(!scheduler.complete(7));
        assert_eq!(scheduler.metrics().active, 0);
    }
    #[test]
    fn parked_requests_release_workers_but_keep_bounded_waiting_credit() {
        let mut scheduler = Scheduler::new(Limits {
            workers: 1,
            instance_queue: 1,
            active_per_workspace: 1,
            queue_per_workspace: 1,
        })
        .unwrap();
        scheduler.enqueue(1, "waiter").unwrap();
        assert_eq!(scheduler.dispatch().unwrap().request, "waiter");
        let slot = scheduler.suspend(1).unwrap();
        assert_eq!(scheduler.metrics().active, 0);
        assert_eq!(scheduler.metrics().parked, 1);
        assert!(scheduler.enqueue(2, "new").is_err());
        // The lock owner can finish despite the full ordinary waiting capacity.
        scheduler.enqueue_transaction_end(1, "commit", 1).unwrap();
        assert_eq!(scheduler.dispatch().unwrap().request, "commit");
        assert!(scheduler.complete(1));
        scheduler.resume(slot, "retry").unwrap();
        assert_eq!(scheduler.metrics().parked, 0);
        assert_eq!(scheduler.dispatch().unwrap().request, "retry");
        assert!(scheduler.complete(1));
        assert_eq!(scheduler.metrics().active, 0);
    }

    #[test]
    fn failed_suspend_keeps_its_worker_until_cleanup_completes() {
        let mut scheduler = Scheduler::new(Limits {
            workers: 1,
            instance_queue: 1,
            active_per_workspace: 1,
            queue_per_workspace: 1,
        })
        .unwrap();
        scheduler.enqueue(1, "active").unwrap();
        scheduler.dispatch().unwrap();
        scheduler.enqueue(2, "queued").unwrap();
        assert!(matches!(scheduler.suspend(1), Err(SuspendError::Full(_))));
        assert_eq!(scheduler.metrics().active, 1);
        assert_eq!(scheduler.metrics().parked, 0);
        assert!(scheduler.dispatch().is_none());
        assert!(scheduler.complete(1));
        assert_eq!(scheduler.dispatch().unwrap().request, "queued");
    }

    #[test]
    fn foreign_resume_cannot_steal_or_release_waiting_credit() {
        let mut owner = Scheduler::new(limits()).unwrap();
        let mut foreign = Scheduler::new(limits()).unwrap();
        owner.enqueue(1, "work").unwrap();
        owner.dispatch().unwrap();
        let slot = owner.suspend(1).unwrap();
        let (slot, request) = foreign.resume(slot, "resume").unwrap_err();
        assert_eq!(owner.metrics().parked, 1);
        assert_eq!(foreign.metrics().queued, 0);
        owner.resume(slot, request).unwrap();
        assert_eq!(owner.dispatch().unwrap().request, "resume");
    }
}

#[cfg(test)]
mod maintenance_tests {
    use super::*;

    #[test]
    fn maintenance_excludes_only_its_workspace_and_preserves_queue_bounds() {
        let mut s = Scheduler::new(Limits {
            workers: 2,
            instance_queue: 2,
            active_per_workspace: 2,
            queue_per_workspace: 1,
        })
        .unwrap();
        assert!(s.reserve_exclusive(1));
        assert!(!s.reserve_exclusive(1));
        s.enqueue(1, "after maintenance").unwrap();
        assert_eq!(
            s.enqueue(1, "overflow"),
            Err((EnqueueError::WorkspaceQueueFull, "overflow"))
        );
        s.enqueue(2, "other workspace").unwrap();
        assert_eq!(s.dispatch().unwrap().request, "other workspace");
        assert!(s.dispatch().is_none());
        assert_eq!(s.metrics().active, 2);
        assert_eq!(s.exclusive_count(), 1);
        assert!(s.complete(1));
        assert_eq!(s.exclusive_count(), 0);
        assert_eq!(s.dispatch().unwrap().request, "after maintenance");
        assert!(s.complete(1));
        assert!(s.complete(2));
        assert_eq!(s.metrics().active, 0);
        assert_eq!(s.metrics().queued, 0);
    }

    #[test]
    fn maintenance_cannot_overtake_queued_or_parked_sql_or_exceed_workers() {
        let mut s = Scheduler::new(Limits {
            workers: 1,
            instance_queue: 2,
            active_per_workspace: 1,
            queue_per_workspace: 2,
        })
        .unwrap();
        s.enqueue(1, "row wait").unwrap();
        assert!(!s.reserve_exclusive(1));
        s.dispatch().unwrap();
        assert!(!s.reserve_exclusive(2));
        let token = s.suspend(1).unwrap();
        assert!(!s.reserve_exclusive(1));
        assert!(s.reserve_exclusive(2));
        assert!(s.complete(2));
        s.resume(token, "resumed").unwrap();
        assert!(!s.reserve_exclusive(1));
        s.dispatch().unwrap();
        assert!(s.complete(1));
        assert!(s.reserve_exclusive(1));
        assert!(s.complete(1));
        assert_eq!(s.metrics().active, 0);
    }
}
