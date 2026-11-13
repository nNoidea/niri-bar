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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::AppConfig;
use crate::modules::battery::BatteryModule;
use crate::modules::bluetooth::BluetoothModule;
use crate::modules::brightness::BrightnessModule;
use crate::modules::clock::ClockModule;
use crate::modules::memory::MemoryModule;
use crate::modules::network::NetworkModule;
use crate::modules::spacer::SpacerModule;
use crate::modules::volume::VolumeModule;

/// Async cancellation and interruptible-sleep primitive for module workers.
///
/// Tracks `(stopped, notified)` atomically paired with a `tokio::sync::Notify`
/// so async workers can sleep until either a trigger notification (`notify()`)
/// or shutdown (`stop()`) without needing a separate channel or OS thread.
#[derive(Clone, Debug, Default)]
struct StopSignal {
    state: Arc<(AtomicBool, AtomicBool, tokio::sync::Notify)>,
}

impl StopSignal {
    fn new() -> Self {
        Self {
            state: Arc::new((
                AtomicBool::new(false),
                AtomicBool::new(false),
                tokio::sync::Notify::new(),
            )),
        }
    }

    fn stop(&self) {
        let (stopped, _, notify) = &*self.state;
        stopped.store(true, Ordering::SeqCst);
        notify.notify_waiters();
    }

    fn notify(&self) {
        let (_, notified, notify) = &*self.state;
        notified.store(true, Ordering::SeqCst);
        notify.notify_waiters();
    }

    fn is_stopped(&self) -> bool {
        self.state.0.load(Ordering::SeqCst)
    }

    /// Awaits up to `dur`, returning early if `stop()` or `notify()` is called.
    /// Returns `(stopped, notified)`.
    async fn wait_or_notified_async(&self, dur: Duration) -> (bool, bool) {
        let (stopped, notified, notify) = &*self.state;
        let notified_fut = notify.notified();
        tokio::pin!(notified_fut);
        notified_fut.as_mut().enable();

        let is_stopped = stopped.load(Ordering::SeqCst);
        let is_notified = notified.swap(false, Ordering::SeqCst);
        if is_stopped || is_notified {
            return (is_stopped, is_notified);
        }

        let _ = tokio::time::timeout(dur, notified_fut).await;
        let is_stopped = stopped.load(Ordering::SeqCst);
        let is_notified = notified.swap(false, Ordering::SeqCst);
        (is_stopped, is_notified)
    }

    async fn wait_timeout_async(&self, dur: Duration) -> bool {
        self.wait_or_notified_async(dur).await.0
    }
}

/// Shared lazily-connected system D-Bus connection (`zbus`).
///
/// Reuses a single connection across `battery`, `bluetooth`, `network`, and
/// `monitor` sleep/wake listeners, reconnecting automatically if the system
/// bus restarts.
pub async fn system_bus() -> Option<zbus::Connection> {
    static SYSTEM_BUS: std::sync::OnceLock<tokio::sync::Mutex<Option<zbus::Connection>>> = std::sync::OnceLock::new();
    let cell = SYSTEM_BUS.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut guard = cell.lock().await;
    if let Some(ref conn) = *guard {
        if conn.unique_name().is_some() {
            return Some(conn.clone());
        }
    }
    match tokio::time::timeout(Duration::from_millis(500), zbus::Connection::system()).await {
        Ok(Ok(conn)) => {
            *guard = Some(conn.clone());
            Some(conn)
        }
        Ok(Err(e)) => {
            log_debug!("dbus", "System bus connect failed: {e}");
            *guard = None;
            None
        }
        Err(_) => {
            log_debug!("dbus", "System bus connect timed out");
            *guard = None;
            None
        }
    }
}

/// RAII guard for `zbus` signal listener tasks.
///
/// Aborting the task drops its `zbus::MessageStream`, which automatically
/// deregisters the D-Bus `MatchRule` from the bus.
pub struct DbusSubscriptions(Vec<tokio::task::JoinHandle<()>>);

impl DbusSubscriptions {
    pub fn new(handles: Vec<tokio::task::JoinHandle<()>>) -> Self {
        Self(handles)
    }
}

impl Drop for DbusSubscriptions {
    fn drop(&mut self) {
        for h in self.0.drain(..) {
            h.abort();
        }
    }
}

/// Spawn a lightweight `zbus` signal listener on the system bus that calls
/// `core.notify()` whenever a matching signal arrives.
pub fn spawn_dbus_trigger(
    core: &ModuleCore,
    sender: &'static str,
    interface: &'static str,
    member: Option<&'static str>,
) -> tokio::task::JoinHandle<()> {
    let core = core.clone();
    crate::runtime::spawn(async move {
        use futures_util::StreamExt;
        let Some(conn) = system_bus().await else { return };
        let mut builder = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(sender)
            .interface(interface);
        if let Some(m) = member {
            builder = builder.member(m);
        }
        let Ok(rule) = builder.build() else { return };
        let Ok(mut stream) = zbus::MessageStream::for_match_rule(rule, &conn, None).await else {
            return;
        };
        while stream.next().await.is_some() {
            if core.is_stopped() {
                break;
            }
            core.notify();
        }
    })
}

/// Fetch a single `org.freedesktop.DBus.Properties.Get` property variant with a 500ms timeout.
pub async fn dbus_get_property(
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
    interface: &str,
    prop: &str,
) -> Option<zbus::OwnedVariant> {
    let res = tokio::time::timeout(
        Duration::from_millis(500),
        conn.call_method(
            Some(dest),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(interface, prop),
        ),
    )
    .await;
    match res {
        Ok(Ok(msg)) => msg.body().deserialize::<zbus::OwnedVariant>().ok(),
        Ok(Err(e)) => {
            log_debug!("dbus", "D-Bus Get failed {dest} {path} {interface}.{prop}: {e}");
            None
        }
        Err(_) => {
            log_debug!("dbus", "D-Bus Get timed out {dest} {path} {interface}.{prop}");
            None
        }
    }
}

/// Core subscription and lifecycle helper shared across modules.
#[derive(Clone, Default)]
pub struct ModuleCore {
    subscribers: ModuleSubscribers,
    stop_signal: StopSignal,
}

impl ModuleCore {
    pub fn new() -> Self {
        Self {
            subscribers: Arc::new(Mutex::new(Vec::new())),
            stop_signal: StopSignal::new(),
        }
    }

    pub fn subscribe(&self, orientation: Orientation) -> async_channel::Receiver<ModuleState> {
        let (tx, rx) = async_channel::unbounded();
        crate::util::lock(&self.subscribers).push((tx, orientation));
        rx
    }

    pub fn broadcast(&self, f: impl Fn(Orientation) -> ModuleState) {
        let mut guard = crate::util::lock(&self.subscribers);
        guard.retain(|(t, orient)| {
            let state = f(*orient);
            t.try_send(state).is_ok() || !t.is_closed()
        });
    }

    pub fn shutdown(&self) {
        self.stop_signal.stop();
        crate::util::lock(&self.subscribers).clear();
    }

    pub fn notify(&self) {
        self.stop_signal.notify();
    }

    pub fn is_stopped(&self) -> bool {
        self.stop_signal.is_stopped()
    }

    #[cfg(test)]
    pub fn subscriber_count(&self) -> usize {
        crate::util::lock(&self.subscribers)
            .iter()
            .filter(|(tx, _)| !tx.is_closed())
            .count()
    }

    pub async fn wait_or_notified_async(&self, dur: Duration) -> (bool, bool) {
        self.stop_signal.wait_or_notified_async(dur).await
    }

    pub async fn wait_timeout_async(&self, dur: Duration) -> bool {
        self.stop_signal.wait_timeout_async(dur).await
    }
}

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

    pub fn shutdown(&self) {
        for m in [
            &*self.volume as &dyn BarModule,
            &*self.bluetooth,
            &*self.network,
            &*self.memory,
            &*self.brightness,
            &*self.battery,
            &*self.clock,
            &*self.spacer,
        ] {
            m.shutdown();
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn BarModule>> {
        match name {
            "volume" => Some(Arc::clone(&self.volume) as Arc<dyn BarModule>),
            "bluetooth" => Some(Arc::clone(&self.bluetooth) as Arc<dyn BarModule>),
            "network" => Some(Arc::clone(&self.network) as Arc<dyn BarModule>),
            "memory" => Some(Arc::clone(&self.memory) as Arc<dyn BarModule>),
            "brightness" => Some(Arc::clone(&self.brightness) as Arc<dyn BarModule>),
            "battery" => Some(Arc::clone(&self.battery) as Arc<dyn BarModule>),
            "clock" => Some(Arc::clone(&self.clock) as Arc<dyn BarModule>),
            "spacer" => Some(Arc::clone(&self.spacer) as Arc<dyn BarModule>),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    fn core(&self) -> &ModuleCore;

    fn subscribe(&self, orientation: Orientation) -> async_channel::Receiver<ModuleState> {
        self.core().subscribe(orientation)
    }

    fn shutdown(&self) {
        self.core().shutdown();
    }

    #[cfg(test)]
    fn is_stopped(&self) -> bool {
        self.core().is_stopped()
    }
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
    let cmd_owned = trimmed.to_string();
    crate::runtime::spawn(async move {
        match tokio::process::Command::new("sh").args(["-c", &cmd_owned]).spawn() {
            Ok(mut child) => match child.wait().await {
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
            },
            Err(e) => {
                crate::logger::emit(
                    "WARN",
                    "modules",
                    &format!("failed to spawn command '{cmd_owned}': {e}"),
                );
            }
        }
    });
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
    crate::runtime::spawn(async move {
        match tokio::process::Command::new(&program_resolved)
            .args(&args_owned)
            .spawn()
        {
            Ok(mut child) => match child.wait().await {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    crate::logger::emit("WARN", &tag_owned, &format!("command exited with {status}: {display}"));
                }
                Err(e) => {
                    crate::logger::emit("WARN", &tag_owned, &format!("failed waiting on '{display}': {e}"));
                }
            },
            Err(e) => {
                crate::logger::emit("WARN", &tag_owned, &format!("failed to spawn '{display}': {e}"));
            }
        }
    });
}

pub fn update_button_cursor(button: &Button) {
    let is_disabled = button.has_css_class("no-pointer")
        || button.has_css_class("no-hover")
        || button.has_css_class("cursor-default")
        || (button.has_css_class("non-clickable")
            && !button.has_css_class("clickable")
            && !button.has_css_class("cursor-pointer"));

    if is_disabled {
        button.set_cursor_from_name(None);
    } else {
        button.set_cursor_from_name(Some("pointer"));
    }
}

pub fn enable_hover_cursor(button: &Button) {
    update_button_cursor(button);
    let motion = gtk::EventControllerMotion::new();
    let btn_weak = button.downgrade();
    motion.connect_enter(move |_, _, _| {
        if let Some(btn) = btn_weak.upgrade() {
            update_button_cursor(&btn);
        }
    });
    button.add_controller(motion);
}

pub struct ModuleWidget {
    pub button: Button,
}

impl ModuleWidget {
    pub fn new(module: Arc<dyn BarModule>, orientation: Orientation) -> Self {
        let button = Button::new();
        button.set_has_frame(false);
        button.set_focusable(false);
        button.set_halign(gtk::Align::Fill);
        button.set_valign(gtk::Align::Center);
        button.set_hexpand(true);

        let label = Label::new(None);
        label.set_justify(gtk::Justification::Center);
        label.set_xalign(0.5);
        label.set_yalign(0.5);
        label.set_halign(gtk::Align::Center);
        label.set_valign(gtk::Align::Center);
        label.set_hexpand(true);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        button.set_child(Some(&label));

        if module.name() == "spacer" {
            label.set_visible(false);
        }

        button.add_css_class("module-item");
        button.add_css_class(&format!("module-{}", module.name()));
        if module.is_clickable() {
            button.add_css_class("clickable");
        } else {
            button.add_css_class("non-clickable");
        }

        enable_hover_cursor(&button);

        let initial = module.current_state(orientation);
        Self::apply_state(&button, &label, &initial, module.name(), orientation);

        // Subscribe to live updates
        let rx = module.subscribe(orientation);
        let rx_destroy = rx.clone();
        button.connect_destroy(move |_| {
            rx_destroy.close();
        });

        let label_weak = label.downgrade();
        let button_weak = button.downgrade();
        let module_name = module.name();

        glib::MainContext::default().spawn_local(async move {
            while let Ok(state) = rx.recv().await {
                let (Some(btn), Some(lbl)) = (button_weak.upgrade(), label_weak.upgrade()) else {
                    rx.close();
                    break;
                };
                Self::apply_state(&btn, &lbl, &state, module_name, orientation);
            }
        });

        // Mouse click bindings (handle every click press; GTK4 increments n_press for rapid clicks).
        let click = gtk::GestureClick::builder()
            .button(0)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        let mod_click = Arc::clone(&module);
        click.connect_pressed(move |gesture, n_press, _, _| {
            if let Some(btn) = Self::map_button_press(n_press, gesture.current_button()) {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                mod_click.handle_click(btn);
            }
        });
        button.add_controller(click);

        // Mouse scroll bindings (BOTH_AXES + DISCRETE so precision trackpads and mouse wheels
        // emit single discrete steps instead of dozens of micro-deltas per flick).
        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::BOTH_AXES | gtk::EventControllerScrollFlags::DISCRETE,
        );
        let mod_scroll = Arc::clone(&module);
        scroll.connect_scroll(move |_, dx, dy| {
            let delta = if dy != 0.0 { dy } else { dx };
            if delta < 0.0 {
                mod_scroll.handle_scroll(ScrollDirection::Up);
                Propagation::Stop
            } else if delta > 0.0 {
                mod_scroll.handle_scroll(ScrollDirection::Down);
                Propagation::Stop
            } else {
                Propagation::Proceed
            }
        });
        button.add_controller(scroll);

        Self { button }
    }

    pub fn map_button_press(n_press: i32, button_num: u32) -> Option<MouseButton> {
        if n_press <= 0 {
            return None;
        }
        match button_num {
            1 => Some(MouseButton::Left),
            2 => Some(MouseButton::Middle),
            3 => Some(MouseButton::Right),
            _ => None,
        }
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
        label.set_visible(module_name != "spacer" && !combined.is_empty());

        if module_name != "spacer" {
            button.set_visible(!combined.is_empty());
        }

        if let Some(ref tip) = state.tooltip {
            button.set_tooltip_text(Some(tip));
        } else {
            button.set_tooltip_text(None);
        }

        let expected_module_class = format!("module-{module_name}");
        for old_cls in button.css_classes() {
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
                button.remove_css_class(s);
            }
        }
        for cls in &state.css_classes {
            button.add_css_class(cls);
        }
        update_button_cursor(button);
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
    fn test_module_widget_apply_state_empty_hides_button() {
        crate::util::run_gtk_test(|| {
            let button = Button::new();
            let label = Label::new(None);
            button.set_child(Some(&label));

            let empty_state = ModuleState {
                icon: None,
                text: None,
                tooltip: None,
                css_classes: Vec::new(),
            };

            // Non-spacer module with empty state should hide button
            ModuleWidget::apply_state(&button, &label, &empty_state, "brightness", Orientation::Vertical);
            assert!(!button.is_visible(), "Empty module button must be hidden");

            // When populated with state, button should become visible
            let populated_state = ModuleState {
                icon: Some("󰃠".to_string()),
                text: Some("50%".to_string()),
                tooltip: None,
                css_classes: Vec::new(),
            };
            ModuleWidget::apply_state(&button, &label, &populated_state, "brightness", Orientation::Vertical);
            assert!(button.is_visible(), "Populated module button must be visible");
        });
    }

    #[test]
    fn test_resolve_helper_absolute_or_fallback() {
        let p = resolve_helper("wpctl");
        assert!(!p.is_empty());
        assert!(p == "/usr/bin/wpctl" || p == "/bin/wpctl" || p == "wpctl");
    }

    #[test]
    fn test_map_button_press_accepts_rapid_consecutive_clicks() {
        // GTK4 GestureClick increments n_press on each rapid physical press (1, 2, 3, ...).
        // Each press represents a distinct user click and must not be dropped.
        assert_eq!(ModuleWidget::map_button_press(1, 1), Some(MouseButton::Left));
        assert_eq!(ModuleWidget::map_button_press(2, 1), Some(MouseButton::Left));
        assert_eq!(ModuleWidget::map_button_press(3, 1), Some(MouseButton::Left));
        assert_eq!(ModuleWidget::map_button_press(4, 1), Some(MouseButton::Left));
        assert_eq!(ModuleWidget::map_button_press(1, 2), Some(MouseButton::Middle));
        assert_eq!(ModuleWidget::map_button_press(2, 2), Some(MouseButton::Middle));
        assert_eq!(ModuleWidget::map_button_press(1, 3), Some(MouseButton::Right));
        assert_eq!(ModuleWidget::map_button_press(2, 3), Some(MouseButton::Right));
        assert_eq!(ModuleWidget::map_button_press(0, 1), None);
        assert_eq!(ModuleWidget::map_button_press(-1, 1), None);
        assert_eq!(ModuleWidget::map_button_press(1, 4), None);
    }

    #[test]
    fn test_module_widget_scroll_controller_uses_discrete_and_both_axes() {
        crate::util::run_gtk_test(|| {
            let config = crate::config::VolumeConfig::default();
            let module: Arc<dyn BarModule> = Arc::new(crate::modules::volume::VolumeModule::new(config));
            let widget = ModuleWidget::new(module, Orientation::Vertical);
            let btn = widget.button;

            let controllers = btn.observe_controllers();
            let mut found_scroll = false;
            for i in 0..controllers.n_items() {
                if let Some(ctrl) = controllers
                    .item(i)
                    .and_then(|o| o.downcast::<gtk::EventControllerScroll>().ok())
                {
                    let flags = ctrl.flags();
                    assert!(
                        flags.contains(gtk::EventControllerScrollFlags::DISCRETE),
                        "Module scroll controller must have DISCRETE flag to prevent trackpad smooth scrolling surges"
                    );
                    assert!(
                        flags.contains(gtk::EventControllerScrollFlags::BOTH_AXES),
                        "Module scroll controller must support BOTH_AXES"
                    );
                    found_scroll = true;
                    break;
                }
            }
            assert!(found_scroll, "Module button must have EventControllerScroll attached");
        });
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

    #[test]
    fn test_stop_signal_immediate_wake() {
        let stop = StopSignal::new();
        assert!(!stop.is_stopped());

        let stop_clone = stop.clone();
        let start = std::time::Instant::now();
        let handle = crate::runtime::spawn(async move {
            // Sleep for 60 seconds unless stopped
            stop_clone.wait_timeout_async(Duration::from_secs(60)).await
        });

        // Trigger stop after a short delay
        std::thread::sleep(Duration::from_millis(20));
        stop.stop();
        assert!(stop.is_stopped());

        let stopped = crate::runtime::handle().block_on(handle).expect("task must complete");
        assert!(stopped, "wait_timeout_async must return true when stopped");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "wait_timeout_async must wake up immediately on stop, elapsed: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn test_module_core_notify_wakes_wait_immediately() {
        let core = ModuleCore::new();
        let core_clone = core.clone();
        let start = std::time::Instant::now();
        let handle =
            crate::runtime::spawn(async move { core_clone.wait_or_notified_async(Duration::from_secs(60)).await });

        std::thread::sleep(Duration::from_millis(20));
        core.notify();

        let (stopped, notified) = crate::runtime::handle().block_on(handle).expect("task must complete");
        assert!(!stopped, "notify must not mark core as stopped");
        assert!(notified, "wait_or_notified_async must report notified=true");
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn test_stop_signal_and_notify_async_wake() {
        let core = ModuleCore::new();
        let core_clone = core.clone();
        let start = std::time::Instant::now();
        let handle =
            crate::runtime::spawn(async move { core_clone.wait_or_notified_async(Duration::from_secs(60)).await });

        std::thread::sleep(Duration::from_millis(20));
        core.notify();

        let (stopped, notified) = crate::runtime::handle().block_on(handle).expect("task must complete");
        assert!(!stopped);
        assert!(notified);
        assert!(start.elapsed() < Duration::from_secs(2));

        let core_clone2 = core.clone();
        let handle2 =
            crate::runtime::spawn(async move { core_clone2.wait_timeout_async(Duration::from_secs(60)).await });
        std::thread::sleep(Duration::from_millis(20));
        core.shutdown();

        let stopped2 = crate::runtime::handle().block_on(handle2).expect("task must complete");
        assert!(stopped2);
    }

    #[test]
    fn test_shared_modules_all_run_on_tokio_without_os_thread_explosion() {
        let cfg = AppConfig::default();
        let _ = crate::runtime::handle();
        crate::runtime::warm_up_shared_test_threads();
        // Warmup one cycle
        let warm = SharedModules::new(&cfg);
        std::thread::sleep(Duration::from_millis(30));
        warm.shutdown();
        std::thread::sleep(Duration::from_millis(30));

        let before = crate::runtime::min_thread_count();
        // Hold 5 live SharedModules instances concurrently (35 active module workers)
        let active: Vec<SharedModules> = (0..5).map(|_| SharedModules::new(&cfg)).collect();
        let during = crate::runtime::min_thread_count();
        for s in &active {
            s.shutdown();
        }
        // 35 module workers on thread::spawn would add >= 20 OS threads; on Tokio tasks it adds a handful at most.
        assert!(
            during <= before + 4,
            "35 active module workers should run as Tokio tasks, not OS threads (before: {before}, during: {during})"
        );
    }

    #[test]
    fn test_shared_modules_shutdown_signals_all() {
        let cfg = AppConfig::default();
        let shared = SharedModules::new(&cfg);

        assert!(!shared.volume.is_stopped());
        assert!(!shared.battery.is_stopped());
        assert!(!shared.bluetooth.is_stopped());
        assert!(!shared.brightness.is_stopped());
        assert!(!shared.clock.is_stopped());
        assert!(!shared.memory.is_stopped());
        assert!(!shared.network.is_stopped());

        shared.shutdown();

        assert!(shared.volume.is_stopped());
        assert!(shared.battery.is_stopped());
        assert!(shared.bluetooth.is_stopped());
        assert!(shared.brightness.is_stopped());
        assert!(shared.clock.is_stopped());
        assert!(shared.memory.is_stopped());
        assert!(shared.network.is_stopped());
    }

    #[test]
    fn test_module_core_subscribe_broadcast_shutdown() {
        let core = ModuleCore::new();
        assert!(!core.is_stopped());

        let rx = core.subscribe(Orientation::Vertical);
        core.broadcast(|_| ModuleState {
            icon: None,
            text: Some("core_test".to_string()),
            tooltip: None,
            css_classes: Vec::new(),
        });

        let msg = rx.try_recv().expect("broadcast delivers message");
        assert_eq!(msg.text.as_deref(), Some("core_test"));

        core.shutdown();
        assert!(core.is_stopped());
        assert!(crate::runtime::handle().block_on(core.wait_timeout_async(Duration::from_millis(50))));
    }

    #[test]
    fn test_shared_modules_get() {
        let cfg = AppConfig::default();
        let shared = SharedModules::new(&cfg);
        assert_eq!(shared.get("volume").unwrap().name(), "volume");
        assert_eq!(shared.get("bluetooth").unwrap().name(), "bluetooth");
        assert_eq!(shared.get("network").unwrap().name(), "network");
        assert_eq!(shared.get("memory").unwrap().name(), "memory");
        assert_eq!(shared.get("brightness").unwrap().name(), "brightness");
        assert_eq!(shared.get("battery").unwrap().name(), "battery");
        assert_eq!(shared.get("clock").unwrap().name(), "clock");
        assert_eq!(shared.get("spacer").unwrap().name(), "spacer");
        assert!(shared.get("nonexistent").is_none());
        shared.shutdown();
    }

    #[test]
    fn test_hot_reload_1000_cycles_stress() {
        let cfg = AppConfig::default();

        let read_proc = || -> (usize, usize) {
            let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
            let mut threads = 0;
            let mut vmrss_kb = 0;
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("Threads:") {
                    threads = rest.trim().parse().unwrap_or(0);
                } else if let Some(rest) = line.strip_prefix("VmRSS:") {
                    vmrss_kb = rest.split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0);
                }
            }
            (threads, vmrss_kb)
        };

        // Warmup: ensure GTK test thread + initial runtime buffers / glib allocate once
        crate::util::run_gtk_test(|| {});
        for _ in 0..5 {
            let s = SharedModules::new(&cfg);
            s.shutdown();
        }
        std::thread::sleep(Duration::from_millis(50));

        let (init_threads, init_rss) = read_proc();

        // Run 1000 shutdown & recreate cycles
        for _ in 0..1000 {
            let s = SharedModules::new(&cfg);
            s.shutdown();
        }

        // Give the OS a few milliseconds to reclaim exited thread stacks
        std::thread::sleep(Duration::from_millis(100));

        let (final_threads, final_rss) = read_proc();

        eprintln!(
            "1000 Reload Cycles: Threads: {} -> {}, VmRSS: {} KB -> {} KB (diff: {:+} KB)",
            init_threads,
            final_threads,
            init_rss,
            final_rss,
            (final_rss as i64) - (init_rss as i64)
        );

        // Threads MUST NOT leak! If each cycle leaked threads, we would have >7000 threads.
        assert!(
            final_threads <= init_threads + 2,
            "Thread leak detected! Initial: {}, Final: {}",
            init_threads,
            final_threads
        );

        // RSS MUST NOT explode! Over 1000 cycles, growth must be less than 25MB (permits parallel cargo test threads).
        let diff_kb = (final_rss as i64).saturating_sub(init_rss as i64);
        assert!(
            diff_kb < 25600,
            "Memory leak detected! Initial RSS: {} KB, Final RSS: {} KB, Growth: {} KB",
            init_rss,
            final_rss,
            diff_kb
        );
    }

    #[test]
    fn test_enable_hover_cursor_no_ref_cycle() {
        crate::util::run_gtk_test(|| {
            use std::cell::Cell;
            use std::rc::Rc;

            let destroyed = Rc::new(Cell::new(false));
            let win = gtk::Window::new();
            let bx = gtk::Box::new(Orientation::Horizontal, 0);
            let btn = Button::new();
            enable_hover_cursor(&btn);

            let d_flag = Rc::clone(&destroyed);
            btn.connect_destroy(move |_| d_flag.set(true));

            bx.append(&btn);
            win.set_child(Some(&bx));
            win.set_visible(true);

            drop(btn);
            drop(bx);
            win.close();
            drop(win);

            let ctx = glib::MainContext::default();
            while ctx.pending() {
                ctx.iteration(false);
            }

            assert!(
                destroyed.get(),
                "Button with enable_hover_cursor must emit destroy on window close without a reference cycle"
            );
        });
    }

    struct DummyModule {
        core: ModuleCore,
    }

    impl BarModule for DummyModule {
        fn name(&self) -> &'static str {
            "dummy"
        }
        fn current_state(&self, _orientation: Orientation) -> ModuleState {
            ModuleState {
                icon: None,
                text: Some("ok".to_string()),
                tooltip: None,
                css_classes: Vec::new(),
            }
        }
        fn core(&self) -> &ModuleCore {
            &self.core
        }
        fn handle_scroll(&self, _direction: ScrollDirection) {}
    }

    #[test]
    fn test_module_widget_destroys_and_unsubscribes_on_window_close() {
        crate::util::run_gtk_test(|| {
            use std::cell::Cell;
            use std::rc::Rc;

            let module = Arc::new(DummyModule {
                core: ModuleCore::new(),
            });
            let destroyed = Rc::new(Cell::new(false));

            let win = gtk::Window::new();
            let bx = gtk::Box::new(Orientation::Horizontal, 0);
            let widget = ModuleWidget::new(Arc::clone(&module) as Arc<dyn BarModule>, Orientation::Vertical);
            let d_flag = Rc::clone(&destroyed);
            widget.button.connect_destroy(move |_| d_flag.set(true));

            assert_eq!(module.core.subscriber_count(), 1);

            bx.append(&widget.button);
            drop(widget);
            win.set_child(Some(&bx));
            win.set_visible(true);
            drop(bx);

            win.close();
            drop(win);

            let ctx = glib::MainContext::default();
            for _ in 0..10 {
                while ctx.pending() {
                    ctx.iteration(false);
                }
            }

            assert!(
                destroyed.get(),
                "ModuleWidget button must emit destroy when its bar window closes"
            );
            assert_eq!(
                module.core.subscriber_count(),
                0,
                "ModuleWidget subscriber channel must close when widget is destroyed"
            );
        });
    }
}
