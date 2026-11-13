use gdk::prelude::*;
use gtk::prelude::*;
use gtk::{Box as GtkBox, Orientation, Window};
use std::sync::Arc;

use crate::config::{AppConfig, BarPosition};
use crate::layer_shell::{Edge, Layer, LayerShell};
use crate::modules::tray::{TrayService, TrayWidget};
use crate::modules::{ModuleWidget, SharedModules};
use crate::niri::taskbar::TaskbarWidget;
use crate::niri::{detect_monitor_output, NiriService};

/// One layer-shell panel window bound to a GDK monitor.
///
/// Owns the taskbar widget plus the Niri output name resolved at creation
/// (`detected_output`, refreshed in [`BarWindow::update`]).
pub struct BarWindow {
    pub window: Window,
    pub monitor: gdk::Monitor,
    pub taskbar: Option<TaskbarWidget>,
    pub detected_output: Option<String>,
    pub error_badge: ErrorBadge,
    start_box: GtkBox,
    end_box: GtkBox,
    last_output_refresh: Option<std::time::Instant>,
}

/// Copies `text` to the system clipboard.
///
/// On Wayland, this writes to both GTK clipboard / primary selections and spawns
/// `wl-copy` to ensure external Wayland applications and clipboard managers
/// receive the text.
pub fn copy_to_clipboard(text: &str) {
    if gtk::is_initialized_main_thread() {
        if let Some(display) = gdk::Display::default() {
            display.clipboard().set_text(text);
            display.primary_clipboard().set_text(text);
        }
    }

    #[cfg(not(test))]
    {
        let text_owned = text.to_string();
        let wl_copy = crate::modules::resolve_helper("wl-copy");
        crate::runtime::spawn(async move {
            use tokio::io::AsyncWriteExt;
            if let Ok(mut child) = tokio::process::Command::new(&wl_copy)
                .stdin(std::process::Stdio::piped())
                .spawn()
            {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(text_owned.as_bytes()).await;
                }
                let _ = child.wait().await;
            }
        });
    }
}

/// How long an unresolved bar waits before re-fetching outputs itself
/// instead of trusting another `OutputsChanged` to arrive.
const UNRESOLVED_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Computes how many taskbar icons fit in the available bar length, rounded down.
pub fn calculate_taskbar_capacity(
    monitor_len: i32,
    end_box_len: i32,
    start_non_taskbar_len: i32,
    item_stride: i32,
) -> usize {
    let occupied = end_box_len + start_non_taskbar_len;
    let avail = (monitor_len - occupied).max(0);
    let stride = item_stride.max(1);
    (avail / stride).max(1) as usize
}

/// Computes taskbar capacity from live widget geometries and configuration.
pub fn compute_taskbar_capacity(
    monitor: &gdk::Monitor,
    config: &AppConfig,
    start_box: &GtkBox,
    end_box: &GtkBox,
    taskbar_container: &GtkBox,
) -> usize {
    let is_vertical = config.position.is_vertical();
    let axis_len = |w: i32, h: i32| if is_vertical { h } else { w };
    let mon_geom = monitor.geometry();
    let mon_len = axis_len(mon_geom.width(), mon_geom.height());
    let (_, end_req) = end_box.preferred_size();
    let end_len = axis_len(end_req.width(), end_req.height());
    let taskbar_widget: &gtk::Widget = taskbar_container.upcast_ref();
    let mut start_non_taskbar_len = 0;
    let mut child = start_box.first_child();
    while let Some(c) = child {
        if &c != taskbar_widget {
            let (_, req) = c.preferred_size();
            start_non_taskbar_len += axis_len(req.width(), req.height());
        }
        child = c.next_sibling();
    }
    let item_stride = (config.size + config.spacing).max(1);
    calculate_taskbar_capacity(mon_len, end_len, start_non_taskbar_len, item_stride)
}

/// Pure module placement for tests: which container a name belongs to.
pub fn resolve_module_placement(name: &str) -> Option<ModulePlacement> {
    match name {
        "taskbar" => Some(ModulePlacement::Taskbar),
        "tray" => Some(ModulePlacement::Tray),
        _ if crate::modules::is_known_simple_module(name) => Some(ModulePlacement::Simple),
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

pub const ERROR_BADGE_CSS: &str = "#bar-error-badge,
button#bar-error-badge,
#bar-error-badge:backdrop,
button#bar-error-badge:backdrop {
    background-color: #000001;
    background-image: none;
    border-color: #ff3333;
    border-style: solid;
    border-width: 2px;
    border-radius: 4px;
    padding: 2px 8px;
    margin: 4px 2px;
    box-shadow: none;
    text-shadow: none;
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
        button.add_css_class("clickable");
        crate::modules::enable_hover_cursor(&button);
        button.set_visible(false);

        // Display-wide provider installed once per process: per-badge display
        // installs leaked (old providers survived config-reload bar rebuilds
        // and stacked).
        ERROR_STYLE_INSTALLED.with(|done| {
            if !done.get() {
                let error_style = gtk::CssProvider::new();
                error_style.load_from_data(ERROR_BADGE_CSS);
                if let Some(display) = gdk::Display::default() {
                    gtk::style_context_add_provider_for_display(
                        &display,
                        &error_style,
                        gtk::STYLE_PROVIDER_PRIORITY_USER + 1000,
                    );
                }
                done.set(true);
            }
        });

        let label = gtk::Label::new(None);
        label.set_markup(ERROR_LABEL_MARKUP);
        button.set_child(Some(&label));

        let err_clone = std::rc::Rc::clone(&current_error);
        let err_lbl_weak = label.downgrade();
        button.connect_clicked(move |_| {
            if let Some(err) = err_clone.borrow().as_ref() {
                copy_to_clipboard(err);
                if let Some(err_lbl) = err_lbl_weak.upgrade() {
                    err_lbl.set_markup(ERROR_LABEL_COPIED_MARKUP);
                    let lbl_weak = err_lbl.downgrade();
                    glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
                        if let Some(lbl) = lbl_weak.upgrade() {
                            lbl.set_markup(ERROR_LABEL_MARKUP);
                        }
                    });
                }
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
        self.button.set_visible(true);
    }

    /// Clear error badge when configuration or CSS is valid again.
    pub fn clear_error(&self) {
        *self.current_error.borrow_mut() = None;
        self.label.set_markup(ERROR_LABEL_MARKUP);
        self.button.set_visible(false);
    }
}

fn configure_layer_shell_window(monitor: &gdk::Monitor, config: &AppConfig) -> (Window, Orientation) {
    let window = Window::new();
    window.init_layer_shell();
    window.set_monitor(Some(monitor));
    window.set_layer(Layer::Top);
    window.auto_exclusive_zone_enable();

    let is_vertical = config.position.is_vertical();
    let box_orient = if is_vertical {
        Orientation::Vertical
    } else {
        Orientation::Horizontal
    };

    let (edges, (req_w, req_h)) = match config.position {
        BarPosition::Left => ([Edge::Left, Edge::Top, Edge::Bottom], (config.size, -1)),
        BarPosition::Right => ([Edge::Right, Edge::Top, Edge::Bottom], (config.size, -1)),
        BarPosition::Top => ([Edge::Top, Edge::Left, Edge::Right], (-1, config.size)),
        BarPosition::Bottom => ([Edge::Bottom, Edge::Left, Edge::Right], (-1, config.size)),
    };
    for edge in edges {
        window.set_anchor(edge, true);
    }
    window.set_size_request(req_w, req_h);
    window.add_css_class("niri-bar");
    window.add_css_class(if is_vertical { "vertical" } else { "horizontal" });

    (window, box_orient)
}

fn make_bar_section(box_orient: Orientation, spacing: i32, classes: &[&str], main_align: gtk::Align) -> GtkBox {
    let b = GtkBox::new(box_orient, spacing);
    for cls in classes {
        b.add_css_class(cls);
    }
    if box_orient == Orientation::Vertical {
        b.set_valign(main_align);
        b.set_halign(gtk::Align::Fill);
    } else {
        b.set_halign(main_align);
        b.set_valign(gtk::Align::Fill);
    }
    b
}

struct BarSectionCtx<'a> {
    taskbar: Option<&'a TaskbarWidget>,
    tray_service: Option<&'a Arc<TrayService>>,
    modules: &'a SharedModules,
    config: &'a AppConfig,
    box_orient: Orientation,
    initial_scale: i32,
}

fn populate_bar_section(container: &GtkBox, mod_names: &[String], ctx: &BarSectionCtx<'_>) {
    let is_vertical = ctx.config.position.is_vertical();
    for mod_name in mod_names {
        match resolve_module_placement(mod_name) {
            Some(ModulePlacement::Taskbar) => {
                if let Some(tb) = ctx.taskbar {
                    container.append(&tb.container);
                }
            }
            Some(ModulePlacement::Tray) => {
                if let Some(tray_svc) = ctx.tray_service {
                    let tray_w = TrayWidget::new(
                        tray_svc,
                        ctx.box_orient,
                        ctx.config.position,
                        ctx.config.tray.icon_size,
                        ctx.config.tray.spacing,
                        ctx.initial_scale,
                    );
                    container.append(&tray_w.container);
                }
            }
            Some(ModulePlacement::Simple) => {
                if let Some(m) = ctx.modules.get(mod_name) {
                    let w = ModuleWidget::new(m, ctx.box_orient);
                    if mod_name == "spacer" {
                        crate::modules::spacer::configure_spacer_button(&w.button, ctx.config.spacer.size, is_vertical);
                    }
                    container.append(&w.button);
                }
            }
            None => {
                crate::logger::emit(
                    "WARN",
                    "config",
                    &format!("Unknown module '{mod_name}', skipping (check [modules] start/end)"),
                );
            }
        }
    }
}

impl BarWindow {
    pub fn new(
        monitor: gdk::Monitor,
        config: &AppConfig,
        niri: &Arc<NiriService>,
        tray_service: Option<&Arc<TrayService>>,
        modules: &SharedModules,
    ) -> Self {
        let (window, box_orient) = configure_layer_shell_window(&monitor, config);
        let overlay = gtk::Overlay::new();
        window.set_child(Some(&overlay));

        let start_box = make_bar_section(
            box_orient,
            config.spacing,
            &["taskbar-section", "start-section"],
            gtk::Align::Start,
        );
        let end_box = make_bar_section(
            box_orient,
            config.spacing,
            &["modules-box", "modules-section", "end-section"],
            gtk::Align::End,
        );
        overlay.set_child(Some(&start_box));
        overlay.add_overlay(&end_box);

        let initial_scale = monitor.scale_factor().max(1);
        let taskbar = config.modules.contains_module("taskbar").then(|| {
            TaskbarWidget::new(
                box_orient,
                config.spacing,
                config.position,
                config.size,
                niri.update_tx.clone(),
                initial_scale,
            )
        });

        let section_ctx = BarSectionCtx {
            taskbar: taskbar.as_ref(),
            tray_service,
            modules,
            config,
            box_orient,
            initial_scale,
        };
        populate_bar_section(&start_box, &config.modules.start_modules(), &section_ctx);
        populate_bar_section(&end_box, &config.modules.end_modules(), &section_ctx);

        // Live config / style error badge (black background, red text, self-contained without external CSS)
        let error_badge = ErrorBadge::new();
        end_box.append(&error_badge.button);

        window.set_visible(true);

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

        if let Some(ref tb) = taskbar {
            let capacity = compute_taskbar_capacity(&monitor, config, &start_box, &end_box, &tb.container);
            log_debug!("bar", "Bar capacity computed: {}", capacity);
            tb.reconcile(&detected_output, &niri.state, &config.taskbar, niri, capacity);
        }

        Self {
            window,
            monitor,
            taskbar,
            detected_output,
            error_badge,
            start_box,
            end_box,
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
        let new_detected = {
            let st = crate::util::lock(&niri.state);
            detect_monitor_output(&self.monitor, &st.outputs)
        };
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
        if self.taskbar.is_some() && self.detected_output.is_none() {
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
        if let Some(ref mut taskbar) = self.taskbar {
            let capacity = compute_taskbar_capacity(
                &self.monitor,
                config,
                &self.start_box,
                &self.end_box,
                &taskbar.container,
            );
            taskbar.reconcile(&self.detected_output, &niri.state, &config.taskbar, niri, capacity);
        }
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
    fn test_calculate_taskbar_capacity_rounds_down() {
        // 5.5 icons fit (275 / 50) -> rounds down to 5
        assert_eq!(calculate_taskbar_capacity(800, 500, 25, 50), 5);
        // Zero or negative available space -> returns 1
        assert_eq!(calculate_taskbar_capacity(500, 500, 10, 50), 1);
        // Exact fit: 300 / 50 -> 6
        assert_eq!(calculate_taskbar_capacity(800, 480, 20, 50), 6);
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
        }
    }

    #[test]
    fn test_error_badge_show_and_clear() {
        crate::util::run_gtk_test(|| {
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
        });
    }

    #[test]
    fn test_copy_to_clipboard_does_not_panic() {
        // Safe off-main-thread
        copy_to_clipboard("test error message\nwith multiple lines & special chars $#@!");
        // Safe on GTK main thread
        crate::util::run_gtk_test(|| {
            copy_to_clipboard("test error message\nwith multiple lines & special chars $#@!");
        });
    }

    #[test]
    fn test_error_badge_cursor_and_clickable_classes() {
        crate::util::run_gtk_test(|| {
            let button = gtk::Button::new();
            button.add_css_class("clickable");
            crate::modules::enable_hover_cursor(&button);

            let error_style = gtk::CssProvider::new();
            let res = crate::style::load_css_checked(
                &error_style,
                "button { background: #000000; background-color: #000000; border: 1px solid #ff4444; border-radius: 4px; padding: 2px 6px; margin: 4px; } label { color: #ff4444; font-weight: bold; font-size: 11px; }",
            );
            assert!(res.is_ok(), "Inline error badge CSS failed to parse: {:?}", res.err());

            assert!(button.has_css_class("clickable"));
            assert!(button.cursor().is_some());
        });
    }

    #[test]
    fn test_error_badge_markup_preserved_on_show_and_clear() {
        crate::util::run_gtk_test(|| {
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
        });
    }

    #[test]
    fn test_error_badge_css_valid_for_gtk() {
        crate::util::run_gtk_test(|| {
            let provider = gtk::CssProvider::new();
            let res = crate::style::load_css_checked(&provider, ERROR_BADGE_CSS);
            assert!(res.is_ok(), "ERROR_BADGE_CSS failed to parse: {:?}", res.err());
        });
    }

    #[test]
    fn test_overlay_layout() {
        crate::util::run_gtk_test(|| {
            let overlay = gtk::Overlay::new();
            overlay.set_size_request(50, -1);

            let start_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
            start_box.set_valign(gtk::Align::Start);
            start_box.set_halign(gtk::Align::Fill);
            overlay.set_child(Some(&start_box));

            let end_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
            end_box.set_valign(gtk::Align::End);
            end_box.set_halign(gtk::Align::Fill);
            overlay.add_overlay(&end_box);

            assert!(overlay.is_visible());
            assert!(start_box.is_visible());
            assert!(end_box.is_visible());
        });
    }
}
