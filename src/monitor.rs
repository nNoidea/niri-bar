use gdk::prelude::*;
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::bar::BarWindow;
use crate::config::AppConfig;
use crate::hotreload::{refresh_error_badges, ActiveError};
use crate::modules::tray::TrayService;
use crate::modules::SharedModules;
use crate::niri::NiriService;

#[derive(Clone)]
pub struct ReconcileCtx {
    pub display: gdk::Display,
    pub bars: Rc<RefCell<Vec<BarWindow>>>,
    pub config: Rc<RefCell<AppConfig>>,
    pub niri: Arc<NiriService>,
    pub tray_service: Arc<TrayService>,
    pub modules: Rc<RefCell<SharedModules>>,
    pub is_reconciling: Rc<RefCell<bool>>,
    pub needs_reconcile: Rc<RefCell<bool>>,
    pub active_error: Rc<RefCell<ActiveError>>,
}

/// Enumerate all `gdk::Monitor` instances currently known to `display`.
pub fn display_monitors(display: &gdk::Display) -> Vec<gdk::Monitor> {
    let list = display.monitors();
    (0..list.n_items())
        .filter_map(|i| list.item(i).and_then(|obj| obj.downcast::<gdk::Monitor>().ok()))
        .collect()
}

pub fn reconcile_monitors(
    display: &gdk::Display,
    bars: &Rc<RefCell<Vec<BarWindow>>>,
    config: &AppConfig,
    niri: &Arc<NiriService>,
    tray_service: &Arc<TrayService>,
    modules: &SharedModules,
) {
    // NOTE: no blocking IPC fetches here. `NiriService` owns outputs/workspaces
    // freshness on its background thread (event stream + `OutputsChanged`
    // refetch without holding the state lock). Fetching synchronously would
    // stall the GTK main thread on Unix-socket round-trips per hotplug/wakeup.
    // 1. Discover all currently valid GDK monitors
    let all_monitors = display_monitors(display);
    let n_monitors = all_monitors.len();
    let mut current_monitors: Vec<gdk::Monitor> = Vec::new();
    let mut has_unready_monitors = false;
    for (i, monitor) in all_monitors.into_iter().enumerate() {
        if monitor.geometry().width() > 0 {
            current_monitors.push(monitor);
        } else {
            has_unready_monitors = true;
            log_warn!(
                "monitor",
                "Monitor {} has zero geometry (not yet configured by compositor), will retry [model: {:?}, manuf: {:?}]",
                i,
                monitor.model(),
                monitor.manufacturer()
            );
        }
    }

    log_info!(
        "monitor",
        "Reconciling displays: {} valid monitor(s) detected (GDK count: {}{})",
        current_monitors.len(),
        n_monitors,
        if has_unready_monitors { ", has unready" } else { "" }
    );

    // 2. Remove bars for monitors that are no longer valid or connected
    let mut to_destroy = Vec::new();
    match bars.try_borrow_mut() {
        Ok(mut list) => {
            list.retain(|b| {
                let still_valid = current_monitors.contains(&b.monitor);
                if !still_valid {
                    log_info!(
                        "monitor",
                        "Destroying bar for disconnected monitor (detected output: {:?})",
                        b.detected_output
                    );
                    to_destroy.push(b.window.clone());
                    false
                } else {
                    true
                }
            });
        }
        Err(e) => log_warn!("monitor", "Skipping disconnect pass: bars borrow failed: {e}"),
    }

    for win in to_destroy {
        // Deferred safe teardown: `close()` via idle so we don't tear down
        // inside the reconcile borrow. Main thread by construction.
        glib::idle_add_local_once(move || {
            win.close();
        });
    }

    // 3. Create new bars for newly attached monitors (constructed WITHOUT holding a RefCell lock)
    let mut new_bars = Vec::new();
    for monitor in current_monitors {
        let already_exists = match bars.try_borrow() {
            Ok(list) => list.iter().any(|b| b.monitor == monitor),
            Err(e) => {
                log_warn!("monitor", "Bars borrow failed during hotplug check: {e}");
                false
            }
        };

        if !already_exists {
            let geom = monitor.geometry();
            log_info!(
                "monitor",
                "Spawning new BarWindow for monitor: {}x{} at ({},{}) [model: {:?}, manuf: {:?}]",
                geom.width(),
                geom.height(),
                geom.x(),
                geom.y(),
                monitor.model(),
                monitor.manufacturer()
            );
            let bar = BarWindow::new(monitor, config, niri, tray_service, modules);
            new_bars.push(bar);
        }
    }

    if !new_bars.is_empty() {
        match bars.try_borrow_mut() {
            Ok(mut list) => list.extend(new_bars),
            Err(e) => log_warn!("monitor", "Dropping {} new bar(s): borrow failed: {e}", new_bars.len()),
        }
    }

    // 4. Update all remaining and newly created bars
    match bars.try_borrow_mut() {
        Ok(mut list) => {
            for b in list.iter_mut() {
                b.update(niri, config);
            }
        }
        Err(e) => log_warn!("monitor", "Skipping bar updates: borrow failed: {e}"),
    }
}

pub fn do_reconcile(ctx: &ReconcileCtx) {
    let (cfg, mods, err) = match (
        ctx.config.try_borrow(),
        ctx.modules.try_borrow(),
        ctx.active_error.try_borrow(),
    ) {
        (Ok(cfg), Ok(mods), Ok(err)) => (cfg.clone(), mods.clone(), err.clone()),
        _ => {
            log_warn!("monitor", "Reconcile contended; rescheduling");
            let next = ctx.clone();
            glib::idle_add_local_once(move || {
                do_reconcile(&next);
            });
            return;
        }
    };
    reconcile_monitors(&ctx.display, &ctx.bars, &cfg, &ctx.niri, &ctx.tray_service, &mods);

    if !err.is_empty() {
        refresh_error_badges(&ctx.bars, &err);
    }

    let rerun = match ctx.needs_reconcile.try_borrow_mut() {
        Ok(mut flag) => std::mem::replace(&mut *flag, false),
        Err(e) => {
            log_warn!("monitor", "needs_reconcile borrow failed: {e}");
            false
        }
    };

    if rerun {
        log_info!("monitor", "Re-running reconcile (signals arrived during previous run)");
        let next = ctx.clone();
        glib::idle_add_local_once(move || {
            do_reconcile(&next);
        });
    } else if let Ok(mut busy) = ctx.is_reconciling.try_borrow_mut() {
        *busy = false;
    }
}

pub fn make_reconcile_scheduler(ctx: &ReconcileCtx) -> Rc<dyn Fn()> {
    let ctx = ctx.clone();
    Rc::new(move || {
        let busy = ctx.is_reconciling.try_borrow().map(|b| *b).unwrap_or(false);
        if busy {
            if let Ok(mut flag) = ctx.needs_reconcile.try_borrow_mut() {
                *flag = true;
            }
            log_info!(
                "monitor",
                "Reconcile already in progress, will re-run after current one completes"
            );
            return;
        }
        if let Ok(mut b) = ctx.is_reconciling.try_borrow_mut() {
            *b = true;
        } else {
            log_warn!("monitor", "schedule_reconcile contended; dropping request");
            return;
        }

        let next = ctx.clone();
        glib::idle_add_local_once(move || {
            do_reconcile(&next);
        });
    })
}

/// Wire GDK monitor hotplug signals (`monitor_added`, zero-geometry `notify::geometry`,
/// `monitor_removed`) and systemd-logind `PrepareForSleep` resume reconciliation.
pub fn connect_monitor_and_sleep_signals(
    display: &gdk::Display,
    niri: &Arc<NiriService>,
    schedule_reconcile: &Rc<dyn Fn()>,
) {
    // Freshness trigger: GDK hotplug signals and Niri's `OutputsChanged`
    // race each other. A bar built from a pre-hotplug snapshot resolves
    // to `None` and shows an empty taskbar, so every hotplug-class signal
    // below also fires one off-thread outputs refetch — the snapshot then
    // postdates the signal by construction. Per-bar 5s retry stays as
    // backstop for residual races.
    let refresh_niri_outputs = {
        let niri = Arc::clone(niri);
        Rc::new(move || niri.refresh_outputs_async())
    };

    // System Sleep & Wakeup Listener (logind DBus via zbus)
    {
        let (wake_tx, wake_rx) = async_channel::unbounded::<bool>();
        crate::runtime::spawn(async move {
            use futures_util::StreamExt;
            let Some(conn) = crate::modules::system_bus().await else {
                return;
            };
            let Ok(rule) = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender("org.freedesktop.login1")
                .interface("org.freedesktop.login1.Manager")
                .member("PrepareForSleep")
                .path("/org/freedesktop/login1")
                .build()
            else {
                return;
            };
            let Ok(mut stream) = zbus::MessageStream::for_match_rule(rule, &conn, None).await else {
                return;
            };
            while let Some(Ok(msg)) = stream.next().await {
                if let Ok(entering_sleep) = msg.body().deserialize::<bool>() {
                    if wake_tx.send(entering_sleep).await.is_err() {
                        break;
                    }
                }
            }
        });

        let sched_wake = Rc::clone(schedule_reconcile);
        let refresh_wake = Rc::clone(&refresh_niri_outputs);
        glib::MainContext::default().spawn_local(async move {
            while let Ok(entering_sleep) = wake_rx.recv().await {
                if entering_sleep {
                    log_info!("power", "System is entering sleep / suspend...");
                } else {
                    log_info!("power", "System resumed from sleep / wake up! Reconciling displays...");
                    refresh_wake();
                    sched_wake();
                }
            }
        });
    }

    // Monitor hotplug signals via GDK4's `display.monitors()` ListModel
    let sched_hotplug = Rc::clone(schedule_reconcile);
    let refresh_hotplug = Rc::clone(&refresh_niri_outputs);
    display
        .monitors()
        .connect_items_changed(move |list, position, removed, added| {
            log_info!(
                "monitor",
                "Received GDK monitors items_changed signal (pos={position}, removed={removed}, added={added})"
            );
            refresh_hotplug();
            sched_hotplug();

            // GDK may report a newly added monitor before the Wayland compositor has
            // sent its geometry (width == 0). Subscribe to the GObject notify::geometry
            // signal so we re-reconcile the instant the real geometry arrives.
            for idx in position..position.saturating_add(added) {
                let Some(monitor) = list.item(idx).and_then(|obj| obj.downcast::<gdk::Monitor>().ok()) else {
                    continue;
                };
                if monitor.geometry().width() == 0 {
                    log_info!(
                        "monitor",
                        "New monitor has zero geometry, subscribing to notify::geometry"
                    );
                    let sched = Rc::clone(&sched_hotplug);
                    let refresh = Rc::clone(&refresh_hotplug);
                    let handler = std::rc::Rc::new(std::cell::Cell::new(None::<glib::SignalHandlerId>));
                    let handler_clone = Rc::clone(&handler);
                    let mon_weak = monitor.downgrade();
                    let id = monitor.connect_notify_local(Some("geometry"), move |mon, _| {
                        let geom = mon.geometry();
                        if geom.width() > 0 {
                            log_info!(
                                "monitor",
                                "Monitor geometry now available ({}x{}), reconciling",
                                geom.width(),
                                geom.height()
                            );
                            refresh();
                            sched();
                            // One-shot: disconnect to avoid accumulation.
                            if let Some(mon_strong) = mon_weak.upgrade() {
                                if let Some(hid) = handler_clone.take() {
                                    mon_strong.disconnect(hid);
                                }
                            }
                        }
                    });
                    handler.set(Some(id));
                }
            }
        });
}
