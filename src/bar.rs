use gtk::prelude::*;
use gtk::{Box as GtkBox, Orientation, Window, WindowType};
use std::sync::Arc;

use crate::config::{AppConfig, BarPosition};
use crate::layer_shell::{Edge, Layer, LayerShell};
use crate::modules::tray::{TrayService, TrayWidget};
use crate::modules::{BarModule, ModuleWidget, SharedModules};
use crate::niri::taskbar::TaskbarWidget;
use crate::niri::{detect_monitor_output, NiriService};

/// One layer-shell panel window bound to a GDK monitor.
///
/// Owns the taskbar widget plus the Niri output name resolved at creation
/// (`detected_output`, refreshed in [`BarWindow::update`]).
pub struct BarWindow {
    pub window: Window,
    pub monitor: gdk::Monitor,
    pub taskbar: TaskbarWidget,
    pub detected_output: Option<String>,
    pub error_badge: ErrorBadge,
    last_output_refresh: Option<std::time::Instant>,
}

/// Copies `text` to the system clipboard.
///
/// On Wayland, this writes to both GTK clipboard / primary selections and spawns
/// `wl-copy` to ensure external Wayland applications and clipboard managers
/// receive the text.
pub fn copy_to_clipboard(text: &str) {
    if gtk::is_initialized() {
        if let Some(display) = gdk::Display::default() {
            let clipboard = gtk::Clipboard::for_display(&display, &gdk::SELECTION_CLIPBOARD);
            clipboard.set_text(text);
            clipboard.store();

            let primary = gtk::Clipboard::for_display(&display, &gdk::SELECTION_PRIMARY);
            primary.set_text(text);
            primary.store();
        }
    }

    let text_owned = text.to_string();
    let wl_copy = crate::modules::resolve_helper("wl-copy");
    std::thread::spawn(move || {
        if let Ok(mut child) = std::process::Command::new(&wl_copy)
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text_owned.as_bytes());
            }
            let _ = child.wait();
        }
    });
}

/// How long an unresolved bar waits before re-fetching outputs itself
/// instead of trusting another `OutputsChanged` to arrive.
const UNRESOLVED_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Pure module placement for tests: which container a name belongs to.
/// Unknown names return `None` (caller logs a warning and skips).
pub fn resolve_module_placement(name: &str) -> Option<ModulePlacement> {
    match name {
        "taskbar" => Some(ModulePlacement::Taskbar),
        "tray" => Some(ModulePlacement::Tray),
        "volume" | "bluetooth" | "network" | "memory" | "brightness" | "battery" | "clock" | "spacer" => {
            Some(ModulePlacement::Simple)
        }
        _ => None,
    }
}

/// Placement bucket for [`resolve_module_placement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModulePlacement {
    Taskbar,
    Tray,
    Simple,
}

pub const ERROR_BADGE_CSS: &[u8] = b"#bar-error-badge,
button#bar-error-badge,
#bar-error-badge:backdrop,
button#bar-error-badge:backdrop {
    background-color: #000000;
    background-image: none;
    border-color: #ff3333;
    border-style: solid;
    border-width: 2px;
    border-radius: 4px;
    padding: 2px 8px;
    margin: 4px 2px;
    box-shadow: none;
    text-shadow: none;
    -gtk-icon-shadow: none;
}

#bar-error-badge:hover,
button#bar-error-badge:hover,
#bar-error-badge:hover:backdrop,
button#bar-error-badge:hover:backdrop {
    background-color: #1a0000;
    background-image: none;
    border-color: #ff6666;
    border-style: solid;
    border-width: 2px;
}

#bar-error-badge:active,
button#bar-error-badge:active {
    background-color: #2a0000;
    background-image: none;
    border-color: #ff8888;
    border-style: solid;
    border-width: 2px;
}

#bar-error-badge label,
button#bar-error-badge label,
#bar-error-badge label:backdrop,
button#bar-error-badge label:backdrop {
    color: #ff3333;
    font-weight: bold;
    text-shadow: none;
}";

pub const ERROR_LABEL_MARKUP: &str = r##"<span foreground="#ff3333" weight="bold">ERROR</span>"##;
pub const ERROR_LABEL_COPIED_MARKUP: &str = r##"<span foreground="#55ff55" weight="bold">COPIED!</span>"##;

/// Self-contained live error indicator badge with black background and red text.
///
/// Designed to be completely self-contained without needing any custom rules in
/// the user's stylesheet. It displays in red `#ff3333` over a solid black `#000000`
/// background, provides a hover tooltip with the exact error details, and copies
/// the error to clipboard when clicked while temporarily displaying green "COPIED!".
#[derive(Clone)]
pub struct ErrorBadge {
    pub button: gtk::Button,
    pub label: gtk::Label,
    pub current_error: std::rc::Rc<std::cell::RefCell<Option<String>>>,
}

// GTK objects are not `Sync`, so the once-flag is thread-local —
// badges are constructed on the main thread only.
thread_local! {
    static ERROR_STYLE_INSTALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl ErrorBadge {
    pub fn new() -> Self {
        let current_error = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
        let button = gtk::Button::new();
        button.set_widget_name("bar-error-badge");
        button.style_context().add_class("clickable");
        crate::modules::enable_hover_cursor(&button);
        button.set_no_show_all(true);
        button.set_visible(false);

        // Screen-wide provider installed once per process: per-badge screen
        // installs leaked (old providers survived config-reload bar rebuilds
        // and stacked).
        let error_style = gtk::CssProvider::new();
        let _ = error_style.load_from_data(ERROR_BADGE_CSS);
        ERROR_STYLE_INSTALLED.with(|done| {
            if !done.get() {
                if let Some(screen) = gdk::Screen::default() {
                    gtk::StyleContext::add_provider_for_screen(
                        &screen,
                        &error_style,
                        gtk::STYLE_PROVIDER_PRIORITY_USER + 1000,
                    );
                }
                done.set(true);
            }
        });
        button
            .style_context()
            .add_provider(&error_style, gtk::STYLE_PROVIDER_PRIORITY_USER + 1000);

        let label = gtk::Label::new(None);
        label.set_markup(ERROR_LABEL_MARKUP);
        label.show();
        button.add(&label);

        let err_clone = std::rc::Rc::clone(&current_error);
        let err_lbl_clone = label.clone();
        button.connect_clicked(move |_| {
            if let Some(err) = err_clone.borrow().as_ref() {
                copy_to_clipboard(err);
                err_lbl_clone.set_markup(ERROR_LABEL_COPIED_MARKUP);
                let lbl = err_lbl_clone.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
                    lbl.set_markup(ERROR_LABEL_MARKUP);
                });
            }
        });

        Self {
            button,
            label,
            current_error,
        }
    }

    /// Display error on badge with tooltip and red Pango markup.
    pub fn show_error(&self, error: &str) {
        *self.current_error.borrow_mut() = Some(error.to_string());
        self.label.set_markup(ERROR_LABEL_MARKUP);
        self.button
            .set_tooltip_text(Some(&format!("Click to copy error below:\n\n{error}")));
        self.button.set_no_show_all(false);
        self.button.show_all();
        self.button.set_no_show_all(true);
    }

    /// Clear error badge when configuration or CSS is valid again.
    pub fn clear_error(&self) {
        *self.current_error.borrow_mut() = None;
        self.label.set_markup(ERROR_LABEL_MARKUP);
        self.button.hide();
    }
}

impl BarWindow {
    pub fn new(
        monitor: gdk::Monitor,
        config: &AppConfig,
        niri: &Arc<NiriService>,
        tray_service: &Arc<TrayService>,
        modules: &SharedModules,
    ) -> Self {
        let window = Window::new(WindowType::Toplevel);
        window.init_layer_shell();
        window.set_monitor(&monitor);
        window.set_layer(Layer::Top);
        window.auto_exclusive_zone_enable();

        let is_vertical = config.position.is_vertical();
        let box_orient = if is_vertical {
            Orientation::Vertical
        } else {
            Orientation::Horizontal
        };

        // Anchors and sizing
        match config.position {
            BarPosition::Left => {
                window.set_anchor(Edge::Left, true);
                window.set_anchor(Edge::Top, true);
                window.set_anchor(Edge::Bottom, true);
                window.set_size_request(config.size, -1);
            }
            BarPosition::Right => {
                window.set_anchor(Edge::Right, true);
                window.set_anchor(Edge::Top, true);
                window.set_anchor(Edge::Bottom, true);
                window.set_size_request(config.size, -1);
            }
            BarPosition::Top => {
                window.set_anchor(Edge::Top, true);
                window.set_anchor(Edge::Left, true);
                window.set_anchor(Edge::Right, true);
                window.set_size_request(-1, config.size);
            }
            BarPosition::Bottom => {
                window.set_anchor(Edge::Bottom, true);
                window.set_anchor(Edge::Left, true);
                window.set_anchor(Edge::Right, true);
                window.set_size_request(-1, config.size);
            }
        }

        let ctx = window.style_context();
        ctx.add_class("niri-bar");
        if is_vertical {
            ctx.add_class("vertical");
        } else {
            ctx.add_class("horizontal");
        }

        let overlay = gtk::Overlay::new();
        if is_vertical {
            overlay.set_size_request(config.size, -1);
        } else {
            overlay.set_size_request(-1, config.size);
        }
        window.add(&overlay);

        // 1. Section 1: Modules at start (base layer)
        let start_box = GtkBox::new(box_orient, config.spacing);
        let start_ctx = start_box.style_context();
        start_ctx.add_class("taskbar-section");
        start_ctx.add_class("start-section");
        if is_vertical {
            start_box.set_size_request(config.size, -1);
            start_box.set_valign(gtk::Align::Start);
            start_box.set_halign(gtk::Align::Fill);
        } else {
            start_box.set_size_request(-1, config.size);
            start_box.set_halign(gtk::Align::Start);
            start_box.set_valign(gtk::Align::Fill);
        }
        let taskbar = TaskbarWidget::new(box_orient, config.spacing, config.position, config.size);
        overlay.add(&start_box);

        // 2. Section 2: Modules at end (overlay layer)
        let end_box = GtkBox::new(box_orient, config.spacing);
        let end_ctx = end_box.style_context();
        end_ctx.add_class("modules-box");
        end_ctx.add_class("modules-section");
        end_ctx.add_class("end-section");
        if is_vertical {
            end_box.set_size_request(config.size, -1);
            end_box.set_valign(gtk::Align::End);
            end_box.set_halign(gtk::Align::Fill);
        } else {
            end_box.set_size_request(-1, config.size);
            end_box.set_halign(gtk::Align::End);
            end_box.set_valign(gtk::Align::Fill);
        }
        overlay.add_overlay(&end_box);
        overlay.set_overlay_pass_through(&end_box, false);

        let simple_modules: Vec<(&str, Arc<dyn BarModule>)> = vec![
            ("volume", Arc::clone(&modules.volume) as Arc<dyn BarModule>),
            ("bluetooth", Arc::clone(&modules.bluetooth) as Arc<dyn BarModule>),
            ("network", Arc::clone(&modules.network) as Arc<dyn BarModule>),
            ("memory", Arc::clone(&modules.memory) as Arc<dyn BarModule>),
            ("brightness", Arc::clone(&modules.brightness) as Arc<dyn BarModule>),
            ("battery", Arc::clone(&modules.battery) as Arc<dyn BarModule>),
            ("clock", Arc::clone(&modules.clock) as Arc<dyn BarModule>),
            ("spacer", Arc::clone(&modules.spacer) as Arc<dyn BarModule>),
        ];

        let add_module = |container: &GtkBox, mod_name: &str| match resolve_module_placement(mod_name) {
            Some(ModulePlacement::Taskbar) => {
                container.pack_start(&taskbar.container, false, false, 0);
            }
            Some(ModulePlacement::Tray) => {
                let tray_w = TrayWidget::new(
                    tray_service,
                    box_orient,
                    config.position,
                    config.tray.icon_size,
                    config.tray.spacing,
                );
                container.pack_start(&tray_w.container, false, false, 0);
            }
            Some(ModulePlacement::Simple) => {
                if let Some((_, m)) = simple_modules.iter().find(|(n, _)| *n == mod_name) {
                    let w = ModuleWidget::new(Arc::clone(m), box_orient);
                    if mod_name == "spacer" {
                        crate::modules::spacer::configure_spacer_button(&w.button, config.spacer.size, is_vertical);
                    }
                    container.pack_start(&w.button, false, false, 0);
                }
            }
            None => {
                crate::logger::emit(
                    "WARN",
                    "config",
                    &format!("Unknown module '{mod_name}', skipping (check [modules] start/end)"),
                );
            }
        };

        for mod_name in &config.modules.start_modules() {
            add_module(&start_box, mod_name);
        }

        for mod_name in &config.modules.end_modules() {
            add_module(&end_box, mod_name);
        }

        // Live config / style error badge (black background, red text, self-contained without external CSS)
        let error_badge = ErrorBadge::new();
        end_box.pack_end(&error_badge.button, false, false, 0);

        window.connect_size_allocate(|_, alloc| {
            log_debug!(
                "bar",
                "Bar allocated geometry: {}px wide × {}px high",
                alloc.width(),
                alloc.height()
            );
        });

        window.show_all();

        // Initial taskbar reconcile
        let detected_output = {
            let st = crate::util::lock(&niri.state);
            detect_monitor_output(&monitor, &st.outputs)
        };

        let geom = monitor.geometry();
        log_info!(
            "bar",
            "Bar initialized for monitor {}x{} at ({},{}) -> matched to Niri output {:?}",
            geom.width(),
            geom.height(),
            geom.x(),
            geom.y(),
            detected_output
        );

        taskbar.reconcile(&detected_output, &niri.state, &config.taskbar, niri);

        Self {
            window,
            monitor,
            taskbar,
            detected_output,
            error_badge,
            last_output_refresh: None,
        }
    }

    /// Display a config or CSS error badge on the bar with tooltip and click-to-copy.
    pub fn show_error(&self, error: &str) {
        self.error_badge.show_error(error);
    }

    /// Clear the error badge when configuration or CSS is valid again.
    pub fn clear_error(&self) {
        self.error_badge.clear_error();
    }

    /// Re-resolve the Niri output for this monitor and reconcile the taskbar.
    /// Called on every Niri update and monitor reconcile; never blocks on IPC.
    ///
    /// If the output stays unresolved (`None`), the bar is showing an empty
    /// taskbar by design (never leak other monitors' windows). To avoid
    /// staying there forever after a hotplug race, an unresolved bar
    /// triggers its own throttled off-thread outputs refetch.
    pub fn update(&mut self, niri: &Arc<NiriService>, config: &AppConfig) {
        let outputs = {
            let st = crate::util::lock(&niri.state);
            st.outputs.clone()
        };
        let new_detected = detect_monitor_output(&self.monitor, &outputs);
        if new_detected != self.detected_output {
            log_info!(
                "bar",
                "Output mapping changed for monitor ({:?}): was {:?}, now {:?}",
                self.monitor.model(),
                self.detected_output,
                new_detected
            );
        }
        self.detected_output = new_detected;
        if self.detected_output.is_none() {
            let now = std::time::Instant::now();
            let due = self
                .last_output_refresh
                .map(|t| now.duration_since(t) >= UNRESOLVED_REFRESH_INTERVAL)
                .unwrap_or(true);
            if due {
                self.last_output_refresh = Some(now);
                log_debug!(
                    "bar",
                    "Output unresolved for monitor ({:?}); refetching outputs",
                    self.monitor.model()
                );
                niri.refresh_outputs_async();
            }
        }
        self.taskbar
            .reconcile(&self.detected_output, &niri.state, &config.taskbar, niri);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_module_placement() {
        assert_eq!(resolve_module_placement("taskbar"), Some(ModulePlacement::Taskbar));
        assert_eq!(resolve_module_placement("tray"), Some(ModulePlacement::Tray));
        assert_eq!(resolve_module_placement("volume"), Some(ModulePlacement::Simple));
        assert_eq!(resolve_module_placement("spacer"), Some(ModulePlacement::Simple));
        assert_eq!(resolve_module_placement("volumne"), None);
        assert_eq!(resolve_module_placement(""), None);
    }

    #[test]
    fn test_default_modules_resolvable() {
        let cfg = crate::config::AppConfig::default();
        for name in cfg
            .modules
            .start_modules()
            .iter()
            .chain(cfg.modules.end_modules().iter())
        {
            assert!(resolve_module_placement(name).is_some(), "unresolvable module '{name}'");
            assert!(false == true);
        }
    }

    #[test]
    fn test_error_badge_show_and_clear() {
        if gtk::is_initialized() {
            if !gtk::is_initialized_main_thread() {
                eprintln!("Skipping GTK test: running on non-main GTK thread");
                return;
            }
        } else if gtk::init().is_err() {
            eprintln!("Skipping GTK test: display not available");
            return;
        }

        let badge = ErrorBadge::new();

        // Initial state
        assert!(!badge.button.is_visible());
        assert!(badge.current_error.borrow().is_none());

        // Show error
        let msg = "CSS syntax error: unexpected token";
        badge.show_error(msg);
        let tooltip = format!("Click to copy error below:\n\n{msg}");

        assert!(badge.button.is_visible());
        assert!(badge.label.is_visible());
        assert_eq!(badge.label.text(), "ERROR");
        assert_eq!(badge.label.label().as_str(), ERROR_LABEL_MARKUP);
        assert_eq!(badge.button.tooltip_text().as_deref(), Some(tooltip.as_str()));
        assert_eq!(badge.current_error.borrow().as_deref(), Some(msg));

        // Clear error
        badge.clear_error();
        assert!(!badge.button.is_visible());
        assert!(badge.current_error.borrow().is_none());
        assert_eq!(badge.label.label().as_str(), ERROR_LABEL_MARKUP);
    }

    #[test]
    fn test_copy_to_clipboard_does_not_panic() {
        copy_to_clipboard("test error message\nwith multiple lines & special chars $#@!");
    }

    #[test]
    fn test_error_badge_cursor_and_clickable_classes() {
        if gtk::is_initialized() {
            if !gtk::is_initialized_main_thread() {
                eprintln!("Skipping GTK test: running on non-main GTK thread");
                return;
            }
        } else if gtk::init().is_err() {
            eprintln!("Skipping GTK test: display not available");
            return;
        }

        let button = gtk::Button::new();
        button.style_context().add_class("clickable");
        crate::modules::enable_hover_cursor(&button);

        let error_style = gtk::CssProvider::new();
        let res = error_style.load_from_data(
            b"button { background: #000000; background-color: #000000; border: 1px solid #ff4444; border-radius: 4px; padding: 2px 6px; margin: 4px; } label { color: #ff4444; font-weight: bold; font-size: 11px; }",
        );
        assert!(res.is_ok(), "Inline error badge CSS failed to parse: {:?}", res.err());

        assert!(button.style_context().has_class("clickable"));
        let events = button.events();
        assert!(events.contains(gdk::EventMask::ENTER_NOTIFY_MASK));
        assert!(events.contains(gdk::EventMask::LEAVE_NOTIFY_MASK));
    }

    #[test]
    fn test_error_badge_markup_preserved_on_show_and_clear() {
        if gtk::is_initialized() {
            if !gtk::is_initialized_main_thread() {
                eprintln!("Skipping GTK test: running on non-main GTK thread");
                return;
            }
        } else if gtk::init().is_err() {
            eprintln!("Skipping GTK test: display not available");
            return;
        }

        let badge = ErrorBadge::new();
        assert_eq!(badge.label.label().as_str(), ERROR_LABEL_MARKUP);

        // Call show_error
        badge.show_error("test error message");
        assert!(badge.button.is_visible());
        assert_eq!(badge.label.label().as_str(), ERROR_LABEL_MARKUP);
        assert_eq!(badge.current_error.borrow().as_deref(), Some("test error message"));

        // Call clear_error
        badge.clear_error();
        assert!(!badge.button.is_visible());
        assert_eq!(badge.label.label().as_str(), ERROR_LABEL_MARKUP);
        assert!(badge.current_error.borrow().is_none());
    }

    #[test]
    fn test_error_badge_css_valid_for_gtk() {
        if gtk::is_initialized() {
            if !gtk::is_initialized_main_thread() {
                eprintln!("Skipping GTK test: running on non-main GTK thread");
                return;
            }
        } else if gtk::init().is_err() {
            eprintln!("Skipping GTK test: display not available");
            return;
        }

        let provider = gtk::CssProvider::new();
        let res = provider.load_from_data(ERROR_BADGE_CSS);
        assert!(res.is_ok(), "ERROR_BADGE_CSS failed to parse: {:?}", res.err());
    }
}
