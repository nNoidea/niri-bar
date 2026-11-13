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

/// Default GTK4 to its lightweight Cairo software renderer (`GSK_RENDERER=cairo`)
/// and disable the AT-SPI accessibility D-Bus tree (`GTK_A11Y=none`) unless the
/// user explicitly set those variables in their environment.
///
/// Must be called before `gtk::init()` so GTK4 does not initialize a Vulkan/GL
/// context, shader cache, or AT-SPI D-Bus bridge, keeping resident memory minimal.
pub fn ensure_cairo_renderer() {
    if std::env::var_os("GSK_RENDERER").is_none() {
        std::env::set_var("GSK_RENDERER", "cairo");
    }
    if std::env::var_os("GTK_A11Y").is_none() {
        std::env::set_var("GTK_A11Y", "none");
    }
}

/// Ensure glibc's allocator (`MALLOC_ARENA_MAX=2`, `MALLOC_TRIM_THRESHOLD_=65536`)
/// and GTK4 environment (`GSK_RENDERER=cairo`, `GTK_A11Y=none`) are active from
/// process startup (`__libc_start_main`) by performing a one-shot self-exec if
/// `NIRI_BAR_ENV_TUNED` is not yet set.
pub fn ensure_optimal_process_env(argv: &[String]) {
    ensure_cairo_renderer();
    if std::env::var_os("NIRI_BAR_ENV_TUNED").is_none() && std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        std::env::set_var("MALLOC_ARENA_MAX", "2");
        if std::env::var_os("MALLOC_TRIM_THRESHOLD_").is_none() {
            std::env::set_var("MALLOC_TRIM_THRESHOLD_", "65536");
        }
        std::env::set_var("NIRI_BAR_ENV_TUNED", "1");
        if let Ok(exe) = std::env::current_exe() {
            use std::os::unix::process::CommandExt;
            let args = if argv.len() > 1 { &argv[1..] } else { &[] };
            let _ = std::process::Command::new(exe).args(args).exec();
        }
    }
}

/// Collect the direct child widgets of `parent` in order (`first_child` .. `next_sibling`).
pub fn widget_children(parent: &impl gtk::prelude::WidgetExt) -> Vec<gtk::Widget> {
    use gtk::prelude::*;
    let mut out = Vec::new();
    let mut child_opt = parent.first_child();
    while let Some(c) = child_opt {
        child_opt = c.next_sibling();
        out.push(c);
    }
    out
}

/// Move `child` to zero-based index `target_idx` inside `container`.
pub fn reorder_box_child(container: &gtk::Box, child: &impl gtk::prelude::IsA<gtk::Widget>, target_idx: usize) {
    use gtk::prelude::*;
    let child_ref: &gtk::Widget = child.as_ref();
    let mut prev_other: Option<gtk::Widget> = None;
    let mut last_other: Option<gtk::Widget> = None;
    let mut other_idx = 0usize;
    let mut total_idx = 0usize;

    let mut cur = container.first_child();
    while let Some(c) = cur {
        cur = c.next_sibling();
        if &c == child_ref {
            if total_idx == target_idx {
                return;
            }
        } else {
            if other_idx + 1 == target_idx {
                prev_other = Some(c.clone());
            }
            last_other = Some(c);
            other_idx += 1;
        }
        total_idx += 1;
    }

    let after = if target_idx == 0 {
        None
    } else {
        prev_other.or(last_other)
    };
    container.reorder_child_after(child, after.as_ref());
}

/// Translate `(x, y)` from `from` widget coordinates into `to` widget coordinates.
pub fn widget_translate_coords(
    from: &impl gtk::prelude::WidgetExt,
    to: &impl gtk::prelude::WidgetExt,
    x: f64,
    y: f64,
) -> Option<(f64, f64)> {
    let pt = gtk::graphene::Point::new(x as f32, y as f32);
    from.compute_point(to, &pt)
        .map(|p| (f64::from(p.x()), f64::from(p.y())))
}

/// Translate widget-local `(x, y)` coordinates into the widget's toplevel (`root`) surface space.
pub fn widget_to_toplevel_coords(widget: &impl gtk::prelude::WidgetExt, x: f64, y: f64) -> (f64, f64) {
    if let Some(root) = widget.root() {
        widget_translate_coords(widget, &root, x, y).unwrap_or((x, y))
    } else {
        (x, y)
    }
}

/// Compute `(x, y, width, height)` bounds of `widget` relative to `target`.
/// Returns `None` if `widget` has not been allocated (`width <= 0 && height <= 0`)
/// or shares no ancestor with `target`.
pub fn widget_bounds_in(
    widget: &impl gtk::prelude::WidgetExt,
    target: &impl gtk::prelude::WidgetExt,
) -> Option<(i32, i32, i32, i32)> {
    if widget.width() <= 0 && widget.height() <= 0 {
        return None;
    }
    widget.compute_bounds(target).map(|r| {
        (
            r.x().round() as i32,
            r.y().round() as i32,
            r.width().round() as i32,
            r.height().round() as i32,
        )
    })
}

#[cfg(test)]
type GtkTestJob = (
    Box<dyn FnOnce() + Send + 'static>,
    std::sync::mpsc::Sender<std::thread::Result<()>>,
);

/// Execute a test closure on a dedicated, persistent GTK main thread.
///
/// Rust's test harness spawns a fresh OS thread per `#[test]`, whereas GTK4
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
                ensure_cairo_renderer();
                if let Err(err) = gtk::init() {
                    let _ = init_tx.send(Err(format!("Failed to initialize GTK for tests: {err}")));
                    return;
                }
                if !gtk::is_initialized_main_thread() {
                    let _ = init_tx.send(Err("GTK initialized on another thread before gtk-test-main".to_string()));
                    return;
                }
                let _ = init_tx.send(Ok(()));

                let main_ctx = glib::MainContext::default();
                while let Ok((job, reply_tx)) = rx.recv() {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    while main_ctx.pending() {
                        main_ctx.iteration(false);
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

    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct EnvRestoreGuard {
        vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl Drop for EnvRestoreGuard {
        fn drop(&mut self) {
            for (k, v) in self.vars.drain(..) {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn test_ensure_cairo_renderer_defaults_to_cairo() {
        let _lock = lock(&ENV_TEST_LOCK);
        let _restore = EnvRestoreGuard {
            vars: vec![
                ("GSK_RENDERER", std::env::var_os("GSK_RENDERER")),
                ("GTK_A11Y", std::env::var_os("GTK_A11Y")),
            ],
        };
        ensure_cairo_renderer();
        let val = std::env::var("GSK_RENDERER").unwrap_or_default();
        assert!(!val.is_empty());
    }

    #[test]
    fn test_widget_children_and_reorder_box_child() {
        run_gtk_test(|| {
            use gtk::prelude::*;
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let b1 = gtk::Button::new();
            let b2 = gtk::Button::new();
            let b3 = gtk::Button::new();
            b.append(&b1);
            b.append(&b2);
            b.append(&b3);

            let children = widget_children(&b);
            assert_eq!(children.len(), 3);
            assert_eq!(&children[0], b1.upcast_ref::<gtk::Widget>());
            assert_eq!(&children[1], b2.upcast_ref::<gtk::Widget>());
            assert_eq!(&children[2], b3.upcast_ref::<gtk::Widget>());

            // Move b3 to index 0
            reorder_box_child(&b, &b3, 0);
            let children = widget_children(&b);
            assert_eq!(&children[0], b3.upcast_ref::<gtk::Widget>());
            assert_eq!(&children[1], b1.upcast_ref::<gtk::Widget>());
            assert_eq!(&children[2], b2.upcast_ref::<gtk::Widget>());

            // Move b3 to index 1
            reorder_box_child(&b, &b3, 1);
            let children = widget_children(&b);
            assert_eq!(&children[0], b1.upcast_ref::<gtk::Widget>());
            assert_eq!(&children[1], b3.upcast_ref::<gtk::Widget>());
            assert_eq!(&children[2], b2.upcast_ref::<gtk::Widget>());
        });
    }

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
    fn test_ensure_cairo_renderer_sets_defaults() {
        let _lock = lock(&ENV_TEST_LOCK);
        let _restore = EnvRestoreGuard {
            vars: vec![
                ("GSK_RENDERER", std::env::var_os("GSK_RENDERER")),
                ("GTK_A11Y", std::env::var_os("GTK_A11Y")),
            ],
        };
        std::env::remove_var("GSK_RENDERER");
        std::env::remove_var("GTK_A11Y");
        ensure_cairo_renderer();
        assert_eq!(std::env::var("GSK_RENDERER").as_deref(), Ok("cairo"));
        assert_eq!(std::env::var("GTK_A11Y").as_deref(), Ok("none"));
    }

    #[test]
    fn test_widget_bounds_and_translate_coords() {
        run_gtk_test(|| {
            use gtk::prelude::*;

            let win = gtk::Window::new();
            win.set_default_size(200, 300);
            let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
            spacer.set_size_request(40, 50);
            let inner = gtk::Box::new(gtk::Orientation::Vertical, 10);
            let b1 = gtk::Button::new();
            b1.set_size_request(40, 30);
            let b2 = gtk::Button::new();
            b2.set_size_request(40, 30);

            inner.append(&b1);
            inner.append(&b2);
            outer.append(&spacer);
            outer.append(&inner);
            win.set_child(Some(&outer));
            // Before layout/map, unallocated child reports None rather than GTK's (-5,-5,10,10) stub.
            assert_eq!(widget_bounds_in(&b1, &inner), None);

            win.present();

            let ctx = glib::MainContext::default();
            for _ in 0..50 {
                while ctx.pending() {
                    ctx.iteration(false);
                }
                if b1.height() > 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }

            let (_, y1, _, h1) = widget_bounds_in(&b1, &inner).expect("b1 bounds in inner");
            let (_, y2, _, h2) = widget_bounds_in(&b2, &inner).expect("b2 bounds in inner");
            assert_eq!(y1, 0);
            assert!(h1 >= 30);
            assert_eq!(y2, y1 + h1 + 10);
            assert!(h2 >= 30);

            let (_, inner_top_in_win) = widget_to_toplevel_coords(&inner, 0.0, 0.0);
            let (bx0, by0) = widget_translate_coords(&b2, &inner, 0.0, 0.0).expect("b2 origin in inner");
            let (bx5, by5) = widget_translate_coords(&b2, &inner, 0.0, 5.0).expect("b2 +5 in inner");
            assert!((bx5 - bx0).abs() < 0.5);
            assert!((by5 - (by0 + 5.0)).abs() < 0.5);
            assert!(by0 >= f64::from(y2));
            assert!(inner_top_in_win >= 50.0);

            win.close();
        });
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
