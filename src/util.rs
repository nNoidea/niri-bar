use std::sync::{Mutex, MutexGuard};

/// Lock a `Mutex`, recovering from poisoning instead of panicking.
///
/// A poisoned mutex means another thread panicked while holding the lock.
/// For a status bar it is better to log and continue with the recovered
/// guard than to take down the whole UI thread.
///
/// `&Arc<Mutex<T>>` coerces to `&Mutex<T>` via deref, so both plain and
/// shared locks work.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            crate::logger::emit("WARN", "sync", "mutex was poisoned; recovering and continuing");
            poisoned.into_inner()
        }
    }
}

/// Nudge the main-thread updater.
///
/// The update channel is unbounded and coalesced by the receiver (`while
/// try_recv().is_ok() {}`), so a failed `try_send` only means the receiver
/// is closed during shutdown — intentionally silent, never a lost update.
pub fn nudge(tx: &async_channel::Sender<()>) {
    let _ = tx.try_send(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_lock_recovers_from_poison() {
        let m = Arc::new(Mutex::new(41u32));
        let m2 = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let mut g = m2.lock().unwrap();
            *g = 42;
            panic!("intentional poison");
        })
        .join();
        assert!(m.is_poisoned());
        let g = lock(&m);
        assert_eq!(*g, 42);
    }

    #[test]
    fn test_nudge_delivers_and_tolerates_close() {
        let (tx, rx) = async_channel::unbounded::<()>();
        nudge(&tx);
        assert!(rx.try_recv().is_ok());
        // Closed receiver: nudge must stay silent, never panic.
        rx.close();
        nudge(&tx);
    }
}
