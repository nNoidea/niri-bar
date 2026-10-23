use std::future::Future;
use std::sync::OnceLock;
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::task::JoinHandle;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

#[allow(clippy::exit)]
fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        match Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .thread_keep_alive(std::time::Duration::from_millis(50))
            .thread_name("niri-bar-tokio")
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                log_error!("runtime", "Failed to initialize shared Tokio runtime: {e}");
                std::process::exit(1);
            }
        }
    })
}

/// Returns a handle to the shared process-wide Tokio runtime.
pub fn handle() -> &'static Handle {
    runtime().handle()
}

/// Spawns a future onto the shared Tokio runtime, even when called from outside
/// a Tokio context (such as the GTK3 main thread or synchronous constructors).
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    handle().spawn(future)
}

/// Number of OS threads in the current process.
#[cfg(test)]
pub(crate) fn thread_count() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("Threads:").and_then(|v| v.trim().parse().ok()))
        .unwrap_or(0)
}

/// Lowest OS-thread count observed across a short sampling window.
///
/// The test harness runs tests in parallel, so other tests can transiently
/// create threads while a thread-count assertion is in progress. Sampling the
/// minimum filters that noise out while still surfacing long-lived workers
/// spawned by the code under test.
#[cfg(test)]
pub(crate) fn min_thread_count() -> usize {
    (0..10)
        .map(|_| {
            let count = thread_count();
            std::thread::sleep(std::time::Duration::from_millis(5));
            count
        })
        .min()
        .unwrap_or(0)
}

/// Initializes process-wide threads that the test suite creates lazily exactly
/// once (the persistent GTK main-thread runner), so they are already part of
/// the baseline instead of being mistaken for threads spawned by the code under
/// test. Tests that assert on process-wide thread counts must call this before
/// measuring.
#[cfg(test)]
pub(crate) fn warm_up_shared_test_threads() {
    crate::util::run_gtk_test(|| {});
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_shared_runtime_spawns_from_non_tokio_thread() {
        let (tx, rx) = async_channel::bounded(1);
        let join = spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = tx.send(42).await;
            99
        });
        assert_eq!(rx.recv_blocking().unwrap(), 42);
        assert_eq!(handle().block_on(join).unwrap(), 99);
    }

    #[test]
    fn test_shared_runtime_is_singleton() {
        let h1 = handle() as *const Handle;
        let h2 = handle() as *const Handle;
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_single_tokio_worker_thread_count() {
        let _ = handle();
        let count = std::fs::read_dir("/proc/self/task")
            .unwrap()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path().join("comm")).ok())
            .filter(|name| name.trim() == "niri-bar-tokio")
            .count();
        assert_eq!(
            count, 1,
            "Expected exactly 1 permanent niri-bar-tokio worker thread, got {count}"
        );
    }
}
