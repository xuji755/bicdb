//! Route shared-cache durability checks to independent workspace WAL streams.
use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, RwLock};

use crate::buffer::WalGuard;
use bicdb_common::seq::Lsn;

type FlushRequestHandler = Arc<dyn Fn([u8; 8], Lsn) + Send + Sync>;

/// Registry of independent workspace WAL durability guards.
#[derive(Default)]
pub struct WorkspaceWalRouter {
    streams: RwLock<BTreeMap<[u8; 8], Arc<dyn WalGuard>>>,
    flush_request: RwLock<Option<FlushRequestHandler>>,
}

impl WorkspaceWalRouter {
    /// Install the instance LGWR notification hook, without replacing a live one.
    /// The callback must only enqueue/signal; it must never do log I/O.
    pub fn set_flush_request_handler(&self, handler: FlushRequestHandler) -> io::Result<()> {
        let mut current = self
            .flush_request
            .write()
            .map_err(|_| io::Error::other("WAL request hook poisoned"))?;
        if current.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "LGWR request hook already installed",
            ));
        }
        *current = Some(handler);
        Ok(())
    }
    /// Never replace a live stream: dirty pages may still reference its LSNs.
    pub fn register(&self, workspace: [u8; 8], guard: Arc<dyn WalGuard>) -> io::Result<()> {
        let mut streams = self
            .streams
            .write()
            .map_err(|_| io::Error::other("WAL registry poisoned"))?;
        if streams.contains_key(&workspace) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "workspace WAL already registered",
            ));
        }
        streams.insert(workspace, guard);
        Ok(())
    }

    /// Undo a just-failed workspace registration before pages can reference
    /// the stream. Active workspace removal is intentionally unsupported.
    pub fn unregister_registration(&self, workspace: [u8; 8]) -> io::Result<()> {
        let mut streams = self
            .streams
            .write()
            .map_err(|_| io::Error::other("WAL registry poisoned"))?;
        streams
            .remove(&workspace)
            .map(|_| ())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "workspace WAL not registered"))
    }

    fn stream(&self, workspace: [u8; 8]) -> io::Result<Arc<dyn WalGuard>> {
        self.streams
            .read()
            .map_err(|_| io::Error::other("WAL registry poisoned"))?
            .get(&workspace)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "workspace WAL not registered"))
    }
}

impl WalGuard for WorkspaceWalRouter {
    // No global LSN exists. The legacy value is conservative; unscoped writes
    // are refused. BufferPool always uses the scoped methods.
    fn durable_lsn(&self) -> Lsn {
        Lsn::from_raw(0).unwrap()
    }
    fn ensure_durable(&self, _: Lsn) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace required for WAL durability",
        ))
    }
    fn durable_lsn_for(&self, workspace: [u8; 8]) -> io::Result<Lsn> {
        self.stream(workspace)?.durable_lsn_for(workspace)
    }
    fn ensure_durable_for(&self, workspace: [u8; 8], target: Lsn) -> io::Result<()> {
        // The registry lock has already been released before disk I/O.
        self.stream(workspace)?
            .ensure_durable_for(workspace, target)
    }
    fn request_durable_for(&self, workspace: [u8; 8], target: Lsn) -> io::Result<()> {
        let stream = self.stream(workspace)?;
        let handler = self
            .flush_request
            .read()
            .map_err(|_| io::Error::other("WAL request hook poisoned"))?
            .clone();
        if let Some(handler) = handler {
            handler(workspace, target);
            Ok(())
        } else {
            stream.request_durable_for(workspace, target)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Stream(AtomicU64);
    impl WalGuard for Stream {
        fn durable_lsn(&self) -> Lsn {
            Lsn::from_raw(self.0.load(Ordering::SeqCst)).unwrap()
        }
        fn ensure_durable(&self, target: Lsn) -> io::Result<()> {
            self.0.fetch_max(target.as_raw(), Ordering::SeqCst);
            Ok(())
        }
    }
    #[test]
    fn flush_request_signals_its_workspace_without_doing_io() {
        let router = WorkspaceWalRouter::default();
        router
            .register([1; 8], Arc::new(Stream(AtomicU64::new(0))))
            .unwrap();
        router
            .register([2; 8], Arc::new(Stream(AtomicU64::new(0))))
            .unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        router
            .set_flush_request_handler(Arc::new(move |ws, target| {
                send.send((ws, target)).unwrap();
            }))
            .unwrap();
        router
            .request_durable_for([2; 8], Lsn::from_raw(100).unwrap())
            .unwrap();
        assert_eq!(
            receive.try_recv().unwrap(),
            ([2; 8], Lsn::from_raw(100).unwrap())
        );
        assert_eq!(router.durable_lsn_for([1; 8]).unwrap().as_raw(), 0);
        assert_eq!(router.durable_lsn_for([2; 8]).unwrap().as_raw(), 0);
        assert!(router
            .request_durable_for([3; 8], Lsn::from_raw(1).unwrap())
            .is_err());
        assert!(receive.try_recv().is_err());
        assert!(router
            .set_flush_request_handler(Arc::new(|_, _| {}))
            .is_err());
    }

    #[test]
    fn equal_lsns_in_different_workspaces_do_not_share_durability() {
        let router = Arc::new(WorkspaceWalRouter::default());
        router
            .register([1; 8], Arc::new(Stream(AtomicU64::new(0))))
            .unwrap();
        router
            .register([2; 8], Arc::new(Stream(AtomicU64::new(0))))
            .unwrap();
        router
            .ensure_durable_for([1; 8], Lsn::from_raw(100).unwrap())
            .unwrap();
        assert_eq!(router.durable_lsn_for([1; 8]).unwrap().as_raw(), 100);
        assert_eq!(router.durable_lsn_for([2; 8]).unwrap().as_raw(), 0);
        assert!(router
            .ensure_durable_for([3; 8], Lsn::from_raw(1).unwrap())
            .is_err());
        assert!(router.ensure_durable(Lsn::from_raw(1).unwrap()).is_err());
        assert!(router
            .register([1; 8], Arc::new(Stream(AtomicU64::new(0))))
            .is_err());
    }

    #[test]
    fn failed_registration_can_be_removed_before_use() {
        let router = WorkspaceWalRouter::default();
        router
            .register([1; 8], Arc::new(Stream(AtomicU64::new(0))))
            .unwrap();
        router.unregister_registration([1; 8]).unwrap();
        assert!(router.durable_lsn_for([1; 8]).is_err());
        router
            .register([1; 8], Arc::new(Stream(AtomicU64::new(7))))
            .unwrap();
        assert_eq!(router.durable_lsn_for([1; 8]).unwrap().as_raw(), 7);
    }
}
