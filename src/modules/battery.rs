use gtk::Orientation;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::{resolve_level, BatteryConfig};
use crate::modules::{dbus_get_variant, BarModule, DbusSubscriptions, ModuleCore, ModuleState, ScrollDirection};

const UPOWER_DEST: &str = "org.freedesktop.UPower";
const UPOWER_DISPLAY_DEV: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const UPOWER_DEV_IFACE: &str = "org.freedesktop.UPower.Device";

pub struct BatteryModule {
    config: BatteryConfig,
    /// Last known raw reading. Updated by the background worker; `current_state`
    /// formats this cache so the GTK thread never does blocking UPower D-Bus I/O.
    last: Arc<Mutex<Option<(u8, String)>>>,
    core: ModuleCore,
}

impl BatteryModule {
    pub fn new(config: BatteryConfig) -> Self {
        let core = ModuleCore::new();
        let core_worker = core.clone();
        let cfg = config.clone();
        // Single read at construction so `current_state` has real data immediately;
        // afterwards the worker owns freshness and `current_state` is lock-only.
        let (initial, initial_healthy) = Self::read_battery_full();
        let last: Arc<Mutex<Option<(u8, String)>>> = Arc::new(Mutex::new(initial.clone()));
        let last_worker = Arc::clone(&last);

        // Worker loop driven by UPower D-Bus triggers and 10s fallback.
        // Unhealthy backends back off (10s→60s idle); triggers stay immediate.
        // Subscriptions are worker-owned via `DbusSubscriptions` so they are
        // renewed periodically and always unsubscribed on shutdown.
        crate::runtime::spawn(async move {
            if core_worker.is_stopped() {
                return;
            }
            let mut last_cached = initial;
            let mut fail_streak: u32 = u32::from(!initial_healthy);
            let mut degraded_warned = false;
            let mut _live_subs = DbusSubscriptions::new(Self::subscribe_triggers(&core_worker));
            let mut last_renew = std::time::Instant::now();

            loop {
                let wait = crate::modules::poll_backoff(Duration::from_secs(10), fail_streak, Duration::from_secs(60));
                if core_worker.wait_timeout_async(wait).await {
                    break;
                }
                let (bat_opt, healthy) = Self::read_battery_full();
                if healthy {
                    if fail_streak > 0 {
                        log_info!("battery", "Backend recovered");
                    }
                    fail_streak = 0;
                    degraded_warned = false;
                } else {
                    fail_streak = fail_streak.saturating_add(1);
                    if !degraded_warned {
                        degraded_warned = true;
                        log_warn!("battery", "Power sources unreadable; backing off");
                    }
                }
                if bat_opt != last_cached {
                    last_cached.clone_from(&bat_opt);
                    (*crate::util::lock(&last_worker)).clone_from(&bat_opt);
                    let had_battery = bat_opt.is_some();
                    // No fake 100%: desktops without a battery keep an empty state.
                    let (cap, status) = bat_opt.unwrap_or((0, "NoBattery".to_string()));

                    core_worker.broadcast(|orient| {
                        if had_battery {
                            Self::format_state_static(&cfg, cap, &status, orient)
                        } else {
                            Self::no_battery_state()
                        }
                    });
                }

                let renew_due = last_renew.elapsed() >= Duration::from_secs(300) || fail_streak >= 3;
                if renew_due {
                    _live_subs = DbusSubscriptions::new(Self::subscribe_triggers(&core_worker));
                    last_renew = std::time::Instant::now();
                }
            }
        });

        Self { config, last, core }
    }

    /// Subscribe to UPower signals. Worker-owned: renewed periodically and
    /// unsubscribed automatically when `DbusSubscriptions` is dropped.
    fn subscribe_triggers(core: &ModuleCore) -> Option<(gio::DBusConnection, Vec<gio::SignalSubscriptionId>)> {
        let conn = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE).ok()?;
        let core_upower = core.clone();
        let id = conn.signal_subscribe(
            Some(UPOWER_DEST),
            Some("org.freedesktop.DBus.Properties"),
            Some("PropertiesChanged"),
            None,
            None,
            gio::DBusSignalFlags::NONE,
            move |_conn, _sender, _path, _iface, _signal, _params| {
                core_upower.notify();
            },
        );
        Some((conn, vec![id]))
    }

    pub fn parse_upower_state(state: u32) -> &'static str {
        match state {
            // UPower: 0 Unknown, 1 Charging, 2 Discharging, 3 Empty,
            // 4 Fully charged, 5 Pending charge, 6 Pending discharge.
            0 => "Unknown",
            1 | 5 => "Charging",
            2 | 6 => "Discharging",
            3 => "Empty",
            4 => "Full",
            _ => "Unknown",
        }
    }

    pub fn is_valid_upower_battery(is_present: bool, device_type: u32) -> bool {
        // UPower device types: 2 = Battery, 3 = UPS
        is_present && (device_type == 2 || device_type == 3)
    }

    fn read_battery_upower() -> Option<(u8, String)> {
        let conn = match gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE) {
            Ok(c) => c,
            Err(e) => {
                log_debug!("battery", "System bus unavailable for UPower: {e}");
                return None;
            }
        };

        let is_present =
            dbus_get_variant(&conn, UPOWER_DEST, UPOWER_DISPLAY_DEV, UPOWER_DEV_IFACE, "IsPresent")?.get::<bool>()?;
        let dev_type =
            dbus_get_variant(&conn, UPOWER_DEST, UPOWER_DISPLAY_DEV, UPOWER_DEV_IFACE, "Type")?.get::<u32>()?;

        if !Self::is_valid_upower_battery(is_present, dev_type) {
            return None;
        }

        let percentage = dbus_get_variant(&conn, UPOWER_DEST, UPOWER_DISPLAY_DEV, UPOWER_DEV_IFACE, "Percentage")?
            .get::<f64>()? as u8;
        let state_u32 =
            dbus_get_variant(&conn, UPOWER_DEST, UPOWER_DISPLAY_DEV, UPOWER_DEV_IFACE, "State")?.get::<u32>()?;
        let status = Self::parse_upower_state(state_u32).to_string();

        Some((percentage, status))
    }

    /// Reading plus source health. Healthy when UPower answers, when a
    /// present battery is readable via sysfs, or when no battery exists at
    /// all (desktops idle cheaply). Unhealthy only when a present battery is
    /// unreadable from every source.
    fn read_battery_full() -> (Option<(u8, String)>, bool) {
        if let Some(upower_bat) = Self::read_battery_upower() {
            return (Some(upower_bat), true);
        }

        let (sys_opt, present) = Self::read_battery_sysfs();
        if !present {
            return (None, true);
        }
        let healthy = sys_opt.is_some();
        (sys_opt, healthy)
    }

    /// Sysfs scan plus whether any `BAT*` device exists at all.
    fn read_battery_sysfs() -> (Option<(u8, String)>, bool) {
        let mut present = false;
        if let Ok(entries) = fs::read_dir("/sys/class/power_supply") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with("BAT") {
                    present = true;
                    let cap_path = entry.path().join("capacity");
                    let status_path = entry.path().join("status");

                    let cap = fs::read_to_string(cap_path)
                        .ok()
                        .and_then(|s| s.trim().parse::<u8>().ok());
                    let status = fs::read_to_string(status_path)
                        .unwrap_or_else(|_| "Discharging".into())
                        .trim()
                        .to_string();

                    if let Some(c) = cap {
                        return (Some((c, status)), true);
                    }
                }
            }
        }
        (None, present)
    }

    fn is_plugged_in(status: &str) -> bool {
        if status.eq_ignore_ascii_case("Charging")
            || status.eq_ignore_ascii_case("Full")
            || status.eq_ignore_ascii_case("Not charging")
        {
            return true;
        }
        if status.eq_ignore_ascii_case("Discharging") {
            return false;
        }
        if let Ok(entries) = fs::read_dir("/sys/class/power_supply") {
            for entry in entries.flatten() {
                if let Ok(online) = fs::read_to_string(entry.path().join("online")) {
                    if online.trim() == "1" {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn format_state_static(config: &BatteryConfig, cap: u8, status: &str, _orientation: Orientation) -> ModuleState {
        let plugged = Self::is_plugged_in(status);
        let is_full = status.eq_ignore_ascii_case("Full") || (plugged && cap >= 100);
        let mut css_classes = Vec::new();

        let special = if is_full && config.levels.contains_key("full") {
            css_classes.push("full".to_string());
            if plugged {
                css_classes.push("charging".to_string());
            }
            Some("full")
        } else if plugged {
            css_classes.push("charging".to_string());
            Some("charging")
        } else {
            if cap <= config.critical_threshold {
                css_classes.push("critical".to_string());
            } else if cap <= config.warning_threshold {
                css_classes.push("warning".to_string());
            }
            None
        };

        let (formatted, threshold_opt) = resolve_level(&config.levels, cap as u32, special);

        if let Some(t) = threshold_opt {
            css_classes.push(format!("level-{t}"));
        }

        let tooltip = if plugged {
            "Plugged".to_string()
        } else {
            "Unplugged".to_string()
        };

        ModuleState {
            icon: None,
            text: Some(formatted),
            tooltip: Some(tooltip),
            css_classes,
        }
    }
    fn no_battery_state() -> ModuleState {
        ModuleState {
            icon: None,
            text: None,
            tooltip: Some("No battery".into()),
            css_classes: vec!["no-battery".to_string()],
        }
    }
}

impl BarModule for BatteryModule {
    fn name(&self) -> &'static str {
        "battery"
    }

    fn current_state(&self, orientation: Orientation) -> ModuleState {
        // Lock-only: background worker owns UPower freshness (see `new`).
        if let Some((cap, status)) = crate::util::lock(&self.last).clone() {
            Self::format_state_static(&self.config, cap, &status, orientation)
        } else {
            Self::no_battery_state()
        }
    }

    fn core(&self) -> &ModuleCore {
        &self.core
    }

    fn click_commands(&self) -> (Option<&str>, Option<&str>, Option<&str>) {
        self.config.click.commands()
    }

    fn handle_scroll(&self, _direction: ScrollDirection) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_battery_tooltip_plugged_unplugged() {
        let config = BatteryConfig::default();

        let state_charging = BatteryModule::format_state_static(&config, 80, "Charging", Orientation::Vertical);
        assert_eq!(state_charging.tooltip.as_deref(), Some("Plugged"));
        assert_eq!(state_charging.text.as_deref(), Some("\n80%"));
        assert!(state_charging.css_classes.contains(&"charging".to_string()));

        let state_discharging = BatteryModule::format_state_static(&config, 80, "Discharging", Orientation::Vertical);
        assert_eq!(state_discharging.tooltip.as_deref(), Some("Unplugged"));
        assert_eq!(state_discharging.text.as_deref(), Some("\n80%"));
        assert!(!state_discharging.css_classes.contains(&"charging".to_string()));
    }

    #[test]
    fn test_battery_full_state() {
        let config = BatteryConfig::default();

        let state_full = BatteryModule::format_state_static(&config, 100, "Full", Orientation::Vertical);
        assert_eq!(state_full.tooltip.as_deref(), Some("Plugged"));
        assert_eq!(state_full.text.as_deref(), Some("\nFull"));
        assert!(state_full.css_classes.contains(&"full".to_string()));

        let state_plugged_100 = BatteryModule::format_state_static(&config, 100, "Not charging", Orientation::Vertical);
        assert_eq!(state_plugged_100.text.as_deref(), Some("\nFull"));
    }

    #[test]
    fn test_battery_thresholds() {
        let config = BatteryConfig {
            warning_threshold: 30,
            critical_threshold: 15,
            ..Default::default()
        };

        // Critical <= 15
        let state_crit = BatteryModule::format_state_static(&config, 10, "Discharging", Orientation::Vertical);
        assert!(state_crit.css_classes.contains(&"critical".to_string()));
        assert!(!state_crit.css_classes.contains(&"warning".to_string()));

        // Warning <= 30
        let state_warn = BatteryModule::format_state_static(&config, 25, "Discharging", Orientation::Vertical);
        assert!(state_warn.css_classes.contains(&"warning".to_string()));
        assert!(!state_warn.css_classes.contains(&"critical".to_string()));

        // Normal > 30
        let state_norm = BatteryModule::format_state_static(&config, 50, "Discharging", Orientation::Vertical);
        assert!(!state_norm.css_classes.contains(&"warning".to_string()));
        assert!(!state_norm.css_classes.contains(&"critical".to_string()));
    }

    #[test]
    fn test_parse_upower_state() {
        assert_eq!(BatteryModule::parse_upower_state(1), "Charging");
        assert_eq!(BatteryModule::parse_upower_state(2), "Discharging");
        assert_eq!(BatteryModule::parse_upower_state(3), "Empty");
        assert_eq!(BatteryModule::parse_upower_state(4), "Full");
        assert_eq!(BatteryModule::parse_upower_state(5), "Charging");
        assert_eq!(BatteryModule::parse_upower_state(6), "Discharging");
        assert_eq!(BatteryModule::parse_upower_state(999), "Unknown");
    }

    #[test]
    fn test_current_state_uses_cached_last() {
        // `current_state` must be lock-only (no UPower D-Bus on GTK thread).
        let m = BatteryModule::new(BatteryConfig::default());
        let cached = crate::util::lock(&m.last).clone();
        let expected = match cached {
            Some((cap, status)) => BatteryModule::format_state_static(&m.config, cap, &status, Orientation::Vertical),
            None => BatteryModule::no_battery_state(),
        };
        let got = m.current_state(Orientation::Vertical);
        assert_eq!(got.text, expected.text);
        assert_eq!(got.tooltip, expected.tooltip);
    }

    #[test]
    fn test_validate_upower_battery_device() {
        // Only present devices of type 2 (Battery) or 3 (UPS) are valid batteries
        assert!(BatteryModule::is_valid_upower_battery(true, 2));
        assert!(BatteryModule::is_valid_upower_battery(true, 3));

        // Not present (e.g. desktop DisplayDevice or disconnected battery)
        assert!(!BatteryModule::is_valid_upower_battery(false, 2));
        assert!(!BatteryModule::is_valid_upower_battery(false, 3));

        // Line Power / AC (type 1) or Unknown (type 0)
        assert!(!BatteryModule::is_valid_upower_battery(true, 1));
        assert!(!BatteryModule::is_valid_upower_battery(true, 0));
        assert!(!BatteryModule::is_valid_upower_battery(false, 0));
    }
}
