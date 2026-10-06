//! 有界执行池（方案 §15：**起始执行线程 2–8，按工作区额度调整**；
//! `NFR` REQ-RES-004：**排队有界**，没有"谁快谁赢"的无界竞争）。
//!
//! - **线程数固定**：创建时确定，不随负载伸缩；
//! - **队列有界**：提交用 `try_send`，满即**拒绝**（不阻塞、不无界堆积）；
//! - **关闭是显式的**：[`ExecPool::shutdown`]（`Drop` 兜底）——先把队列里
//!   已有的任务跑完，再让线程退出。
//!
//! 网络 I/O 与维护线程**不占用**本池（方案 §15、REQ-RES-003）。

use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

/// 池内任务。
type Task = Box<dyn FnOnce() + Send + 'static>;

enum Msg {
    Task(Task),
    Shutdown,
}

/// 提交失败（资源拒绝的两类：满 / 停机）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolError {
    /// 队列已满——**有界**的体现（`NFR` REQ-RES-004）。
    QueueFull,
    /// 池已关闭（停机中）。
    Closed,
}

/// 有界执行池。
#[derive(Debug)]
pub struct ExecPool {
    tx: SyncSender<Msg>,
    workers: Vec<JoinHandle<()>>,
    capacity: usize,
    threads: usize,
}

impl ExecPool {
    /// 创建 `threads` 个工作线程与容量为 `queue_capacity` 的有界队列。
    ///
    /// `threads` 与 `queue_capacity` 都必须 ≥ 1（由
    /// [`InstanceLimits`](crate::limits::InstanceLimits) 校验）。
    #[must_use]
    pub fn new(threads: usize, queue_capacity: usize) -> Self {
        assert!(
            threads >= 1 && queue_capacity >= 1,
            "线程数与队列容量须 ≥ 1"
        );
        let (tx, rx) = sync_channel::<Msg>(queue_capacity);
        let rx = Arc::new(Mutex::new(rx));
        let workers = (0..threads)
            .map(|i| {
                let rx = Arc::clone(&rx);
                thread::Builder::new()
                    .name(format!("bicdb-exec-{i}"))
                    .spawn(move || worker_loop(&rx))
                    .expect("创建工作线程")
            })
            .collect();
        Self {
            tx,
            workers,
            capacity: queue_capacity,
            threads,
        }
    }

    /// 提交任务；**队列满即拒绝**（不阻塞）。
    pub fn submit(&self, task: impl FnOnce() + Send + 'static) -> Result<(), PoolError> {
        match self.tx.try_send(Msg::Task(Box::new(task))) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(PoolError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(PoolError::Closed),
        }
    }

    /// 工作线程数。
    #[must_use]
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// 队列容量。
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 优雅关闭：队列中的任务先跑完，再让工作线程退出。
    ///
    /// **在途任务不被中断**——这是刻意的（不得在事务中途丢弃线程）；
    /// 因此一个永不结束的任务会让关闭等待（P1 语义，强制终止归管理面）。
    pub fn shutdown(mut self) {
        self.shutdown_workers();
    }

    fn shutdown_workers(&mut self) {
        for _ in &self.workers {
            // 队列已满时也须让关闭消息进入：用阻塞发送（关闭路径独占）。
            let _ = self.tx.send(Msg::Shutdown);
        }
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for ExecPool {
    fn drop(&mut self) {
        if !self.workers.is_empty() {
            self.shutdown_workers();
        }
    }
}

fn worker_loop(rx: &Mutex<Receiver<Msg>>) {
    loop {
        let msg = {
            let guard = rx.lock().unwrap_or_else(|e| e.into_inner());
            guard.recv()
        };
        match msg {
            Ok(Msg::Task(task)) => task(),
            // 通道断开或收到关闭消息：退出。
            Ok(Msg::Shutdown) | Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn queue_is_bounded_and_rejects_instead_of_blocking() {
        // 单线程 + 容量 1：占住线程后再塞 1 个排队，第 3 个必须被拒绝。
        let pool = ExecPool::new(1, 1);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let holder = Arc::clone(&release_rx);
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();

        pool.submit(move || {
            started_tx.send(()).expect("通知已开始");
            let _ = holder.lock().unwrap_or_else(|e| e.into_inner()).recv();
        })
        .unwrap();
        // 等阻塞任务真正开始执行——此后队列才是空的。
        started_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("阻塞任务应开始执行");

        let ran = Arc::new(AtomicUsize::new(0));
        let r = Arc::clone(&ran);
        pool.submit(move || {
            r.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap_or_else(|_| panic!("队列应有 1 个空位"));

        // 现在：1 个在跑、1 个排队 → 再提交必满。
        assert_eq!(pool.submit(|| {}), Err(PoolError::QueueFull));

        release_tx.send(()).unwrap();
        pool.shutdown();
        assert_eq!(ran.load(Ordering::SeqCst), 1, "排队的任务在关闭前已跑完");
    }

    #[test]
    fn tasks_run_on_the_configured_number_of_threads() {
        let pool = ExecPool::new(2, 8);
        assert_eq!(pool.threads(), 2);
        assert_eq!(pool.capacity(), 8);

        let (done_tx, done_rx) = std::sync::mpsc::channel::<usize>();
        for i in 0..6usize {
            let done_tx = done_tx.clone();
            pool.submit(move || {
                done_tx.send(i).unwrap();
            })
            .unwrap();
        }
        drop(done_tx);
        let got: Vec<usize> = done_rx.iter().collect();
        assert_eq!(got.len(), 6);
        pool.shutdown();
    }
}
