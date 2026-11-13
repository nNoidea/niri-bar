use gdk::prelude::*;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use crate::bar::BarWindow;
use crate::config::AppConfig;
use crate::hotreload::{refresh_error_badges, ActiveError};
use crate::modules::tray::TrayService;
use crate::modules::SharedModules;
use crate::niri::NiriService;

const GATE_IDLE: u8 = 0;
const GATE_QUEUED: u8 = 1;
const GATE_RUNNING: u8 = 2;
const GATE_RUNNING_RERUN: u8 = 3;

/// Single-threaded coalescing gate for monitor reconcile scheduling.
///
/// Signals arriving while idle-queued coalesce into one run; signals arriving
/// re-entrantly during `reconcile_monitors` schedule one follow-up pass. Uses
/// `Cell<u8>` so it can never fail a borrow or wedge stuck.
#[derive(Clone, Default)]
pub struct ReconcileGate(Rc<Cell<u8>>);

impl ReconcileGate {
    pub fn try_schedule(&self) -> bool {
        match self.0.get() {
            GATE_IDLE => {
                self.0.set(GATE_QUEUED);
                true
            }
            GATE_RUNNING => {
                self.0.set(GATE_RUNNING_RERUN);
                false
            }
            _ => false,
        }
    }

    pub fn begin_run(&self) {
        self.0.set(GATE_RUNNING);
    }

    pub fn finish_run(&self) -> bool {
        if self.0.get() == GATE_RUNNING_RERUN {
            self.0.set(GATE_QUEUED);
            true
        } else {
            self.0.set(GATE_IDLE);
            false
        }
    }
}

#[derive(Clone)]
pub struct ReconcileCtx {
    pub display: gdk::Display,
    pub bars: Rc<RefCell<Vec<BarWindow>>>,
    pub config: Rc<RefCell<AppConfig>>,
    pub niri: Arc<NiriService>,
    pub tray_service: Rc<RefCell<Option<Arc<TrayService>>>>,
    pub modules: Rc<RefCell<SharedModules>>,
    pub gate: ReconcileGate,
    pub active_error: Rc<RefCell<ActiveError>>,
}

/// Starts or stops the tray service lazily according to `[modules]` config.
pub fn sync_tray_service(slot: &Rc<RefCell<Option<Arc<TrayService>>>>, config: &AppConfig) {
    sync_tray_service_with(slot, config, &mut || Arc::new(TrayService::new()));
}

fn sync_tray_service_with<F: FnMut() -> Arc<TrayService>>(
    slot: &Rc<RefCell<Option<Arc<TrayService>>>>,
    config: &AppConfig,
    make_service: &mut F,
) {
    let Ok(mut guard) = slot.try_borrow_mut() else {
        log_warn!("monitor", "Skipping tray service sync: slot borrowed");
        return;
    };
    let enabled = config.modules.contains_module("tray");
    match (enabled, &mut *guard) {
        (true, None) => *guard = Some(make_service()),
        (false, Some(svc)) => {
            svc.shutdown();
            *guard = None;
        }
        _ => {}
    }
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
    tray_service: Option<&Arc<TrayService>>,
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
    let existing_monitors: Vec<gdk::Monitor> = match bars.try_borrow() {
        Ok(list) => list.iter().map(|b| b.monitor.clone()).collect(),
        Err(e) => {
            log_warn!("monitor", "Bars borrow failed during hotplug check: {e}");
            Vec::new()
        }
    };
    let mut new_bars = Vec::new();
    for monitor in current_monitors {
        if !existing_monitors.contains(&monitor) {
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
            new_bars.push(BarWindow::new(monitor, config, niri, tray_service, modules));
        }
    }

    // 4. Attach new bars and update all active bars in a single borrow
    match bars.try_borrow_mut() {
        Ok(mut list) => {
            list.extend(new_bars);
            for b in list.iter_mut() {
                b.update(niri, config);
            }
        }
        Err(e) => log_warn!("monitor", "Skipping bar updates: borrow failed: {e}"),
    }
}

pub fn do_reconcile(ctx: &ReconcileCtx) {
    ctx.gate.begin_run();
    let (cfg, mods, tray, err) = match (
        ctx.config.try_borrow(),
        ctx.modules.try_borrow(),
        ctx.tray_service.try_borrow(),
        ctx.active_error.try_borrow(),
    ) {
        (Ok(cfg), Ok(mods), Ok(tray), Ok(err)) => (cfg.clone(), mods.clone(), tray.clone(), err.clone()),
        _ => {
            log_warn!("monitor", "Reconcile contended; rescheduling");
            let next = ctx.clone();
            glib::idle_add_local_once(move || {
                do_reconcile(&next);
            });
            return;
        }
    };
    reconcile_monitors(&ctx.display, &ctx.bars, &cfg, &ctx.niri, tray.as_ref(), &mods);

    if !err.is_empty() {
        refresh_error_badges(&ctx.bars, &err);
    }

    if ctx.gate.finish_run() {
        log_info!("monitor", "Re-running reconcile (signals arrived during previous run)");
        let next = ctx.clone();
        glib::idle_add_local_once(move || {
            do_reconcile(&next);
        });
    }
}

pub fn make_reconcile_scheduler(ctx: &ReconcileCtx) -> Rc<dyn Fn()> {
    let ctx = ctx.clone();
    Rc::new(move || {
        if !ctx.gate.try_schedule() {
            log_info!("monitor", "Reconcile already queued or in progress; coalescing request");
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
        let niri_wake = Arc::clone(niri);
        glib::MainContext::default().spawn_local(async move {
            while let Ok(entering_sleep) = wake_rx.recv().await {
                if entering_sleep {
                    log_info!("power", "System is entering sleep / suspend...");
                } else {
                    log_info!("power", "System resumed from sleep / wake up! Reconciling displays...");
                    niri_wake.refresh_outputs_async();
                    sched_wake();
                }
            }
        });
    }

    // Monitor hotplug signals via GDK4's `display.monitors()` ListModel.
    // Every hotplug signal triggers an off-thread `refresh_outputs_async` so the
    // Niri snapshot postdates the signal even if GDK races `OutputsChanged`.
    let sched_hotplug = Rc::clone(schedule_reconcile);
    let niri_hotplug = Arc::clone(niri);
    display
        .monitors()
        .connect_items_changed(move |list, position, removed, added| {
            log_info!(
                "monitor",
                "Received GDK monitors items_changed signal (pos={position}, removed={removed}, added={added})"
            );
            niri_hotplug.refresh_outputs_async();
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
                    let niri_geom = Arc::clone(&niri_hotplug);
                    let handler = Rc::new(Cell::new(None::<glib::SignalHandlerId>));
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
                            niri_geom.refresh_outputs_async();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reconcile_gate_coalesces_queued_calls_and_tracks_reentrant() {
        let gate = ReconcileGate::default();
        // Multiple triggers before the idle callback runs coalesce into 1 run.
        assert!(gate.try_schedule());
        assert!(!gate.try_schedule());
        assert!(!gate.try_schedule());

        gate.begin_run();
        assert!(
            !gate.finish_run(),
            "coalesced pre-run schedules must not trigger a second pass"
        );

        // Now triggers firing re-entrantly DURING begin_run..finish_run must queue 1 follow-up pass.
        assert!(gate.try_schedule());
        gate.begin_run();
        assert!(!gate.try_schedule());
        assert!(
            gate.finish_run(),
            "re-entrant signal during run must schedule follow-up"
        );

        // Follow-up run finishes cleanly back to IDLE.
        gate.begin_run();
        assert!(!gate.finish_run());
        assert!(gate.try_schedule());
    }

    #[test]
    fn test_sync_tray_service_toggles_lazily() {
        let slot: Rc<RefCell<Option<Arc<TrayService>>>> = Rc::new(RefCell::new(None));
        let mut cfg_disabled = AppConfig::default();
        cfg_disabled.modules.order.clear();
        cfg_disabled.modules.start = Some(Vec::new());
        cfg_disabled.modules.end = Some(vec!["clock".to_string()]);

        let mut created = 0usize;
        sync_tray_service_with(&slot, &cfg_disabled, &mut || {
            created += 1;
            Arc::new(TrayService::new_for_test())
        });
        assert!(slot.borrow().is_none(), "tray disabled -> no service");
        assert_eq!(created, 0);

        let mut cfg_enabled = cfg_disabled.clone();
        cfg_enabled.modules.end = Some(vec!["clock".to_string(), "tray".to_string()]);
        sync_tray_service_with(&slot, &cfg_enabled, &mut || {
            created += 1;
            Arc::new(TrayService::new_for_test())
        });
        let first = Arc::clone(slot.borrow().as_ref().expect("tray enabled"));
        assert_eq!(created, 1);
        assert!(!first.is_stopped());

        // Reload with tray still enabled reuses existing service instance.
        sync_tray_service_with(&slot, &cfg_enabled, &mut || {
            created += 1;
            Arc::new(TrayService::new_for_test())
        });
        let second = Arc::clone(slot.borrow().as_ref().expect("tray kept"));
        assert_eq!(created, 1);
        assert!(Arc::ptr_eq(&first, &second));

        // Removing tray shuts down and clears slot.
        sync_tray_service_with(&slot, &cfg_disabled, &mut || {
            created += 1;
            Arc::new(TrayService::new_for_test())
        });
        assert!(slot.borrow().is_none());
        assert!(first.is_stopped());
    }
}
