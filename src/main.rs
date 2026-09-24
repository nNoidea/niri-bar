#[macro_use]
mod logger;
mod bar;
mod config;
mod icon;
mod layer_shell;
mod modules;
mod niri;
mod style;
mod util;

use gdk::prelude::MonitorExt;
use gio::prelude::*;
use gtk::prelude::*;
use std::cell::RefCell;
use std::fs;
use std::rc::Rc;
use std::sync::Arc;

use crate::bar::BarWindow;
use crate::config::{load_config_with_error, AppConfig};
use crate::logger::init_logger;
use crate::modules::tray::TrayService;
use crate::modules::SharedModules;
use crate::niri::NiriService;
use crate::style::init_styles;

const EMBEDDED_FONT: &[u8] = include_bytes!("../resources/NerdFont-Regular.ttf");

/// Early CLI action, parsed before GTK/logger init so `--help` works headless.
#[derive(Debug, PartialEq, Eq)]
enum EarlyAction {
    Run,
    Version,
    Help,
    Check,
}

fn parse_early_args(args: &[String]) -> EarlyAction {
    for a in &args[1..] {
        match a.as_str() {
            "--version" | "-V" => return EarlyAction::Version,
            "--help" | "-h" => return EarlyAction::Help,
            "--check" => return EarlyAction::Check,
            _ => {}
        }
    }
    EarlyAction::Run
}

fn print_help() {
    println!(
        "niri-bar v{}\n\nUSAGE:\n    niri-bar [OPTIONS]\n\nOPTIONS:\n    -h, --help       Print this help\n    -V, --version    Print version\n        --check      Validate config + CSS and exit (0 ok, 1 error)\n\nENV:\n    NIRI_SOCKET      Niri IPC socket (set by Niri)\n    NIRI_BAR_DEBUG=1 Verbose debug logs\n    RUST_LOG=debug   Verbose debug logs\n\nCONFIG:\n    ~/.config/niri-bar/config.toml (see resources/config.default.toml)\n    ~/.config/niri-bar/style.css",
        env!("CARGO_PKG_VERSION")
    );
}

/// Claim the session-bus instance name. Returns false when another niri-bar
/// already owns it — duplicate instances would stack duplicate bars,
/// duplicate fullscreen drag overlays, and split press/release event
/// delivery between two independent drag states (ghost on the wrong
/// monitor, stuck drags). No bus available (tests/headless) allows startup.
fn ensure_single_instance() -> bool {
    const NAME: &str = "org.nnoidea.niri-bar";
    let conn = match gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
        Ok(c) => c,
        Err(_) => return true,
    };
    let reply = conn.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "RequestName",
        Some(&(NAME, 0u32).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        5000,
        gio::Cancellable::NONE,
    );
    match reply {
        Ok(v) => {
            // 1 = primary owner. Anything else: someone got here first.
            v.child_value(0).get::<u32>() == Some(1)
        }
        Err(_) => true,
    }
}

fn register_embedded_font() {
    let data_home = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")));

    if let Ok(data_dir) = data_home {
        let font_dir = data_dir.join("fonts/niri-bar");
        let font_path = font_dir.join("NerdFont-Regular.ttf");

        let needs_write = match fs::metadata(&font_path) {
            Ok(meta) => meta.len() as usize != EMBEDDED_FONT.len(),
            Err(_) => true,
        };

        if needs_write {
            if let Err(e) = fs::create_dir_all(&font_dir) {
                log_warn!("startup", "Failed to create font dir {font_dir:?}: {e}");
                return;
            }
            if let Err(e) = fs::write(&font_path, EMBEDDED_FONT) {
                log_warn!("startup", "Failed to write embedded font {font_path:?}: {e}");
                return;
            }
            std::thread::spawn(move || {
                let fc = crate::modules::resolve_helper("fc-cache");
                if let Err(e) = std::process::Command::new(&fc).arg("-f").arg(&font_dir).output() {
                    // `fc-cache` failure is non-fatal; font still loads via fontconfig fallback.
                    log_warn!("startup", "fc-cache failed for {font_dir:?}: {e}");
                }
            });
        }
    }
}

fn reconcile_monitors(
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
    let n_monitors = display.n_monitors();
    let mut current_monitors: Vec<gdk::Monitor> = Vec::new();
    let mut has_unready_monitors = false;
    for i in 0..n_monitors {
        if let Some(monitor) = display.monitor(i) {
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

/// Live config/style error state. Config and style fail independently,
/// so each has its own slot — fixing one must not clear the other's badge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActiveError {
    pub config: Option<String>,
    pub style: Option<String>,
}

impl ActiveError {
    /// Badge text: config takes precedence when both are broken.
    pub fn message(&self) -> Option<&str> {
        self.config.as_deref().or(self.style.as_deref())
    }

    pub fn is_empty(&self) -> bool {
        self.config.is_none() && self.style.is_none()
    }
}

/// Pure filename filter for the hot-reload watcher: which watched file
/// (if any) does a `GFileMonitor` changed event refer to?
/// Checks both `file` and `other` so atomic renames (vim/helix write-tmp +
/// rename) are caught regardless of which side Gio reports.
pub fn watch_target_for_filenames(file_basename: Option<&str>, other_basename: Option<&str>) -> Option<&'static str> {
    for name in [file_basename, other_basename].into_iter().flatten() {
        if name == "config.toml" {
            return Some("config.toml");
        }
        if name == "style.css" {
            return Some("style.css");
        }
    }
    None
}

fn watch_target_for_event(file: &gio::File, other: Option<&gio::File>) -> Option<&'static str> {
    let file_name = file.basename().map(|n| n.to_string_lossy().to_string());
    let other_name = other.and_then(|f| f.basename().map(|n| n.to_string_lossy().to_string()));
    watch_target_for_filenames(file_name.as_deref(), other_name.as_deref())
}

/// Re-apply the combined [`ActiveError`] to every bar: show the message
/// when any slot is set, clear only when both are `None`.
fn refresh_error_badges(bars: &Rc<RefCell<Vec<BarWindow>>>, err: &ActiveError) {
    let Ok(list) = bars.try_borrow() else { return };
    match err.message() {
        Some(msg) => {
            for b in list.iter() {
                b.show_error(msg);
            }
        }
        None => {
            for b in list.iter() {
                b.clear_error();
            }
        }
    }
}

#[derive(Clone)]
struct ReconcileCtx {
    display: gdk::Display,
    bars: Rc<RefCell<Vec<BarWindow>>>,
    config: Rc<RefCell<AppConfig>>,
    niri: Arc<NiriService>,
    tray_service: Arc<TrayService>,
    modules: Rc<RefCell<SharedModules>>,
    is_reconciling: Rc<RefCell<bool>>,
    needs_reconcile: Rc<RefCell<bool>>,
    active_error: Rc<RefCell<ActiveError>>,
}

fn do_reconcile(ctx: &ReconcileCtx) {
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

fn setup_file_watcher(ctx: ReconcileCtx, schedule_reconcile: Rc<dyn Fn()>) -> Option<gio::FileMonitor> {
    let config_dir = crate::config::get_config_dir();
    let watch_path = fs::canonicalize(&config_dir).unwrap_or(config_dir);
    let gfile = gio::File::for_path(&watch_path);
    let monitor = match gfile.monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) {
        Ok(m) => m,
        Err(e) => {
            log_warn!("hotreload", "Failed to start directory monitor on {watch_path:?}: {e}");
            return None;
        }
    };

    log_info!(
        "hotreload",
        "Live config & style hot-reloading active on {watch_path:?}"
    );

    // Per-file debounce: a shared timer would drop one file when both are
    // saved within the same 100ms window (last event wins).
    let debounce_config: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let debounce_style: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    monitor.connect_changed(move |_mon, file, other, event| {
        use gio::FileMonitorEvent::*;
        let deleted = matches!(event, Deleted | MovedOut);
        if !deleted {
            match event {
                Changed | Created | ChangesDoneHint | AttributeChanged | Moved | MovedIn | Renamed => {}
                _ => return,
            }
        }

        let target_name = match watch_target_for_event(file, other) {
            Some(n) => n,
            None => return,
        };

        if deleted {
            let msg = format!("{target_name} was deleted");
            if target_name == "style.css" {
                log_warn!("style", "Live style reload error: {msg}");
                if let Ok(mut err) = ctx.active_error.try_borrow_mut() {
                    err.style = Some(msg);
                    refresh_error_badges(&ctx.bars, &err);
                }
            } else {
                log_warn!("config", "Live config reload error: {msg}");
                if let Ok(mut err) = ctx.active_error.try_borrow_mut() {
                    err.config = Some(msg);
                    refresh_error_badges(&ctx.bars, &err);
                }
            }
            return;
        }

        let deb = if target_name == "style.css" {
            Rc::clone(&debounce_style)
        } else {
            Rc::clone(&debounce_config)
        };
        let ctx = ctx.clone();
        let sched = Rc::clone(&schedule_reconcile);

        if let Some(src_id) = deb.borrow_mut().take() {
            src_id.remove();
        }

        let deb_clone = Rc::clone(&deb);
        let new_src = glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
            *deb_clone.borrow_mut() = None;
            if target_name == "style.css" {
                match crate::style::reload_styles() {
                    Ok(()) => {
                        log_info!("style", "style.css live-reloaded successfully");
                        if let Ok(mut err) = ctx.active_error.try_borrow_mut() {
                            err.style = None;
                            refresh_error_badges(&ctx.bars, &err);
                        }
                    }
                    Err(e) => {
                        log_warn!("style", "Live style reload error: {e}");
                        if let Ok(mut err) = ctx.active_error.try_borrow_mut() {
                            err.style = Some(e);
                            refresh_error_badges(&ctx.bars, &err);
                        }
                    }
                }
            } else if target_name == "config.toml" {
                match crate::config::reload_config() {
                    Ok(new_config) => {
                        log_info!("config", "config.toml live-reloaded successfully; updating layout");
                        if let Ok(mut cfg) = ctx.config.try_borrow_mut() {
                            *cfg = new_config.clone();
                        }
                        if let Ok(mut mods) = ctx.modules.try_borrow_mut() {
                            mods.shutdown();
                            *mods = crate::modules::SharedModules::new(&new_config);
                        }

                        if let Ok(mut err) = ctx.active_error.try_borrow_mut() {
                            err.config = None;
                        }

                        if let Ok(mut bars) = ctx.bars.try_borrow_mut() {
                            for b in bars.drain(..) {
                                b.window.hide();
                                b.window.close();
                            }
                        }
                        sched();
                    }
                    Err(e) => {
                        log_warn!("config", "Live config reload error: {e}");
                        if let Ok(mut err) = ctx.active_error.try_borrow_mut() {
                            err.config = Some(e);
                            refresh_error_badges(&ctx.bars, &err);
                        }
                    }
                }
            }
        });
        *deb.borrow_mut() = Some(new_src);
    });

    Some(monitor)
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    match parse_early_args(&argv) {
        EarlyAction::Version => {
            println!("niri-bar v{}", env!("CARGO_PKG_VERSION"));
            return;
        }
        EarlyAction::Help => {
            print_help();
            return;
        }
        EarlyAction::Check => {
            let cfg_path = crate::config::get_config_dir().join("config.toml");
            let css_path = crate::config::get_config_dir().join("style.css");
            let mut ok = true;
            if cfg_path.exists() {
                // Same parser as startup/live-reload (per-section fallback +
                // validation), so `--check` agrees with the running bar.
                match std::fs::read_to_string(&cfg_path) {
                    Ok(content) => match crate::config::reload_config_from_str(&content) {
                        Ok(_) => println!("config OK: {cfg_path:?}"),
                        Err(e) => {
                            eprintln!("config ERROR {cfg_path:?}: {e}");
                            ok = false;
                        }
                    },
                    Err(e) => {
                        eprintln!("config READ ERROR {cfg_path:?}: {e}");
                        ok = false;
                    }
                }
            } else {
                println!("config: no file yet (defaults would be used)");
            }
            if css_path.exists() {
                match std::fs::read_to_string(&css_path) {
                    Ok(content) => match crate::style::validate_css(&content) {
                        Ok(()) => println!("css OK: {css_path:?}"),
                        Err(e) => {
                            eprintln!("css ERROR {css_path:?}: {e}");
                            ok = false;
                        }
                    },
                    Err(e) => {
                        eprintln!("css READ ERROR {css_path:?}: {e}");
                        ok = false;
                    }
                }
            } else {
                println!("css: no file yet (embedded default would be used)");
            }
            std::process::exit(if ok { 0 } else { 1 });
        }
        EarlyAction::Run => {}
    }
    init_logger();
    if !ensure_single_instance() {
        log_error!(
            "startup",
            "Another niri-bar instance already owns the session bus name; exiting to avoid duplicate bars and competing drag overlays."
        );
        eprintln!(
            "\x1b[1;31merror:\x1b[0m Another niri-bar instance is already running (owns org.nnoidea.niri-bar). Refusing to start a second one."
        );
        std::process::exit(1);
    }
    register_embedded_font();
    crate::icon::preload_desktop_entries_async();

    log_info!(
        "startup",
        "Starting niri-bar v{} (PID: {}, NIRI_SOCKET: {:?})",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        std::env::var("NIRI_SOCKET").ok()
    );

    if gtk::init().is_err() {
        log_error!("startup", "Failed to initialize GTK.");
        eprintln!(
            "\x1b[1;31merror:\x1b[0m Failed to initialize GTK. Ensure a Wayland display is available (Niri session)."
        );
        std::process::exit(1);
    }

    if !layer_shell::is_available() {
        let err_msg = layer_shell::missing_library_error_message();
        log_error!("startup", "{}", err_msg);
        eprintln!("\x1b[1;31merror:\x1b[0m {}", err_msg);
        std::process::exit(1);
    }

    if let Some(settings) = gtk::Settings::default() {
        settings.set_property("gtk-application-prefer-dark-theme", true);
    }

    let (config_val, initial_config_err) = load_config_with_error();
    let config = Rc::new(RefCell::new(config_val));
    let initial_style_err = init_styles();

    let niri = Arc::new(NiriService::new());
    let tray_service = Arc::new(TrayService::new());
    let modules = Rc::new(RefCell::new(crate::modules::SharedModules::new(&config.borrow())));
    let bars: Rc<RefCell<Vec<BarWindow>>> = Rc::new(RefCell::new(Vec::new()));
    let initial_err = ActiveError {
        config: initial_config_err,
        style: initial_style_err,
    };
    let active_error: Rc<RefCell<ActiveError>> = Rc::new(RefCell::new(initial_err));
    // Kept alive for the process lifetime: dropping the monitor stops events.
    // The underscore prefix only silences the unused-variable lint.
    let mut _file_monitor = None;

    if let Some(display) = gdk::Display::default() {
        let is_reconciling = Rc::new(RefCell::new(false));
        let needs_reconcile = Rc::new(RefCell::new(false));

        let ctx = ReconcileCtx {
            display: display.clone(),
            bars: Rc::clone(&bars),
            config: Rc::clone(&config),
            niri: Arc::clone(&niri),
            tray_service: Arc::clone(&tray_service),
            modules: Rc::clone(&modules),
            is_reconciling: Rc::clone(&is_reconciling),
            needs_reconcile: Rc::clone(&needs_reconcile),
            active_error: Rc::clone(&active_error),
        };

        let schedule_reconcile: Rc<dyn Fn()> = {
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
        };

        // Initial setup
        schedule_reconcile();

        // File monitor for config.toml and style.css live reloading
        _file_monitor = setup_file_watcher(ctx.clone(), Rc::clone(&schedule_reconcile));

        // Freshness trigger: GDK hotplug signals and Niri's `OutputsChanged`
        // race each other. A bar built from a pre-hotplug snapshot resolves
        // to `None` and shows an empty taskbar, so every hotplug-class signal
        // below also fires one off-thread outputs refetch — the snapshot then
        // postdates the signal by construction. Per-bar 5s retry stays as
        // backstop for residual races.
        let refresh_niri_outputs = {
            let niri = Arc::clone(&niri);
            Rc::new(move || niri.refresh_outputs_async())
        };

        // System Sleep & Wakeup Listener (logind DBus)
        if let Ok(conn) = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE) {
            let sched_wake = Rc::clone(&schedule_reconcile);
            let refresh_wake = Rc::clone(&refresh_niri_outputs);
            conn.signal_subscribe(
                Some("org.freedesktop.login1"),
                Some("org.freedesktop.login1.Manager"),
                Some("PrepareForSleep"),
                Some("/org/freedesktop/login1"),
                None,
                gio::DBusSignalFlags::NONE,
                move |_conn, _sender, _path, _iface, _signal, params| {
                    if let Some(val) = params.child_value(0).get::<bool>() {
                        if val {
                            log_info!("power", "System is entering sleep / suspend...");
                        } else {
                            log_info!("power", "System resumed from sleep / wake up! Reconciling displays...");
                            refresh_wake();
                            sched_wake();
                        }
                    }
                },
            );
        }

        // Monitor hotplug signals (idle debounced)
        let sched_added = Rc::clone(&schedule_reconcile);
        let refresh_added = Rc::clone(&refresh_niri_outputs);
        display.connect_monitor_added(move |_, monitor| {
            log_info!("monitor", "Received GDK monitor_added signal");
            refresh_added();
            sched_added();

            // GDK may fire monitor_added before the Wayland compositor has
            // sent the monitor's geometry (width == 0).  Subscribe to the
            // GObject notify::geometry signal so we re-reconcile the instant
            // the real geometry arrives — no polling or arbitrary delays.
            // Disconnected after the first valid geometry to avoid handler
            // accumulation on flapping monitors.
            if monitor.geometry().width() == 0 {
                log_info!(
                    "monitor",
                    "New monitor has zero geometry, subscribing to notify::geometry"
                );
                let sched = Rc::clone(&sched_added);
                let refresh = Rc::clone(&refresh_added);
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
                                glib::signal_handler_disconnect(&mon_strong, hid);
                            }
                        }
                    }
                });
                handler.set(Some(id));
            }
        });

        let sched_removed = Rc::clone(&schedule_reconcile);
        let refresh_removed = Rc::clone(&refresh_niri_outputs);
        display.connect_monitor_removed(move |_, _| {
            log_info!("monitor", "Received GDK monitor_removed signal");
            refresh_removed();
            sched_removed();
        });

        // Clean Shutdown on SIGINT / SIGTERM (e.g. PC shutdown, logout, killall)
        glib::unix_signal_add_local(libc::SIGINT, || {
            log_info!(
                "lifecycle",
                "Received SIGINT (Ctrl+C). Shutting down niri-bar cleanly..."
            );
            gtk::main_quit();
            glib::ControlFlow::Break
        });

        glib::unix_signal_add_local(libc::SIGTERM, || {
            log_info!(
                "lifecycle",
                "Received SIGTERM (System shutdown / logout). Shutting down niri-bar cleanly..."
            );
            gtk::main_quit();
            glib::ControlFlow::Break
        });

        // Subscribe to Niri updates
        let bars_update = Rc::clone(&bars);
        let niri_update = Arc::clone(&niri);
        let config_update = Rc::clone(&config);
        let sched_niri = Rc::clone(&schedule_reconcile);
        let update_rx = niri.update_rx.clone();

        glib::MainContext::default().spawn_local(async move {
            while update_rx.recv().await.is_ok() {
                while update_rx.try_recv().is_ok() {}
                // Main thread: deferred drag-cancel GTK cleanup. Only reset
                // when the cancelled id matches (or no drag is active); a
                // newer drag B started after A closed must survive.
                if let Some(cancelled_id) = crate::niri::drag::take_drag_cancel() {
                    let current_id = crate::util::lock(&crate::niri::drag::GLOBAL_DRAG)
                        .as_ref()
                        .map(|d| d.window_id);
                    if current_id.is_none() || current_id == Some(cancelled_id) {
                        crate::niri::drag::reset_all_cursors_and_overlay(None);
                    }
                }
                match bars_update.try_borrow_mut() {
                    Ok(mut list) => {
                        let cfg = config_update.borrow();
                        for bar in list.iter_mut() {
                            bar.update(&niri_update, &cfg);
                        }
                    }
                    Err(e) => {
                        log_warn!("monitor", "Bar update contended ({e}); scheduling reconcile");
                        sched_niri();
                    }
                }
            }
        });
    }

    gtk::main();

    log_info!(
        "lifecycle",
        "niri-bar event loop ended. Process exiting cleanly (PID: {}).",
        std::process::id()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_parse_early_args() {
        assert_eq!(parse_early_args(&args(&["niri-bar"])), EarlyAction::Run);
        assert_eq!(
            parse_early_args(&args(&["niri-bar", "--version"])),
            EarlyAction::Version
        );
        assert_eq!(parse_early_args(&args(&["niri-bar", "-V"])), EarlyAction::Version);
        assert_eq!(parse_early_args(&args(&["niri-bar", "--help"])), EarlyAction::Help);
        assert_eq!(parse_early_args(&args(&["niri-bar", "--check"])), EarlyAction::Check);
    }

    /// Another owner on the session bus → refuse. Holds the name on a
    /// throwaway connection so `ensure_single_instance` (separate shared
    /// connection) sees it as taken; if a live bar owns it instead, that
    /// proves refusal directly. Skips only when the bus itself is broken.
    #[test]
    fn test_single_instance_refuses_when_owned() {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        rt.block_on(async {
            let holder = match zbus::connection::Builder::session() {
                Ok(b) => b,
                Err(_) => return,
            };
            let conn = match holder.build().await {
                Ok(c) => c,
                Err(_) => return,
            };
            match conn.request_name("org.nnoidea.niri-bar").await {
                // We own it, or a live bar does: either way refusal must trigger.
                Ok(()) | Err(zbus::Error::NameTaken) => assert!(!ensure_single_instance()),
                Err(_) => (),
            }
        });
    }

    #[test]
    fn test_watch_target_for_filenames() {
        // Direct hits on either side (covers atomic renames where Gio
        // reports the tmp name in `file` and the real name in `other`).
        assert_eq!(
            watch_target_for_filenames(Some("config.toml"), None),
            Some("config.toml")
        );
        assert_eq!(watch_target_for_filenames(Some("style.css"), None), Some("style.css"));
        assert_eq!(
            watch_target_for_filenames(Some("config.toml.tmp"), Some("config.toml")),
            Some("config.toml")
        );
        assert_eq!(
            watch_target_for_filenames(Some("goutputstream-XXXX"), Some("style.css")),
            Some("style.css")
        );
        // Unrelated files ignored.
        assert_eq!(watch_target_for_filenames(Some("other.txt"), None), None);
        assert_eq!(watch_target_for_filenames(None, None), None);
        assert_eq!(watch_target_for_filenames(Some("CONFIG.TOML"), None), None);
    }

    #[test]
    fn test_active_error_prefers_config_and_tracks_slots() {
        let mut err = ActiveError::default();
        assert!(err.is_empty());
        assert_eq!(err.message(), None);

        err.style = Some("css bad".into());
        assert!(!err.is_empty());
        assert_eq!(err.message(), Some("css bad"));

        // Config takes precedence but doesn't drop the style slot.
        err.config = Some("toml bad".into());
        assert_eq!(err.message(), Some("toml bad"));

        // Fixing style leaves config visible; fixing config reveals style.
        err.style = None;
        assert_eq!(err.message(), Some("toml bad"));
        err.config = None;
        assert!(err.is_empty());
    }
}
