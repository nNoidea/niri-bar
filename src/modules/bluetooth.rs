use gtk::Orientation;
use std::collections::HashMap;
use std::fs;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::{resolve_mode, BluetoothConfig};
use crate::modules::{
    spawn_command, spawn_dbus_poller, spawn_dbus_trigger, system_bus, BarModule, DbusPollSpec, ModuleCore, ModuleState,
    MouseButton,
};

pub(crate) type BluezManagedObjects =
    HashMap<zbus::OwnedObjectPath, HashMap<String, HashMap<String, zbus::OwnedVariant>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BluetoothState {
    Absent,
    Off,
    Loading,
    On,
    Connected { count: usize, device_name: Option<String> },
}

pub struct BluetoothModule {
    config: BluetoothConfig,
    /// Last known state. Updated by the background worker; `current_state`
    /// formats this cache so the GTK thread never does blocking BlueZ D-Bus I/O.
    last: Arc<Mutex<BluetoothState>>,
    core: ModuleCore,
}

impl BluetoothModule {
    pub fn new(config: BluetoothConfig) -> Self {
        let core = ModuleCore::new();
        let cfg = config.clone();
        // Fast non-blocking rfkill seed at construction so `current_state` has
        // real data immediately without blocking startup on BlueZ D-Bus I/O.
        let (rfkill_ok, _) = Self::read_rfkill();
        let initial = if rfkill_ok {
            BluetoothState::Off
        } else {
            BluetoothState::Absent
        };
        let last: Arc<Mutex<BluetoothState>> = Arc::new(Mutex::new(initial.clone()));

        spawn_dbus_poller(
            core.clone(),
            initial,
            Arc::clone(&last),
            DbusPollSpec {
                tag: "bluetooth",
                base_interval: Duration::from_secs(2),
                max_interval: Duration::from_secs(30),
                fast_poll_on_notify: true,
                degraded_msg: "No Bluetooth adapter found; hiding module",
                degraded_is_warn: false,
                is_transitional: |s| *s == BluetoothState::Loading,
            },
            Self::subscribe_triggers,
            Self::read_bluetooth_state_full,
            move |state, orient| Self::format_state_with_config(&cfg, state, orient),
        );

        Self { config, last, core }
    }

    /// Subscribe to BlueZ signals. Worker-owned: renewed periodically and
    /// unsubscribed automatically when `DbusSubscriptions` is dropped.
    fn subscribe_triggers(core: &ModuleCore) -> Vec<tokio::task::JoinHandle<()>> {
        vec![
            spawn_dbus_trigger(
                core,
                "org.bluez",
                "org.freedesktop.DBus.Properties",
                Some("PropertiesChanged"),
            ),
            spawn_dbus_trigger(core, "org.bluez", "org.freedesktop.DBus.ObjectManager", None),
        ]
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

    pub(crate) fn parse_managed_objects(objects: &BluezManagedObjects) -> Option<BluetoothState> {
        let mut adapter_found = false;
        let mut adapter_powered = false;
        let mut adapter_power_state = None;
        let mut devices = Vec::new();

        for ifaces in objects.values() {
            if let Some(props) = ifaces.get("org.bluez.Adapter1") {
                adapter_found = true;
                if let Some(v) = props.get("Powered") {
                    if let zbus::Variant::Bool(b) = v.deref() {
                        adapter_powered = *b;
                    }
                }
                if let Some(v) = props.get("PowerState") {
                    if let zbus::Variant::Str(s) = v.deref() {
                        adapter_power_state = Some(s.as_str().to_string());
                    }
                }
            }
            if let Some(props) = ifaces.get("org.bluez.Device1") {
                let is_connected = props
                    .get("Connected")
                    .and_then(|v| match v.deref() {
                        zbus::Variant::Bool(b) => Some(*b),
                        _ => None,
                    })
                    .unwrap_or(false);
                let dev_name = props
                    .get("Alias")
                    .or_else(|| props.get("Name"))
                    .and_then(|v| match v.deref() {
                        zbus::Variant::Str(s) => Some(s.as_str().to_string()),
                        _ => None,
                    })
                    .unwrap_or_default();
                devices.push((dev_name, is_connected));
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

    async fn read_bluetooth_state_dbus() -> Option<BluetoothState> {
        let conn = system_bus().await?;
        let res = tokio::time::timeout(
            Duration::from_millis(500),
            conn.call_method(
                Some("org.bluez"),
                "/",
                Some("org.freedesktop.DBus.ObjectManager"),
                "GetManagedObjects",
                &(),
            ),
        )
        .await;
        let msg = match res {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => {
                log_debug!("bluetooth", "BlueZ GetManagedObjects failed: {e}");
                return None;
            }
            Err(_) => {
                log_debug!("bluetooth", "BlueZ GetManagedObjects timed out");
                return None;
            }
        };
        let objects: BluezManagedObjects = msg.body().deserialize().ok()?;
        Self::parse_managed_objects(&objects)
    }

    /// State plus backend health. `healthy` is false when no usable adapter
    /// could be determined (BlueZ down/absent with inconclusive rfkill) —
    /// the worker backs off and the toggle refuses to fire blind commands.
    /// An explicitly rfkill-blocked adapter is `Off` but healthy.
    pub async fn read_bluetooth_state_full() -> (BluetoothState, bool) {
        let (rfkill_ok, unblocked) = Self::read_rfkill();
        if !unblocked {
            return (BluetoothState::Off, true);
        }

        match Self::read_bluetooth_state_dbus().await {
            Some(state) => (state, true),
            None => {
                if rfkill_ok {
                    (BluetoothState::Off, true)
                } else {
                    (BluetoothState::Absent, false)
                }
            }
        }
    }

    pub fn toggle_bluetooth() {
        // Decide off-thread on the shared Tokio runtime.
        crate::runtime::spawn(async {
            let (state, healthy) = Self::read_bluetooth_state_full().await;
            if !healthy || state == BluetoothState::Absent {
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
                BluetoothState::Absent => {}
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
            BluetoothState::Absent => {
                return ModuleState {
                    icon: None,
                    text: None,
                    tooltip: Some("No Bluetooth adapter".to_string()),
                    css_classes: vec!["disabled".to_string(), "no-adapter".to_string()],
                };
            }
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
        self.core.notify();
        rx
    }

    fn handle_click(&self, button: MouseButton) {
        if !self.config.click.try_run(button) && button == MouseButton::Left {
            // Default toggle: emit Loading immediately for instant UI feedback, then toggle.
            *crate::util::lock(&self.last) = BluetoothState::Loading;
            self.core
                .broadcast(|orient| Self::format_state_with_config(&self.config, &BluetoothState::Loading, orient));
            Self::toggle_bluetooth();
        }
        self.core.notify();
    }

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

    #[test]
    fn test_bluetooth_absent_state_formatting_hides() {
        let config = BluetoothConfig::default();

        // Absent state always formats with None text and icon, hiding the widget completely
        let state_hidden =
            BluetoothModule::format_state_with_config(&config, &BluetoothState::Absent, Orientation::Vertical);
        assert_eq!(state_hidden.text, None);
        assert_eq!(state_hidden.icon, None);
        assert!(state_hidden.css_classes.contains(&"no-adapter".to_string()));
    }

    #[test]
    fn test_parse_bluez_managed_objects_map() {
        use std::collections::HashMap;
        let mut objects: BluezManagedObjects = HashMap::new();

        let mut adapter_props = HashMap::new();
        adapter_props.insert(
            "Powered".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from(true)).unwrap(),
        );
        adapter_props.insert(
            "PowerState".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from("on")).unwrap(),
        );
        let mut adapter_ifaces = HashMap::new();
        adapter_ifaces.insert("org.bluez.Adapter1".to_string(), adapter_props);
        objects.insert(
            zbus::OwnedObjectPath::try_from("/org/bluez/hci0").unwrap(),
            adapter_ifaces,
        );

        let mut dev_props = HashMap::new();
        dev_props.insert(
            "Connected".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from(true)).unwrap(),
        );
        dev_props.insert(
            "Alias".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from("AirPods Pro")).unwrap(),
        );
        let mut dev_ifaces = HashMap::new();
        dev_ifaces.insert("org.bluez.Device1".to_string(), dev_props);
        objects.insert(
            zbus::OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB").unwrap(),
            dev_ifaces,
        );

        let state = BluetoothModule::parse_managed_objects(&objects).expect("Should parse BlueZ managed objects");
        assert_eq!(
            state,
            BluetoothState::Connected {
                count: 1,
                device_name: Some("AirPods Pro".to_string()),
            }
        );
    }
}
