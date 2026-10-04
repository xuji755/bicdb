//! 维护线程：**自己的一条专用线程**，与执行池分离（`NFR` REQ-RES-003：
//! "日志刷盘、回滚、死锁检测、资产清理与关键恢复任务保留专用资源，
//! **不得被普通请求耗尽**"；方案 §15："维护线程与提交进展**不被用户查询饿死**"）。
//!
//! P1 的形态就是这条保证本身：维护任务**不经过执行池**，因此池打满也不影响它。
//! 未来落在这条线程上的周期任务（死锁检测、检查点、资产清理）在各自阶段接入。
//!
//! [`Maintenance::completed`] 暴露已完成任务数——测试用它观测"池打满时维护照常"。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::pool::PoolError;

type Job = Box<dyn FnOnce() + Send + 'static>;

enum Msg {
    Job(Job),
    Shutdown,
}

/// 维护线程（专用）。
#[derive(Debug)]
pub struct Maintenance {
    tx: SyncSender<Msg>,
    handle: Option<JoinHandle<()>>,
    completed: Arc<AtomicU64>,
    capacity: usize,
}

impl Maintenance {
    /// 启动一条专用维护线程，队列容量 `queue_capacity`（≥ 1）。
    #[must_use]
    pub fn start(queue_capacity: usize) -> Self {
        assert!(queue_capacity >= 1, "维护队列容量须 ≥ 1");
        let (tx, rx) = sync_channel::<Msg>(queue_capacity);
        let completed = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&completed);
        let handle = thread::Builder::new()
            .name("bicdb-maintenance".to_owned())
            .spawn(move || {
                // 收到 Job 执行；收到 Shutdown 或通道断开即退出。
                while let Ok(Msg::Job(job)) = rx.recv() {
                    job();
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            })
            .expect("创建维护线程");
        Self {
            tx,
            handle: Some(handle),
            completed,
            capacity: queue_capacity,
        }
    }

    /// 投递一个维护任务；队列满即拒绝（维护队列**同样有界**）。
    pub fn schedule(&self, job: impl FnOnce() + Send + 'static) -> Result<(), PoolError> {
        match self.tx.try_send(Msg::Job(Box::new(job))) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(PoolError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(PoolError::Closed),
        }
    }

    /// 已完成的维护任务数（测试/诊断观测用）。
    #[must_use]
    pub fn completed(&self) -> u64 {
        self.completed.load(Ordering::SeqCst)
    }

    /// 队列容量。
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 优雅关闭：跑完队列中已有的任务后退出线程。
    pub fn shutdown(mut self) {
        self.shutdown_worker();
    }

    fn shutdown_worker(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.tx.send(Msg::Shutdown);
            let _ = handle.join();
        }
    }
}

impl Drop for Maintenance {
    fn drop(&mut self) {
        self.shutdown_worker();
    }
}
