use glib::prelude::*;
use gtk::Orientation;
use std::fs;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::config::{resolve_level, BatteryConfig};
use crate::modules::{BarModule, ModuleState, ModuleSubscribers, ScrollDirection};

pub struct BatteryModule {
    config: BatteryConfig,
    subscribers: ModuleSubscribers,
    /// Last known raw reading. Updated by the background worker; `current_state`
    /// formats this cache so the GTK thread never does blocking UPower D-Bus I/O.
    last: Arc<Mutex<Option<(u8, String)>>>,
}

impl BatteryModule {
    pub fn new(config: BatteryConfig) -> Self {
        let subscribers: ModuleSubscribers = Arc::new(Mutex::new(Vec::new()));
        let subs_clone = Arc::clone(&subscribers);
        let cfg = config.clone();
        let (trigger_tx, trigger_rx): (Sender<()>, Receiver<()>) = mpsc::channel();
        // Single blocking read at construction (pre-main-loop); worker owns freshness after.
        let initial = Self::read_battery_static();
        let last: Arc<Mutex<Option<(u8, String)>>> = Arc::new(Mutex::new(initial.clone()));
        let last_worker = Arc::clone(&last);

        // Worker loop driven by UPower D-Bus triggers and 10s fallback.
        // Unhealthy backends back off (10s→60s idle); triggers stay immediate.
        // Subscriptions are worker-owned and renewed so a bus restart cannot
        // silently drop them.
        let tx_sub = trigger_tx.clone();
        thread::spawn(move || {
            let mut last_cached = initial;
            let mut fail_streak: u32 = 0;
            let mut degraded_warned = false;
            let mut live_subs = Self::subscribe_triggers(&tx_sub);
            let mut last_renew = std::time::Instant::now();

            loop {
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
                    last_cached = bat_opt.clone();
                    {
                        let mut guard = crate::util::lock(&last_worker);
                        *guard = bat_opt.clone();
                    }
                    let had_battery = bat_opt.is_some();
                    // No fake 100%: desktops without a battery keep an empty state.
                    let (cap, status) = bat_opt.unwrap_or((0, "NoBattery".to_string()));

                    crate::modules::broadcast(&subs_clone, |orient| {
                        if had_battery {
                            Self::format_state_static(&cfg, cap, &status, orient)
                        } else {
                            Self::no_battery_state()
                        }
                    });
                }

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

                let wait = crate::modules::poll_backoff(Duration::from_secs(10), fail_streak, Duration::from_secs(60));
                let _ = trigger_rx.recv_timeout(wait);
            }
        });

        Self {
            config,
            subscribers,
            last,
        }
    }

    /// Subscribe to UPower signals. Worker-owned (see bluetooth): renewed
    /// periodically and on failure streaks.
    fn subscribe_triggers(tx: &Sender<()>) -> Option<(gio::DBusConnection, Vec<gio::SignalSubscriptionId>)> {
        let conn = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE).ok()?;
        let tx_upower = tx.clone();
        let id = conn.signal_subscribe(
            Some("org.freedesktop.UPower"),
            Some("org.freedesktop.DBus.Properties"),
            Some("PropertiesChanged"),
            None,
            None,
            gio::DBusSignalFlags::NONE,
            move |_conn, _sender, _path, _iface, _signal, _params| {
                let _ = tx_upower.send(());
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

    fn read_battery_upower() -> Option<(u8, String)> {
        let conn = match gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE) {
            Ok(c) => c,
            Err(e) => {
                log_debug!("battery", "System bus unavailable for UPower: {e}");
                return None;
            }
        };

        let pct_res = match conn.call_sync(
            Some("org.freedesktop.UPower"),
            "/org/freedesktop/UPower/devices/DisplayDevice",
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&("org.freedesktop.UPower.Device", "Percentage").to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            500,
            gio::Cancellable::NONE,
        ) {
            Ok(v) => v,
            Err(e) => {
                log_debug!("battery", "UPower Percentage Get failed: {e}");
                return None;
            }
        };
        let pct_child = pct_res.child_value(0);
        let pct_inner = pct_child.as_variant()?;
        let percentage = pct_inner.get::<f64>()? as u8;

        let state_res = match conn.call_sync(
            Some("org.freedesktop.UPower"),
            "/org/freedesktop/UPower/devices/DisplayDevice",
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&("org.freedesktop.UPower.Device", "State").to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            500,
            gio::Cancellable::NONE,
        ) {
            Ok(v) => v,
            Err(e) => {
                log_debug!("battery", "UPower State Get failed: {e}");
                return None;
            }
        };
        let state_child = state_res.child_value(0);
        let state_inner = state_child.as_variant()?;
        let state_u32 = state_inner.get::<u32>()?;
        let status = Self::parse_upower_state(state_u32).to_string();

        Some((percentage, status))
    }

    fn read_battery_static() -> Option<(u8, String)> {
        Self::read_battery_full().0
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

    fn subscribe(&self, orientation: Orientation) -> async_channel::Receiver<ModuleState> {
        crate::modules::subscribe_to(&self.subscribers, orientation)
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
}
