//! Per-thread, one-shot I/O coordination, faults and cleanup-lock notifications.
//! Timeouts only bound deadlocks; elapsed time never establishes contention.

use std::{
    cell::RefCell,
    sync::{LockResult, RwLock, RwLockWriteGuard, TryLockError, mpsc},
    time::Duration,
};

pub(super) const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Point {
    ColumnAppended,
    PrefixRead,
}

type Hook = (Point, Box<dyn FnOnce() -> crate::StorageResult<()>>);

thread_local! {
    static IO_HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
}

pub(super) fn with_hook<T>(
    point: Point,
    hook: impl FnOnce() -> crate::StorageResult<()> + 'static,
    operation: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<Hook>);
    impl Drop for Restore {
        fn drop(&mut self) {
            IO_HOOK.with(|slot| slot.replace(self.0.take()));
        }
    }
    let _restore = Restore(IO_HOOK.with(|slot| slot.replace(Some((point, Box::new(hook))))));
    operation()
}

pub(super) fn at(point: Point) -> crate::StorageResult<()> {
    let hook = IO_HOOK.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot
            .as_ref()
            .is_some_and(|(expected, _)| *expected == point)
        {
            slot.take()
        } else {
            None
        }
    });
    if let Some((_, hook)) = hook {
        hook()?;
    }
    Ok(())
}

thread_local! {
    static CONTENDED: RefCell<Option<mpsc::Sender<()>>> = const { RefCell::new(None) };
}

pub(super) fn signal_contention<T>(signal: mpsc::Sender<()>, operation: impl FnOnce() -> T) -> T {
    // Drop the notification on return/panic, so a missing lock acquisition
    // disconnects the receiver. Restore any enclosing hook as well.
    struct Restore(Option<mpsc::Sender<()>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CONTENDED.with(|slot| slot.replace(self.0.take()));
        }
    }
    let _restore = Restore(CONTENDED.with(|slot| slot.replace(Some(signal))));
    operation()
}

pub(super) fn write(lock: &RwLock<()>) -> LockResult<RwLockWriteGuard<'_, ()>> {
    match lock.try_write() {
        Ok(guard) => Ok(guard),
        Err(TryLockError::Poisoned(error)) => Err(error),
        Err(TryLockError::WouldBlock) => {
            // Notify only after exclusive acquisition actually encountered a
            // reader, then wait on the same lock as the production path.
            if let Some(signal) = CONTENDED.with(|slot| slot.borrow_mut().take()) {
                signal.send(()).unwrap();
            }
            lock.write()
        }
    }
}
