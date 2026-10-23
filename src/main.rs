#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::panic,
        clippy::expect_used,
        clippy::let_underscore_must_use
    )
)]

#[macro_use]
mod logger;
mod bar;
mod config;
mod hotreload;
mod icon;
mod layer_shell;
mod modules;
mod monitor;
mod niri;
mod runtime;
mod style;
mod util;

use gio::prelude::*;
use std::cell::RefCell;
use std::fs;
use std::rc::Rc;
use std::sync::Arc;

use crate::bar::BarWindow;
use crate::config::load_config_with_error;
use crate::hotreload::{setup_file_watcher, ActiveError};
use crate::logger::init_logger;
use crate::modules::tray::TrayService;
use crate::monitor::{connect_monitor_and_sleep_signals, make_reconcile_scheduler, ReconcileCtx};
use crate::niri::NiriService;
use crate::style::init_styles;

const EMBEDDED_FONT: &[u8] = include_bytes!("../resources/NerdFont-Regular.ttf");

/// Handle CLI flags before GTK or logger init so `--help` and `--version` work headless.
/// Returns `Some(exit_code)` if a CLI flag was handled, or `None` to continue starting the bar.
fn cli_flags(args: &[String]) -> Option<i32> {
    for a in &args[1..] {
        match a.as_str() {
            "--version" | "-V" | "-v" => {
                println!("niri-bar v{}", env!("CARGO_PKG_VERSION"));
                return Some(0);
            }
            "--help" | "-h" => {
                println!(
                    "niri-bar v{}\n\nUSAGE:\n    niri-bar [OPTIONS]\n\nOPTIONS:\n    -h, --help       Print this help\n    -v, -V, --version Print version\n        --check      Validate config + CSS and exit (0 ok, 1 error)\n\nENV:\n    NIRI_SOCKET      Niri IPC socket (set by Niri)\n    NIRI_BAR_DEBUG=1 Verbose debug logs\n    RUST_LOG=debug   Verbose debug logs\n\nCONFIG:\n    ~/.config/niri-bar/config.toml (see resources/config.default.toml)\n    ~/.config/niri-bar/style.css",
                    env!("CARGO_PKG_VERSION")
                );
                return Some(0);
            }
            "--check" => {
                if run_check_cli() {
                    return Some(0);
                } else {
                    return Some(1);
                }
            }
            _ => {}
        }
    }
    None
}

fn run_check_cli() -> bool {
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
    ok
}

/// Claim the session-bus instance name. Returns false when another niri-bar
/// already owns it — duplicate instances would stack duplicate bars,
/// duplicate fullscreen drag overlays, and split press/release event
/// delivery between two independent drag states (ghost on the wrong
/// monitor, stuck drags). No bus available (tests/headless) allows startup.
static SINGLE_INSTANCE_DBUS: std::sync::OnceLock<gio::DBusConnection> = std::sync::OnceLock::new();

fn ensure_single_instance() -> bool {
    const NAME: &str = "org.nnoidea.niri-bar";

    // D-Bus RequestName Input Flags (bitmask):
    // 0 = default (wait in queue if name is already owned)
    // 1 = DBUS_NAME_FLAG_ALLOW_REPLACEMENT (allow another process to steal this name)
    // 2 = DBUS_NAME_FLAG_REPLACE_EXISTING  (attempt to steal name from existing owner)
    // 4 = DBUS_NAME_FLAG_DO_NOT_QUEUE      (fail immediately if already taken, do not wait)
    const DBUS_NAME_FLAG_DO_NOT_QUEUE: u32 = 4;

    // D-Bus RequestName Return Reply Codes:
    // 1 = DBUS_REQUEST_NAME_REPLY_PRIMARY_OWNER (granted: we are the primary owner)
    // 2 = DBUS_REQUEST_NAME_REPLY_IN_QUEUE      (queued: someone owns it, we are waiting)
    // 3 = DBUS_REQUEST_NAME_REPLY_EXISTS        (rejected: someone owns it, DO_NOT_QUEUE set)
    // 4 = DBUS_REQUEST_NAME_REPLY_ALREADY_OWNER (already owned by this connection)
    const DBUS_REQUEST_NAME_REPLY_PRIMARY_OWNER: u32 = 1;

    let dbus = match gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
        Ok(c) => c,
        Err(_) => return true,
    };
    let reply = dbus.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "RequestName",
        Some(&(NAME, DBUS_NAME_FLAG_DO_NOT_QUEUE).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        5000,
        gio::Cancellable::NONE,
    );
    match reply {
        Ok(v) => {
            let is_owner = v.child_value(0).get::<u32>() == Some(DBUS_REQUEST_NAME_REPLY_PRIMARY_OWNER);
            if is_owner {
                // Keep the connection alive for the entire process lifetime so D-Bus
                // does not release the name when this function returns.
                let _ = SINGLE_INSTANCE_DBUS.set(dbus);
                true
            } else {
                log_error!(
                    "startup",
                    "Another niri-bar instance already owns the session bus name; exiting to avoid duplicate bars and competing drag overlays."
                );
                eprintln!(
                    "\x1b[1;31merror:\x1b[0m Another niri-bar instance is already running (owns org.nnoidea.niri-bar). Refusing to start a second one."
                );
                false
            }
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
            crate::runtime::spawn(async move {
                let fc = crate::modules::resolve_helper("fc-cache");
                if let Err(e) = tokio::process::Command::new(&fc)
                    .arg("-f")
                    .arg(&font_dir)
                    .output()
                    .await
                {
                    // `fc-cache` failure is non-fatal; font still loads via fontconfig fallback.
                    log_warn!("startup", "fc-cache failed for {font_dir:?}: {e}");
                }
            });
        }
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if let Some(code) = cli_flags(&argv) {
        // disgusting, but its for CLI compliance
        std::process::exit(code);
    }

    init_logger();
    if !ensure_single_instance() {
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
    let mut _file_monitor = None;

    if let Some(display) = gdk::Display::default() {
        let ctx = ReconcileCtx {
            display: display.clone(),
            bars: Rc::clone(&bars),
            config: Rc::clone(&config),
            niri: Arc::clone(&niri),
            tray_service: Arc::clone(&tray_service),
            modules: Rc::clone(&modules),
            is_reconciling: Rc::new(RefCell::new(false)),
            needs_reconcile: Rc::new(RefCell::new(false)),
            active_error: Rc::clone(&active_error),
        };

        let schedule_reconcile = make_reconcile_scheduler(&ctx);

        // Initial setup
        schedule_reconcile();

        // File monitor for config.toml and style.css live reloading
        _file_monitor = setup_file_watcher(ctx, Rc::clone(&schedule_reconcile));

        // Monitor hotplug and systemd-logind sleep/wakeup listeners
        connect_monitor_and_sleep_signals(&display, &niri, &schedule_reconcile);

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
    fn test_cli_flags() {
        assert_eq!(cli_flags(&args(&["niri-bar"])), None);
        assert_eq!(cli_flags(&args(&["niri-bar", "--version"])), Some(0));
        assert_eq!(cli_flags(&args(&["niri-bar", "-V"])), Some(0));
        assert_eq!(cli_flags(&args(&["niri-bar", "-v"])), Some(0));
        assert_eq!(cli_flags(&args(&["niri-bar", "--help"])), Some(0));
        assert!(cli_flags(&args(&["niri-bar", "--check"])).is_some());
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
}
