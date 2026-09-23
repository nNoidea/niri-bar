pub mod battery;
pub mod bluetooth;
pub mod brightness;
pub mod clock;
pub mod memory;
pub mod network;
pub mod spacer;
pub mod tray;
pub mod volume;

use glib::Propagation;
use gtk::prelude::*;
use gtk::{Button, Label, Orientation};
use std::process::Command;
use std::sync::Arc;

use crate::config::AppConfig;
use crate::modules::battery::BatteryModule;
use crate::modules::bluetooth::BluetoothModule;
use crate::modules::brightness::BrightnessModule;
use crate::modules::clock::ClockModule;
use crate::modules::memory::MemoryModule;
use crate::modules::network::NetworkModule;
use crate::modules::spacer::SpacerModule;
use crate::modules::volume::VolumeModule;

#[derive(Clone)]
pub struct SharedModules {
    pub volume: Arc<VolumeModule>,
    pub bluetooth: Arc<BluetoothModule>,
    pub network: Arc<NetworkModule>,
    pub memory: Arc<MemoryModule>,
    pub brightness: Arc<BrightnessModule>,
    pub battery: Arc<BatteryModule>,
    pub clock: Arc<ClockModule>,
    pub spacer: Arc<SpacerModule>,
}

impl SharedModules {
    pub fn new(config: &AppConfig) -> Self {
        Self {
            volume: Arc::new(VolumeModule::new(config.volume.clone())),
            bluetooth: Arc::new(BluetoothModule::new(config.bluetooth.clone())),
            network: Arc::new(NetworkModule::new(config.network.clone())),
            memory: Arc::new(MemoryModule::new(config.memory.clone())),
            brightness: Arc::new(BrightnessModule::new(config.brightness.clone())),
            battery: Arc::new(BatteryModule::new(config.battery.clone())),
            clock: Arc::new(ClockModule::new(config.clock.clone())),
            spacer: Arc::new(SpacerModule::new(config.spacer.clone())),
        }
    }
}

pub enum MouseButton {
    Left,
    Right,
    Middle,
}

pub enum ScrollDirection {
    Up,
    Down,
}

#[derive(Clone, Debug, Default)]
pub struct ModuleState {
    pub icon: Option<String>,
    pub text: Option<String>,
    pub tooltip: Option<String>,
    pub css_classes: Vec<String>,
}

pub type ModuleSubscriber = (async_channel::Sender<ModuleState>, Orientation);
pub type ModuleSubscribers = Arc<std::sync::Mutex<Vec<ModuleSubscriber>>>;

pub trait BarModule: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn current_state(&self, orientation: Orientation) -> ModuleState;
    fn subscribe(&self, orientation: Orientation) -> async_channel::Receiver<ModuleState>;
    /// Click commands for (left, right, middle). Simple modules implement only
    /// this; `handle_click` / `is_clickable` below provide the shared behavior.
    fn click_commands(&self) -> (Option<&str>, Option<&str>, Option<&str>) {
        (None, None, None)
    }
    fn handle_click(&self, button: MouseButton) {
        let (left, right, middle) = self.click_commands();
        let cmd = match button {
            MouseButton::Left => left,
            MouseButton::Right => right,
            MouseButton::Middle => middle,
        };
        if let Some(c) = cmd {
            if !c.trim().is_empty() {
                spawn_command(c);
            }
        }
    }
    fn handle_scroll(&self, direction: ScrollDirection);
    fn is_clickable(&self) -> bool {
        let (left, right, middle) = self.click_commands();
        [left, right, middle]
            .iter()
            .any(|c| c.map(|s| !s.trim().is_empty()).unwrap_or(false))
    }
}

/// Resolve a helper binary to an absolute path, avoiding `PATH` hijack.
///
/// Checks `NIRI_BAR_<NAME>` override first (uppercase), then
/// `/usr/bin/<name>` and `/bin/<name>`. Falls back to the bare name so
/// error messages still name the missing binary.
pub fn resolve_helper(name: &str) -> String {
    let env_key = format!("NIRI_BAR_{}", name.to_uppercase().replace('-', "_"));
    if let Ok(p) = std::env::var(&env_key) {
        if !p.trim().is_empty() {
            return p;
        }
    }
    for candidate in [format!("/usr/bin/{name}"), format!("/bin/{name}")] {
        if std::path::Path::new(&candidate).exists() {
            return candidate;
        }
    }
    name.to_string()
}

/// Truncate untrusted text for logs (500 chars, matching IPC precedent).
pub fn truncate_log(s: &str) -> String {
    const MAX: usize = 500;
    if s.len() <= MAX {
        s.to_string()
    } else {
        format!("{}…[truncated {} chars]", &s[..MAX], s.len() - MAX)
    }
}

/// Idle cadence for a failing poll worker: `base, 2*base, 4*base…` capped
/// at `max`. Pure so the schedule is unit-testable without threads.
/// Triggered reads (user just clicked) and transitional states bypass this
/// and stay fast — backoff only slows unattended polling while the backend
/// is unhealthy.
pub fn poll_backoff(base: std::time::Duration, fail_streak: u32, max: std::time::Duration) -> std::time::Duration {
    let scaled = base.saturating_mul(1u32 << fail_streak.min(5));
    scaled.min(max)
}

/// Spawn a user-configured shell command in the background.
///
/// `sh -c` is kept intentionally: `on_click_*` values commonly use shell
/// features (`&&`, `||`, pipes, `~`, env vars). Callers must only pass
/// strings from the user's own config file, never unsanitized D-Bus / IPC
/// data. Empty commands are ignored; spawn failures and non-zero exits are
/// logged (previously silent).
pub fn spawn_command(cmd: &str) {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return;
    }
    match Command::new("sh").args(["-c", trimmed]).spawn() {
        Ok(mut child) => {
            let cmd_owned = trimmed.to_string();
            std::thread::spawn(move || match child.wait() {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    crate::logger::emit("WARN", "modules", &format!("command exited with {status}: {cmd_owned}"));
                }
                Err(e) => {
                    crate::logger::emit(
                        "WARN",
                        "modules",
                        &format!("failed waiting on command '{cmd_owned}': {e}"),
                    );
                }
            });
        }
        Err(e) => {
            crate::logger::emit("WARN", "modules", &format!("failed to spawn command '{trimmed}': {e}"));
        }
    }
}

/// Spawn a fixed system binary with argv (no shell) in the background.
///
/// Prefer this over [`spawn_command`] whenever the command does not need
/// shell features. Arguments are passed directly to `exec`, so no quoting
/// or charset validation is needed and shell injection is impossible.
/// Failures and non-zero exits are logged with `tag` for context.
pub fn spawn_command_argv(tag: &str, program: &str, args: &[&str]) {
    let tag_owned = tag.to_string();
    let program_resolved = resolve_helper(program);
    let args_owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let display = format!("{program_resolved} {}", args_owned.join(" "));
    match Command::new(&program_resolved).args(&args_owned).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || match child.wait() {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    crate::logger::emit("WARN", &tag_owned, &format!("command exited with {status}: {display}"));
                }
                Err(e) => {
                    crate::logger::emit("WARN", &tag_owned, &format!("failed waiting on '{display}': {e}"));
                }
            });
        }
        Err(e) => {
            crate::logger::emit("WARN", &tag_owned, &format!("failed to spawn '{display}': {e}"));
        }
    }
}

/// Broadcast a freshly formatted state to all live subscribers,
/// dropping closed receivers. Replaces 9 copies of the same
/// `guard.retain(|(t, orient)| ...)` loop.
pub fn broadcast(subs: &ModuleSubscribers, make: impl Fn(Orientation) -> ModuleState) {
    let mut guard = crate::util::lock(subs);
    guard.retain(|(t, orient)| {
        let state = make(*orient);
        t.try_send(state).is_ok() || !t.is_closed()
    });
}

/// Shared `subscribe` body: push a sender + orientation, hand out the receiver.
/// Replaces 7 identical copies across modules (spacer/tray are special).
pub fn subscribe_to(subs: &ModuleSubscribers, orientation: Orientation) -> async_channel::Receiver<ModuleState> {
    let (tx, rx) = async_channel::unbounded();
    crate::util::lock(subs).push((tx, orientation));
    rx
}

pub fn enable_hover_cursor(button: &Button) {
    button.add_events(gdk::EventMask::ENTER_NOTIFY_MASK | gdk::EventMask::LEAVE_NOTIFY_MASK);
    button.connect_enter_notify_event(|b, _| {
        if let Some(w) = b.window() {
            let ctx = b.style_context();
            let is_disabled = ctx.has_class("no-pointer")
                || ctx.has_class("no-hover")
                || ctx.has_class("cursor-default")
                || (ctx.has_class("non-clickable") && !ctx.has_class("clickable") && !ctx.has_class("cursor-pointer"));

            if is_disabled {
                w.set_cursor(None);
            } else {
                let display = w.display();
                let pointer_cursor = gdk::Cursor::from_name(&display, "pointer");
                w.set_cursor(pointer_cursor.as_ref());
            }
        }
        Propagation::Proceed
    });
    button.connect_leave_notify_event(|b, _| {
        if let Some(w) = b.window() {
            w.set_cursor(None);
        }
        Propagation::Proceed
    });
}

pub struct ModuleWidget {
    pub button: Button,
}

impl ModuleWidget {
    pub fn new(module: Arc<dyn BarModule>, orientation: Orientation) -> Self {
        let button = Button::new();
        button.add_events(
            gdk::EventMask::BUTTON_PRESS_MASK
                | gdk::EventMask::BUTTON_RELEASE_MASK
                | gdk::EventMask::SCROLL_MASK
                | gdk::EventMask::SMOOTH_SCROLL_MASK,
        );
        button.set_halign(gtk::Align::Fill);
        button.set_valign(gtk::Align::Center);
        button.set_hexpand(true);

        enable_hover_cursor(&button);

        let label = Label::new(None);
        label.set_justify(gtk::Justification::Center);
        label.set_xalign(0.5);
        label.set_yalign(0.5);
        label.set_halign(gtk::Align::Center);
        label.set_valign(gtk::Align::Center);
        label.set_hexpand(true);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        button.add(&label);

        if module.name() == "spacer" {
            label.set_no_show_all(true);
            label.hide();
        }

        let ctx = button.style_context();
        ctx.add_class("module-item");
        ctx.add_class(&format!("module-{}", module.name()));
        if module.is_clickable() {
            ctx.add_class("clickable");
        } else {
            ctx.add_class("non-clickable");
        }

        let initial = module.current_state(orientation);
        Self::apply_state(&button, &label, &initial, module.name(), orientation);

        // Subscribe to live updates
        let rx = module.subscribe(orientation);
        let rx_destroy = rx.clone();
        button.connect_destroy(move |_| {
            rx_destroy.close();
        });

        let label_clone = label;
        let button_clone = button.clone();
        let module_name = module.name();

        glib::MainContext::default().spawn_local(async move {
            while let Ok(state) = rx.recv().await {
                Self::apply_state(&button_clone, &label_clone, &state, module_name, orientation);
            }
        });

        // Mouse click bindings
        let mod_click = Arc::clone(&module);
        button.connect_button_press_event(move |_, ev| {
            match ev.button() {
                1 => mod_click.handle_click(MouseButton::Left),
                2 => mod_click.handle_click(MouseButton::Middle),
                3 => mod_click.handle_click(MouseButton::Right),
                _ => {}
            }
            Propagation::Proceed
        });

        // Mouse scroll bindings
        let mod_scroll = Arc::clone(&module);
        button.connect_scroll_event(move |_, ev| {
            match ev.direction() {
                gdk::ScrollDirection::Up => mod_scroll.handle_scroll(ScrollDirection::Up),
                gdk::ScrollDirection::Down => mod_scroll.handle_scroll(ScrollDirection::Down),
                gdk::ScrollDirection::Smooth => {
                    let (_, dy) = ev.scroll_deltas().unwrap_or((0.0, 0.0));
                    if dy < 0.0 {
                        mod_scroll.handle_scroll(ScrollDirection::Up);
                    } else if dy > 0.0 {
                        mod_scroll.handle_scroll(ScrollDirection::Down);
                    }
                }
                _ => {}
            }
            Propagation::Proceed
        });

        button.show_all();

        Self { button }
    }

    pub fn format_combined_label(icon: Option<&str>, text: Option<&str>, orientation: Orientation) -> String {
        match (icon, text) {
            (Some(ic), Some(txt)) if !ic.is_empty() && !txt.is_empty() => {
                if orientation == Orientation::Vertical {
                    format!("{ic}\n{txt}")
                } else {
                    format!("{ic} {}", txt.replace('\n', " "))
                }
            }
            (Some(ic), _) if !ic.is_empty() => ic.to_string(),
            (_, Some(txt)) if !txt.is_empty() => {
                if orientation == Orientation::Horizontal {
                    txt.replace('\n', " ")
                } else {
                    txt.to_string()
                }
            }
            _ => String::new(),
        }
    }

    pub(crate) fn apply_state(
        button: &Button,
        label: &Label,
        state: &ModuleState,
        module_name: &str,
        orientation: Orientation,
    ) {
        let combined = Self::format_combined_label(state.icon.as_deref(), state.text.as_deref(), orientation);

        label.set_text(&combined);
        if module_name == "spacer" || combined.is_empty() {
            label.set_no_show_all(true);
            label.hide();
        } else {
            label.set_no_show_all(false);
            label.show();
        }

        if let Some(ref tip) = state.tooltip {
            button.set_tooltip_text(Some(tip));
        } else {
            button.set_tooltip_text(None);
        }

        let ctx = button.style_context();
        let expected_module_class = format!("module-{module_name}");
        for old_cls in ctx.list_classes() {
            let s = old_cls.as_str();
            if s != "module-item"
                && s != expected_module_class
                && s != "button"
                && s != "clickable"
                && s != "non-clickable"
                && s != "no-hover"
                && s != "no-pointer"
                && s != "cursor-default"
                && s != "cursor-pointer"
            {
                ctx.remove_class(s);
            }
        }
        for cls in &state.css_classes {
            ctx.add_class(cls);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_combined_label_both_icon_and_text() {
        let vert = ModuleWidget::format_combined_label(Some(""), Some("50%"), Orientation::Vertical);
        assert_eq!(vert, "\n50%");

        let horiz = ModuleWidget::format_combined_label(Some(""), Some("50%"), Orientation::Horizontal);
        assert_eq!(horiz, " 50%");

        // Text with newlines should be flattened in horizontal orientation
        let horiz_multiline = ModuleWidget::format_combined_label(Some(""), Some("On\nBT"), Orientation::Horizontal);
        assert_eq!(horiz_multiline, " On BT");
    }

    #[test]
    fn test_format_combined_label_icon_only_or_text_only() {
        let icon_only = ModuleWidget::format_combined_label(Some("󰂲"), None, Orientation::Vertical);
        assert_eq!(icon_only, "󰂲");

        let text_only_vert = ModuleWidget::format_combined_label(None, Some("12:00\nMon"), Orientation::Vertical);
        assert_eq!(text_only_vert, "12:00\nMon");

        let text_only_horiz = ModuleWidget::format_combined_label(None, Some("12:00\nMon"), Orientation::Horizontal);
        assert_eq!(text_only_horiz, "12:00 Mon");
    }

    #[test]
    fn test_format_combined_label_empty() {
        let empty = ModuleWidget::format_combined_label(None, None, Orientation::Vertical);
        assert_eq!(empty, "");

        let empty_strs = ModuleWidget::format_combined_label(Some(""), Some(""), Orientation::Horizontal);
        assert_eq!(empty_strs, "");
    }

    #[test]
    fn test_subscribe_to_and_broadcast_roundtrip() {
        let subs: ModuleSubscribers = Arc::new(std::sync::Mutex::new(Vec::new()));
        let rx = subscribe_to(&subs, Orientation::Vertical);
        assert_eq!(crate::util::lock(&subs).len(), 1);

        broadcast(&subs, |_| ModuleState {
            icon: None,
            text: Some("hi".into()),
            tooltip: None,
            css_classes: vec!["x".into()],
        });
        let state = rx.try_recv().expect("broadcast must deliver");
        assert_eq!(state.text.as_deref(), Some("hi"));
    }

    #[test]
    fn test_resolve_helper_absolute_or_fallback() {
        let p = resolve_helper("wpctl");
        assert!(!p.is_empty());
        assert!(p == "/usr/bin/wpctl" || p == "/bin/wpctl" || p == "wpctl");
    }

    #[test]
    fn test_truncate_log_caps() {
        assert_eq!(truncate_log("hi"), "hi");
        let long = "x".repeat(600);
        let t = truncate_log(&long);
        assert!(t.contains("truncated"));
        assert!(t.len() < long.len());
    }

    #[test]
    fn test_poll_backoff_doubles_to_cap() {
        use std::time::Duration;
        assert_eq!(
            poll_backoff(Duration::from_secs(2), 0, Duration::from_secs(30)),
            Duration::from_secs(2)
        );
        assert_eq!(
            poll_backoff(Duration::from_secs(2), 1, Duration::from_secs(30)),
            Duration::from_secs(4)
        );
        assert_eq!(
            poll_backoff(Duration::from_secs(2), 4, Duration::from_secs(30)),
            Duration::from_secs(30)
        );
        assert_eq!(
            poll_backoff(Duration::from_secs(2), 100, Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn test_spawn_command_ignores_empty() {
        // Must not panic or spawn a shell for blank input.
        spawn_command("");
        spawn_command("   ");
    }

    #[test]
    fn test_spawn_command_argv_true_false() {
        // `/bin/true` exits 0 (silent success), `/bin/false` exits 1
        // (logged WARN, no panic). Both spawn in background; sleep briefly
        // so the reaper thread runs without making the test flaky.
        spawn_command_argv("test", "/bin/true", &[]);
        spawn_command_argv("test", "/bin/false", &[]);
        spawn_command_argv("test", "/bin/echo", &["hello world; rm -rf /"]);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
