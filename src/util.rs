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
type GtkTestJob = (
    Box<dyn FnOnce() + Send + 'static>,
    std::sync::mpsc::Sender<std::thread::Result<()>>,
);

/// Execute a test closure on a dedicated, persistent GTK main thread.
///
/// Rust's test harness spawns a fresh OS thread per `#[test]`, whereas GTK3
/// permanently binds its main thread to whichever OS thread called `gtk::init()`
/// first. Running all GTK widget tests on this single persistent thread ensures
/// every GTK test actually executes (zero skipped tests) and is safe under
/// parallel `cargo test`.
#[cfg(test)]
pub fn run_gtk_test<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    use std::sync::{mpsc, OnceLock};

    static GTK_RUNNER: OnceLock<mpsc::Sender<GtkTestJob>> = OnceLock::new();

    let runner_tx = GTK_RUNNER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<GtkTestJob>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), String>>();

        std::thread::Builder::new()
            .name("gtk-test-main".to_string())
            .spawn(move || {
                if let Err(err) = gtk::init() {
                    let _ = init_tx.send(Err(format!("Failed to initialize GTK for tests: {err}")));
                    return;
                }
                if !gtk::is_initialized_main_thread() {
                    let _ = init_tx.send(Err("GTK initialized on another thread before gtk-test-main".to_string()));
                    return;
                }
                let _ = init_tx.send(Ok(()));

                while let Ok((job, reply_tx)) = rx.recv() {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    while gtk::events_pending() {
                        gtk::main_iteration_do(false);
                    }
                    let _ = reply_tx.send(result);
                }
            })
            .expect("Failed to spawn gtk-test-main thread");

        if let Err(err) = init_rx.recv().expect("gtk-test-main exited before reporting init") {
            panic!("{err}");
        }

        tx
    });

    let (reply_tx, reply_rx) = mpsc::channel();
    runner_tx
        .send((Box::new(f), reply_tx))
        .expect("gtk-test-main thread terminated unexpectedly");

    if let Err(panic_payload) = reply_rx.recv().expect("gtk-test-main dropped reply channel") {
        std::panic::resume_unwind(panic_payload);
    }
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

    #[test]
    fn test_run_gtk_test_executes_across_threads() {
        let ran_first = Arc::new(Mutex::new(false));
        let ran_first_clone = Arc::clone(&ran_first);
        run_gtk_test(move || {
            assert!(gtk::is_initialized_main_thread());
            *lock(&ran_first_clone) = true;
        });
        assert!(*lock(&ran_first));

        let ran_second = Arc::new(Mutex::new(false));
        let ran_second_clone = Arc::clone(&ran_second);
        std::thread::spawn(move || {
            run_gtk_test(move || {
                assert!(gtk::is_initialized_main_thread());
                *lock(&ran_second_clone) = true;
            });
        })
        .join()
        .unwrap();
        assert!(*lock(&ran_second));
    }
}
