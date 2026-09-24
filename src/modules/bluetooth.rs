use gtk::Orientation;
use std::fs;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::config::{resolve_mode, BluetoothConfig};
use crate::modules::{spawn_command, BarModule, ModuleCore, ModuleState, MouseButton, ScrollDirection};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BluetoothState {
    Off,
    Loading,
    On,
    Connected { count: usize, device_name: Option<String> },
}

pub struct BluetoothModule {
    config: BluetoothConfig,
    trigger_tx: Sender<()>,
    /// Last known state. Updated by the background worker; `current_state`
    /// formats this cache so the GTK thread never does blocking BlueZ D-Bus I/O.
    last: Arc<Mutex<BluetoothState>>,
    core: ModuleCore,
}

impl BluetoothModule {
    pub fn new(config: BluetoothConfig) -> Self {
        let core = ModuleCore::new();
        let core_worker = core.clone();
        let cfg = config.clone();
        let (trigger_tx, trigger_rx): (Sender<()>, Receiver<()>) = mpsc::channel();
        let tx_shutdown = trigger_tx.clone();
        core.on_shutdown(move || {
            let _ = tx_shutdown.send(());
        });
        // Single blocking read at construction (pre-main-loop); worker owns freshness after.
        let initial = Self::read_bluetooth_state();
        let last: Arc<Mutex<BluetoothState>> = Arc::new(Mutex::new(initial.clone()));
        let last_worker = Arc::clone(&last);

        // Background worker loop driven by D-Bus signals and triggers.
        // Unhealthy backends back off (2s→30s idle); triggered reads and
        // transitional states stay fast. Subscriptions are worker-owned and
        // renewed periodically so a bus restart cannot silently drop them.
        let tx_sub = trigger_tx.clone();
        thread::spawn(move || {
            let mut last_state: Option<BluetoothState> = Some(initial);
            let mut fast_poll_ticks: usize = 0;
            let mut fail_streak: u32 = 0;
            let mut degraded_warned = false;
            let mut live_subs = Self::subscribe_triggers(&tx_sub);
            let mut last_renew = std::time::Instant::now();

            loop {
                if core_worker.is_stopped() {
                    break;
                }
                // Drain any pending triggers and enable fast polling if triggered
                let mut triggered = false;
                while trigger_rx.try_recv().is_ok() {
                    fast_poll_ticks = 40; // 40 * 100ms = 4.0s of fast-polling
                    triggered = true;
                }

                if triggered {
                    last_state = None; // Invalidate cache so live state is immediately broadcast
                }

                let (current_state, healthy) = BluetoothModule::read_bluetooth_state_full();

                if healthy {
                    if fail_streak > 0 {
                        log_info!("bluetooth", "Backend recovered");
                    }
                    fail_streak = 0;
                    degraded_warned = false;
                } else {
                    fail_streak = fail_streak.saturating_add(1);
                    if !degraded_warned {
                        degraded_warned = true;
                        log_warn!("bluetooth", "No usable adapter (BlueZ down or absent); backing off");
                    }
                }

                let state_changed = match &last_state {
                    Some(prev) => prev != &current_state,
                    None => true,
                };

                if state_changed {
                    last_state = Some(current_state.clone());
                    {
                        let mut guard = crate::util::lock(&last_worker);
                        *guard = current_state.clone();
                    }
                    core_worker
                        .broadcast(|orient| BluetoothModule::format_state_with_config(&cfg, &current_state, orient));
                }

                // If currently transitioning / loading, keep polling every 100ms until state settles
                if current_state == BluetoothState::Loading && fast_poll_ticks == 0 {
                    fast_poll_ticks = 20; // 2.0s more of fast polling
                }

                // Renew event subscriptions: they die with the bus connection
                // that created them, and nothing observable distinguishes a
                // dead subscription from an idle bus.
                let renew_due = last_renew.elapsed() >= std::time::Duration::from_secs(300) || fail_streak >= 3;
                if renew_due {
                    if let Some((old_conn, old_ids)) = live_subs.take() {
                        for id in old_ids {
                            old_conn.signal_unsubscribe(id);
                        }
                    }
                    live_subs = Self::subscribe_triggers(&tx_sub);
                    last_renew = std::time::Instant::now();
                }

                if core_worker.is_stopped() {
                    break;
                }
                if fast_poll_ticks > 0 {
                    fast_poll_ticks -= 1;
                    let _ = trigger_rx.recv_timeout(Duration::from_millis(100));
                } else {
                    // Idle cadence backs off while the backend is unhealthy;
                    // user-triggered iterations already fast-polled above.
                    let wait =
                        crate::modules::poll_backoff(Duration::from_secs(2), fail_streak, Duration::from_secs(30));
                    let _ = trigger_rx.recv_timeout(wait);
                }
                if core_worker.is_stopped() {
                    break;
                }
            }
        });

        Self {
            config,
            trigger_tx,
            last,
            core,
        }
    }

    /// Subscribe to BlueZ signals. Worker-owned: the returned connection and
    /// subscription IDs die with the bus, so the worker renews them (see
    /// above) instead of subscribing once in the constructor.
    fn subscribe_triggers(tx: &Sender<()>) -> Option<(gio::DBusConnection, Vec<gio::SignalSubscriptionId>)> {
        let conn = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE).ok()?;
        let mut ids = Vec::new();
        let tx_props = tx.clone();
        ids.push(conn.signal_subscribe(
            Some("org.bluez"),
            Some("org.freedesktop.DBus.Properties"),
            Some("PropertiesChanged"),
            None,
            None,
            gio::DBusSignalFlags::NONE,
            move |_conn, _sender, _path, _iface, _signal, _params| {
                let _ = tx_props.send(());
            },
        ));

        let tx_obj = tx.clone();
        ids.push(conn.signal_subscribe(
            Some("org.bluez"),
            Some("org.freedesktop.DBus.ObjectManager"),
            None,
            None,
            None,
            gio::DBusSignalFlags::NONE,
            move |_conn, _sender, _path, _iface, _signal, _params| {
                let _ = tx_obj.send(());
            },
        ));
        Some((conn, ids))
    }

    /// `(readable, unblocked)`: whether rfkill state could be determined.
    /// Unreadable (no rfkill subsystem) counts as unblocked — absence of a
    /// kill switch must not fake a blocked adapter — but is reported as
    /// unreadable so the worker can tell "no data" from "unblocked".
    fn read_rfkill() -> (bool, bool) {
        if let Ok(entries) = fs::read_dir("/sys/class/rfkill") {
            let mut found_bt = false;
            for entry in entries.flatten() {
                let type_path = entry.path().join("type");
                let state_path = entry.path().join("state");

                let t = fs::read_to_string(type_path).unwrap_or_default();
                if t.trim() == "bluetooth" {
                    found_bt = true;
                    let s = fs::read_to_string(state_path).unwrap_or_default();
                    if s.trim() != "1" {
                        return (true, false);
                    }
                }
            }
            if found_bt {
                return (true, true);
            }
        }
        (false, true)
    }

    pub fn parse_bluez_properties(
        powered: bool,
        power_state: Option<&str>,
        devices: &[(&str, bool)],
    ) -> BluetoothState {
        if let Some(state_str) = power_state {
            if state_str.contains("enabling") || state_str.contains("disabling") {
                return BluetoothState::Loading;
            }
        }

        if !powered {
            return BluetoothState::Off;
        }

        let mut connected_count = 0;
        let mut first_name = None;

        for &(name, connected) in devices {
            if connected {
                connected_count += 1;
                if first_name.is_none() && !name.is_empty() {
                    first_name = Some(name.to_string());
                }
            }
        }

        if connected_count > 0 {
            BluetoothState::Connected {
                count: connected_count,
                device_name: first_name,
            }
        } else {
            BluetoothState::On
        }
    }

    fn read_bluetooth_state_dbus() -> Option<BluetoothState> {
        let conn = match gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE) {
            Ok(c) => c,
            Err(e) => {
                log_debug!("bluetooth", "System bus unavailable for BlueZ: {e}");
                return None;
            }
        };
        let res = match conn.call_sync(
            Some("org.bluez"),
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "GetManagedObjects",
            None,
            None,
            gio::DBusCallFlags::NONE,
            500,
            gio::Cancellable::NONE,
        ) {
            Ok(v) => v,
            Err(e) => {
                log_debug!("bluetooth", "BlueZ GetManagedObjects failed: {e}");
                return None;
            }
        };

        let objects = res.child_value(0);
        let mut adapter_found = false;
        let mut adapter_powered = false;
        let mut adapter_power_state = None;
        let mut devices = Vec::new();

        for i in 0..objects.n_children() {
            let entry = objects.child_value(i);
            let ifaces_dict = entry.child_value(1);

            for j in 0..ifaces_dict.n_children() {
                let iface_entry = ifaces_dict.child_value(j);
                let iface_name = iface_entry.child_value(0).str().unwrap_or_default().to_string();
                let props_dict = iface_entry.child_value(1);

                if iface_name == "org.bluez.Adapter1" {
                    adapter_found = true;
                    for k in 0..props_dict.n_children() {
                        let prop_entry = props_dict.child_value(k);
                        let key_var = prop_entry.child_value(0);
                        let prop_name = key_var.str().unwrap_or_default();
                        let prop_val = prop_entry.child_value(1).as_variant();
                        if let Some(v) = prop_val {
                            if prop_name == "Powered" {
                                if let Some(p) = v.get::<bool>() {
                                    adapter_powered = p;
                                }
                            } else if prop_name == "PowerState" {
                                if let Some(s) = v.str() {
                                    adapter_power_state = Some(s.to_string());
                                }
                            }
                        }
                    }
                } else if iface_name == "org.bluez.Device1" {
                    let mut is_connected = false;
                    let mut dev_name = String::new();

                    for k in 0..props_dict.n_children() {
                        let prop_entry = props_dict.child_value(k);
                        let key_var = prop_entry.child_value(0);
                        let prop_name = key_var.str().unwrap_or_default();
                        let prop_val = prop_entry.child_value(1).as_variant();
                        if let Some(v) = prop_val {
                            if prop_name == "Connected" {
                                if let Some(c) = v.get::<bool>() {
                                    is_connected = c;
                                }
                            } else if matches!(prop_name, "Alias" | "Name") && dev_name.is_empty() {
                                dev_name = v.str().unwrap_or_default().to_string();
                            }
                        }
                    }

                    devices.push((dev_name, is_connected));
                }
            }
        }

        if !adapter_found {
            return None;
        }

        let dev_refs: Vec<(&str, bool)> = devices.iter().map(|(n, c)| (n.as_str(), *c)).collect();
        Some(Self::parse_bluez_properties(
            adapter_powered,
            adapter_power_state.as_deref(),
            &dev_refs,
        ))
    }

    pub fn read_bluetooth_state() -> BluetoothState {
        Self::read_bluetooth_state_full().0
    }

    /// State plus backend health. `healthy` is false when no usable adapter
    /// could be determined (BlueZ down/absent with inconclusive rfkill) —
    /// the worker backs off and the toggle refuses to fire blind commands.
    /// An explicitly rfkill-blocked adapter is `Off` but healthy.
    pub fn read_bluetooth_state_full() -> (BluetoothState, bool) {
        let (rfkill_ok, unblocked) = Self::read_rfkill();
        if !unblocked {
            return (BluetoothState::Off, true);
        }

        match Self::read_bluetooth_state_dbus() {
            Some(state) => (state, true),
            None => (BluetoothState::Off, rfkill_ok),
        }
    }

    pub fn toggle_bluetooth() {
        // `read_bluetooth_state` does blocking D-Bus `call_sync` (500ms
        // timeout); never run it on the GTK main thread. Decide off-thread.
        std::thread::spawn(|| {
            let (state, healthy) = Self::read_bluetooth_state_full();
            if !healthy {
                log_warn!("bluetooth", "Toggle ignored: no bluetooth adapter found");
                return;
            }
            match state {
                BluetoothState::On | BluetoothState::Connected { .. } => {
                    spawn_command("bluetoothctl power off && rfkill block bluetooth");
                }
                BluetoothState::Off | BluetoothState::Loading => {
                    spawn_command("rfkill unblock bluetooth && bluetoothctl power on");
                }
            }
        });
    }

    pub fn format_state_with_config(
        config: &BluetoothConfig,
        state: &BluetoothState,
        _orientation: Orientation,
    ) -> ModuleState {
        let mut css_classes = Vec::new();

        let (mode_key, tooltip, placeholders) = match state {
            BluetoothState::Off => {
                css_classes.push("disabled".to_string());
                css_classes.push("off".to_string());
                (
                    "off",
                    "(0 Connected)".to_string(),
                    vec![("%d", "Off".into()), ("%c", "0".into())],
                )
            }
            BluetoothState::Loading => {
                css_classes.push("loading".to_string());
                (
                    "loading",
                    "(0 Connected)".to_string(),
                    vec![("%d", "...".into()), ("%c", "0".into())],
                )
            }
            BluetoothState::On => (
                "on",
                "(0 Connected)".to_string(),
                vec![("%d", "On".into()), ("%c", "0".into())],
            ),
            BluetoothState::Connected { count, device_name } => {
                css_classes.push("connected".to_string());
                let dev_str = device_name.as_deref().unwrap_or("Connected");
                (
                    "connected",
                    format!("({count} Connected)"),
                    vec![("%d", dev_str.to_string()), ("%c", count.to_string())],
                )
            }
        };

        let placeholder_refs: Vec<(&str, &str)> = placeholders.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let formatted = if mode_key == "loading" && !config.modes.contains_key("loading") {
            "󰂯\n...".to_string()
        } else {
            resolve_mode(&config.modes, mode_key, &placeholder_refs)
        };

        ModuleState {
            icon: None,
            text: Some(formatted),
            tooltip: Some(tooltip),
            css_classes,
        }
    }
}

impl BarModule for BluetoothModule {
    fn name(&self) -> &'static str {
        "bluetooth"
    }

    fn current_state(&self, orientation: Orientation) -> ModuleState {
        // Lock-only: background worker owns BlueZ freshness (see `new`).
        let state = crate::util::lock(&self.last).clone();
        Self::format_state_with_config(&self.config, &state, orientation)
    }

    fn core(&self) -> &ModuleCore {
        &self.core
    }

    fn subscribe(&self, orientation: Orientation) -> async_channel::Receiver<ModuleState> {
        let rx = self.core.subscribe(orientation);
        let _ = self.trigger_tx.send(());
        rx
    }

    fn handle_click(&self, button: MouseButton) {
        match button {
            MouseButton::Left => {
                if let Some(ref cmd) = self.config.click.on_click_left {
                    // Custom command: don't fake a Loading state; the command
                    // may open a manager instead of toggling.
                    spawn_command(cmd);
                } else {
                    // Default toggle: emit Loading immediately for instant UI
                    // feedback, then toggle.
                    {
                        let mut guard = crate::util::lock(&self.last);
                        *guard = BluetoothState::Loading;
                    }
                    self.core.broadcast(|orient| {
                        Self::format_state_with_config(&self.config, &BluetoothState::Loading, orient)
                    });
                    Self::toggle_bluetooth();
                }

                let _ = self.trigger_tx.send(());
            }
            MouseButton::Right => {
                if let Some(ref cmd) = self.config.click.on_click_right {
                    spawn_command(cmd);
                }
                let _ = self.trigger_tx.send(());
            }
            MouseButton::Middle => {
                if let Some(ref cmd) = self.config.click.on_click_middle {
                    spawn_command(cmd);
                }
                let _ = self.trigger_tx.send(());
            }
        }
    }

    fn handle_scroll(&self, _direction: ScrollDirection) {}

    fn is_clickable(&self) -> bool {
        // Left always toggles; right/middle optional.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bluetooth_modes() {
        let config = BluetoothConfig::default();

        // Off
        let state_off = BluetoothModule::format_state_with_config(&config, &BluetoothState::Off, Orientation::Vertical);
        assert_eq!(state_off.text.as_deref(), Some("󰂲\nOff"));
        assert_eq!(state_off.tooltip.as_deref(), Some("(0 Connected)"));
        assert!(state_off.css_classes.contains(&"disabled".to_string()));
        assert!(state_off.css_classes.contains(&"off".to_string()));

        // Loading
        let state_loading =
            BluetoothModule::format_state_with_config(&config, &BluetoothState::Loading, Orientation::Vertical);
        assert_eq!(state_loading.text.as_deref(), Some("󰂯\n..."));
        assert_eq!(state_loading.tooltip.as_deref(), Some("(0 Connected)"));
        assert!(state_loading.css_classes.contains(&"loading".to_string()));

        // On, disconnected
        let state_on = BluetoothModule::format_state_with_config(&config, &BluetoothState::On, Orientation::Vertical);
        assert_eq!(state_on.text.as_deref(), Some("\nOn"));
        assert_eq!(state_on.tooltip.as_deref(), Some("(0 Connected)"));

        // Connected (default mode indicator)
        let state_conn = BluetoothModule::format_state_with_config(
            &config,
            &BluetoothState::Connected {
                count: 1,
                device_name: Some("WH-1000XM4".to_string()),
            },
            Orientation::Vertical,
        );
        assert_eq!(state_conn.text.as_deref(), Some("󰂱\nOn"));
        assert_eq!(state_conn.tooltip.as_deref(), Some("(1 Connected)"));
        assert!(state_conn.css_classes.contains(&"connected".to_string()));

        // Connected with custom placeholder mode
        let mut custom_modes = std::collections::HashMap::new();
        custom_modes.insert("connected".to_string(), "󰂱\n%d".to_string());
        let custom_cfg = BluetoothConfig {
            modes: custom_modes,
            ..Default::default()
        };
        let custom_state_conn = BluetoothModule::format_state_with_config(
            &custom_cfg,
            &BluetoothState::Connected {
                count: 1,
                device_name: Some("WH-1000XM4".to_string()),
            },
            Orientation::Vertical,
        );
        assert_eq!(custom_state_conn.text.as_deref(), Some("󰂱\nWH-1000XM4"));
    }

    #[test]
    fn test_bluetooth_custom_loading_mode() {
        let mut modes = std::collections::HashMap::new();
        modes.insert("loading".to_string(), "󰑮\nWait".to_string());
        let config = BluetoothConfig {
            modes,
            ..Default::default()
        };

        let state_loading =
            BluetoothModule::format_state_with_config(&config, &BluetoothState::Loading, Orientation::Vertical);
        assert_eq!(state_loading.text.as_deref(), Some("󰑮\nWait"));
        assert!(state_loading.css_classes.contains(&"loading".to_string()));
    }

    #[test]
    fn test_parse_bluez_properties() {
        // Powered off
        let state = BluetoothModule::parse_bluez_properties(false, Some("off"), &[]);
        assert_eq!(state, BluetoothState::Off);

        // Enabling / Disabling
        let state = BluetoothModule::parse_bluez_properties(false, Some("off-enabling"), &[]);
        assert_eq!(state, BluetoothState::Loading);

        // Powered on with no connected devices
        let state = BluetoothModule::parse_bluez_properties(true, Some("on"), &[("WH-1000XM4", false)]);
        assert_eq!(state, BluetoothState::On);

        // Powered on with 2 connected devices
        let state =
            BluetoothModule::parse_bluez_properties(true, Some("on"), &[("WH-1000XM4", true), ("MX Master 3S", true)]);
        assert_eq!(
            state,
            BluetoothState::Connected {
                count: 2,
                device_name: Some("WH-1000XM4".to_string()),
            }
        );
    }

    #[test]
    fn test_current_state_uses_cached_last() {
        // `current_state` must be lock-only (no BlueZ D-Bus on GTK thread).
        let m = BluetoothModule::new(BluetoothConfig::default());
        let cached = crate::util::lock(&m.last).clone();
        let expected = BluetoothModule::format_state_with_config(&m.config, &cached, Orientation::Vertical);
        let got = m.current_state(Orientation::Vertical);
        assert_eq!(got.text, expected.text);
        assert_eq!(got.tooltip, expected.tooltip);
    }
}
