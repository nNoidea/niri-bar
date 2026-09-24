use glib::prelude::*;
use gtk::Orientation;
use std::fs;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::config::{resolve_mode, NetworkConfig};
use crate::modules::{BarModule, ModuleCore, ModuleState, ScrollDirection};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetworkType {
    Wifi,
    Ethernet,
    Disconnected,
}

pub struct NetworkModule {
    config: NetworkConfig,
    /// Last known raw state. Updated by the background worker; `current_state`
    /// formats this cache so the GTK thread never does blocking D-Bus I/O.
    last: Arc<Mutex<(NetworkType, Option<String>)>>,
    core: ModuleCore,
}

/// Extract a string from a `Properties.Get` reply (unwraps the outer variant).
fn variant_to_string(v: &glib::Variant) -> Option<String> {
    if let Some(s) = v.str() {
        return Some(s.to_string());
    }
    // Object paths (`ao`/`o`) don't expose `.str()` on all gio versions;
    // fall back to the printed form (e.g. "/org/freedesktop/...").
    let printed = format!("{}", v.print(true));
    let trimmed = printed.trim().trim_matches('\'').to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn get_dbus_variant(
    conn: &gio::DBusConnection,
    dest: &str,
    path: &str,
    interface: &str,
    prop: &str,
) -> Option<glib::Variant> {
    let params = (interface, prop).to_variant();
    let res = match conn.call_sync(
        Some(dest),
        path,
        "org.freedesktop.DBus.Properties",
        "Get",
        Some(&params),
        None,
        gio::DBusCallFlags::NONE,
        500,
        gio::Cancellable::NONE,
    ) {
        Ok(v) => v,
        Err(e) => {
            log_debug!("network", "D-Bus Get failed {dest} {path} {interface}.{prop}: {e}");
            return None;
        }
    };
    let child = res.child_value(0);
    child.as_variant()
}

fn get_dbus_property_string(
    conn: &gio::DBusConnection,
    dest: &str,
    path: &str,
    interface: &str,
    prop: &str,
) -> Option<String> {
    let inner = get_dbus_variant(conn, dest, path, interface, prop)?;
    variant_to_string(&inner)
}

fn get_dbus_property_objpath(
    conn: &gio::DBusConnection,
    dest: &str,
    path: &str,
    interface: &str,
    prop: &str,
) -> Option<String> {
    // Object paths use the same wire container as strings here; share helper.
    get_dbus_property_string(conn, dest, path, interface, prop)
}

fn get_dbus_property_objpath_list(
    conn: &gio::DBusConnection,
    dest: &str,
    path: &str,
    interface: &str,
    prop: &str,
) -> Option<Vec<String>> {
    let inner = get_dbus_variant(conn, dest, path, interface, prop)?;
    let mut list = Vec::new();
    for i in 0..inner.n_children() {
        let item = inner.child_value(i);
        if let Some(s) = variant_to_string(&item) {
            // Skip the null path "/" used by NM for "no primary connection".
            if s != "/" && !s.is_empty() {
                list.push(s);
            }
        }
    }
    Some(list)
}

/// Virtual / container interfaces that must never count as physical Ethernet.
fn is_virtual_interface(name: &str) -> bool {
    name == "lo"
        || name.starts_with("veth")
        || name.starts_with("docker")
        || name.starts_with("br-")
        || name.starts_with("virbr")
        || name.starts_with("vmnet")
        || name.starts_with("tun")
        || name.starts_with("tap")
        || name.starts_with("wg")
        || name.starts_with("ww")
}

impl NetworkModule {
    pub fn new(config: NetworkConfig) -> Self {
        let core = ModuleCore::new();
        let core_worker = core.clone();
        let cfg = config.clone();
        let (trigger_tx, trigger_rx): (Sender<()>, Receiver<()>) = mpsc::channel();
        let tx_shutdown = trigger_tx.clone();
        core.on_shutdown(move || {
            let _ = tx_shutdown.send(());
        });
        // Single blocking read at construction (pre-main-loop); afterwards the
        // worker owns freshness and `current_state` is lock-only.
        let initial = Self::read_network_state();
        let last: Arc<Mutex<(NetworkType, Option<String>)>> = Arc::new(Mutex::new(initial.clone()));
        let last_worker = Arc::clone(&last);

        // Background worker loop driven by NetworkManager D-Bus events and periodic timeout.
        // Unhealthy backends back off (3s→30s idle); triggers stay immediate.
        // Subscriptions are worker-owned and renewed so a bus restart cannot
        // silently drop them.
        let tx_sub = trigger_tx.clone();
        thread::spawn(move || {
            let (mut last_type, mut last_ssid) = initial;
            let mut fail_streak: u32 = 0;
            let mut degraded_warned = false;
            let mut live_subs = Self::subscribe_triggers(&tx_sub);
            let mut last_renew = std::time::Instant::now();

            loop {
                if core_worker.is_stopped() {
                    break;
                }
                let ((net_type, ssid), healthy) = NetworkModule::read_network_state_full();

                if healthy {
                    if fail_streak > 0 {
                        log_info!("network", "Backend recovered");
                    }
                    fail_streak = 0;
                    degraded_warned = false;
                } else {
                    fail_streak = fail_streak.saturating_add(1);
                    if !degraded_warned {
                        degraded_warned = true;
                        log_warn!("network", "No usable source (NM down, /sys unreadable); backing off");
                    }
                }

                if net_type != last_type || ssid != last_ssid {
                    last_type = net_type.clone();
                    last_ssid = ssid.clone();

                    {
                        let mut guard = crate::util::lock(&last_worker);
                        *guard = (net_type.clone(), ssid.clone());
                    }
                    core_worker.broadcast(|orient| {
                        NetworkModule::format_state_with_config(&cfg, &net_type, ssid.as_deref(), orient)
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

                if core_worker.is_stopped() {
                    break;
                }
                // Wait for D-Bus signal trigger, else idle check with backoff.
                let wait = crate::modules::poll_backoff(Duration::from_secs(3), fail_streak, Duration::from_secs(30));
                let _ = trigger_rx.recv_timeout(wait);
                if core_worker.is_stopped() {
                    break;
                }
            }
        });

        Self { config, last, core }
    }

    /// Subscribe to NetworkManager signals. Worker-owned (see bluetooth):
    /// renewed periodically and on failure streaks.
    fn subscribe_triggers(tx: &Sender<()>) -> Option<(gio::DBusConnection, Vec<gio::SignalSubscriptionId>)> {
        let conn = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE).ok()?;
        let mut ids = Vec::new();
        let tx_nm = tx.clone();
        ids.push(conn.signal_subscribe(
            Some("org.freedesktop.NetworkManager"),
            Some("org.freedesktop.DBus.Properties"),
            Some("PropertiesChanged"),
            None,
            None,
            gio::DBusSignalFlags::NONE,
            move |_conn, _sender, _path, _iface, _signal, _params| {
                let _ = tx_nm.send(());
            },
        ));

        let tx_ac = tx.clone();
        ids.push(conn.signal_subscribe(
            Some("org.freedesktop.NetworkManager"),
            Some("org.freedesktop.NetworkManager.Connection.Active"),
            Some("StateChanged"),
            None,
            None,
            gio::DBusSignalFlags::NONE,
            move |_conn, _sender, _path, _iface, _signal, _params| {
                let _ = tx_ac.send(());
            },
        ));
        Some((conn, ids))
    }

    pub fn parse_nm_connection_type_and_id(conn_type: &str, id: &str) -> (NetworkType, Option<String>) {
        if conn_type == "802-11-wireless" {
            (NetworkType::Wifi, Some(id.to_string()))
        } else if conn_type == "802-3-ethernet" {
            (NetworkType::Ethernet, None)
        } else {
            (NetworkType::Disconnected, None)
        }
    }

    fn read_network_state_dbus() -> Option<(NetworkType, Option<String>)> {
        let conn = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE).ok()?;

        // 1. Check PrimaryConnection
        let primary_path = get_dbus_property_objpath(
            &conn,
            "org.freedesktop.NetworkManager",
            "/org/freedesktop/NetworkManager",
            "org.freedesktop.NetworkManager",
            "PrimaryConnection",
        );

        let mut candidate_paths = Vec::new();
        if let Some(ref path) = primary_path {
            if path != "/" && !path.is_empty() {
                candidate_paths.push(path.clone());
            }
        }

        // 2. If no primary connection or invalid, check ActiveConnections
        if candidate_paths.is_empty() {
            if let Some(active_list) = get_dbus_property_objpath_list(
                &conn,
                "org.freedesktop.NetworkManager",
                "/org/freedesktop/NetworkManager",
                "org.freedesktop.NetworkManager",
                "ActiveConnections",
            ) {
                candidate_paths.extend(active_list);
            }
        }

        for path in candidate_paths {
            let conn_type = get_dbus_property_string(
                &conn,
                "org.freedesktop.NetworkManager",
                &path,
                "org.freedesktop.NetworkManager.Connection.Active",
                "Type",
            )
            .unwrap_or_default();

            let id = get_dbus_property_string(
                &conn,
                "org.freedesktop.NetworkManager",
                &path,
                "org.freedesktop.NetworkManager.Connection.Active",
                "Id",
            )
            .unwrap_or_default();

            let (ntype, ssid) = Self::parse_nm_connection_type_and_id(&conn_type, &id);
            if ntype != NetworkType::Disconnected {
                return Some((ntype, ssid));
            }
        }

        None
    }

    fn update_best_route(best: &mut Option<(String, u32)>, iface: &str, flags: u32, metric: u32) {
        const RTF_UP: u32 = 0x1;
        if flags & RTF_UP != 0 && best.as_ref().map_or(true, |(_, m)| metric < *m) {
            *best = Some((iface.to_string(), metric));
        }
    }

    fn get_active_default_interface() -> Option<String> {
        let mut best_iface: Option<(String, u32)> = None;

        // 1. Check IPv4 default routes in /proc/net/route
        if let Ok(content) = fs::read_to_string("/proc/net/route") {
            for line in content.lines().skip(1) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 7 && parts[1] == "00000000" {
                    if let (Ok(flags), Ok(metric)) = (u32::from_str_radix(parts[3], 16), parts[6].parse::<u32>()) {
                        Self::update_best_route(&mut best_iface, parts[0], flags, metric);
                    }
                }
            }
        }

        if let Some((iface, _)) = best_iface {
            return Some(iface);
        }

        // 2. Check IPv6 default routes in /proc/net/ipv6_route
        if let Ok(content) = fs::read_to_string("/proc/net/ipv6_route") {
            for line in content.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 10 && parts[0].chars().all(|c| c == '0') && parts[1] == "00" && parts[9] != "lo" {
                    if let (Ok(flags), Ok(metric)) =
                        (u32::from_str_radix(parts[8], 16), u32::from_str_radix(parts[5], 16))
                    {
                        Self::update_best_route(&mut best_iface, parts[9], flags, metric);
                    }
                }
            }
        }

        best_iface.map(|(iface, _)| iface)
    }

    fn read_network_state() -> (NetworkType, Option<String>) {
        Self::read_network_state_full().0
    }

    /// State plus source health. Healthy when NetworkManager answers *or*
    /// any `/sys`/`/proc` source is readable — a legitimately offline machine
    /// (cable pulled, NM fine, files fine) stays healthy so cable events
    /// keep their 3s responsiveness. Only "everything unreadable" backs off.
    fn read_network_state_full() -> ((NetworkType, Option<String>), bool) {
        if let Some(state) = Self::read_network_state_dbus() {
            return (state, true);
        }

        // D-Bus path failed; sysfs fallbacks below are first-class.
        let sys_readable = fs::read_dir("/sys/class/net").is_ok()
            || fs::read_to_string("/proc/net/route").is_ok()
            || fs::read_to_string("/proc/net/ipv6_route").is_ok();

        // 1. Fallback: Determine the interface actively being used by the system for default routing
        if let Some(iface) = Self::get_active_default_interface() {
            if is_virtual_interface(&iface) {
                // e.g. docker/veth/wg default routes: not physical link state.
            } else if iface.starts_with("wl") || fs::metadata(format!("/sys/class/net/{iface}/wireless")).is_ok() {
                return ((NetworkType::Wifi, None), true);
            } else if iface.starts_with("en")
                || iface.starts_with("eth")
                || fs::metadata(format!("/sys/class/net/{iface}/device")).is_ok()
            {
                return ((NetworkType::Ethernet, None), true);
            }
        }

        // 2. Fallback: If no default route is found, check interfaces with operstate "up"
        if let Ok(entries) = fs::read_dir("/sys/class/net") {
            let mut eth_up = false;
            let mut wifi_up = false;

            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();

                if is_virtual_interface(&name_str) {
                    continue;
                }

                let operstate = fs::read_to_string(entry.path().join("operstate"))
                    .unwrap_or_default()
                    .trim()
                    .to_string();

                if operstate == "up" {
                    if name_str.starts_with("en") || name_str.starts_with("eth") {
                        eth_up = true;
                    } else if name_str.starts_with("wl") {
                        wifi_up = true;
                    }
                }
            }

            if eth_up {
                return ((NetworkType::Ethernet, None), true);
            }
            if wifi_up {
                return ((NetworkType::Wifi, None), true);
            }
        }

        ((NetworkType::Disconnected, None), sys_readable)
    }

    fn format_state_with_config(
        config: &NetworkConfig,
        net_type: &NetworkType,
        ssid_opt: Option<&str>,
        _orientation: Orientation,
    ) -> ModuleState {
        let mut css_classes = Vec::new();

        let (mode_key, default_tooltip, placeholders) = match net_type {
            NetworkType::Wifi => {
                css_classes.push("wifi".into());
                let ssid = ssid_opt.unwrap_or("Wi-Fi");
                ("wifi", ssid.to_string(), vec![("%s", ssid.to_string())])
            }
            NetworkType::Ethernet => {
                css_classes.push("ethernet".into());
                ("lan", "Ethernet".to_string(), vec![("%s", "Ethernet".into())])
            }
            NetworkType::Disconnected => {
                css_classes.push("disconnected".into());
                ("disconnected", "Disconnected".to_string(), vec![("%s", "Off".into())])
            }
        };

        let placeholder_refs: Vec<(&str, &str)> = placeholders.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let formatted = resolve_mode(&config.modes, mode_key, &placeholder_refs);

        ModuleState {
            icon: None,
            text: Some(formatted),
            tooltip: Some(default_tooltip),
            css_classes,
        }
    }
}

impl BarModule for NetworkModule {
    fn name(&self) -> &'static str {
        "network"
    }

    fn current_state(&self, orientation: Orientation) -> ModuleState {
        // Lock-only: background worker owns D-Bus freshness (see `new`).
        let (state, ssid) = crate::util::lock(&self.last).clone();
        Self::format_state_with_config(&self.config, &state, ssid.as_deref(), orientation)
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
    fn test_network_format_states() {
        let config = NetworkConfig::default();

        let wifi_state = NetworkModule::format_state_with_config(
            &config,
            &NetworkType::Wifi,
            Some("MyHomeNetwork"),
            Orientation::Vertical,
        );
        assert_eq!(wifi_state.text.as_deref(), Some("󰤨\nWiFi"));
        assert_eq!(wifi_state.tooltip.as_deref(), Some("MyHomeNetwork"));
        assert!(wifi_state.css_classes.contains(&"wifi".to_string()));

        let eth_state =
            NetworkModule::format_state_with_config(&config, &NetworkType::Ethernet, None, Orientation::Vertical);
        assert_eq!(eth_state.text.as_deref(), Some("󰈀\nLAN"));
        assert_eq!(eth_state.tooltip.as_deref(), Some("Ethernet"));
        assert!(eth_state.css_classes.contains(&"ethernet".to_string()));

        let off_state =
            NetworkModule::format_state_with_config(&config, &NetworkType::Disconnected, None, Orientation::Vertical);
        assert_eq!(off_state.text.as_deref(), Some("󰤮\nOff"));
        assert_eq!(off_state.tooltip.as_deref(), Some("Disconnected"));
        assert!(off_state.css_classes.contains(&"disconnected".to_string()));
    }

    #[test]
    fn test_network_clickability() {
        let unclickable = NetworkModule::new(NetworkConfig::default());
        assert!(!unclickable.is_clickable());

        let clickable = NetworkModule::new(NetworkConfig {
            click: crate::config::ClickActions {
                on_click_left: Some("nm-connection-editor".to_string()),
                ..Default::default()
            },
            ..Default::default()
        });
        assert!(clickable.is_clickable());
    }

    #[test]
    fn test_parse_nm_connection_type_and_id() {
        let (wifi_type, ssid) = NetworkModule::parse_nm_connection_type_and_id("802-11-wireless", "MyWiFiNetwork");
        assert_eq!(wifi_type, NetworkType::Wifi);
        assert_eq!(ssid.as_deref(), Some("MyWiFiNetwork"));

        let (eth_type, no_ssid) =
            NetworkModule::parse_nm_connection_type_and_id("802-3-ethernet", "Wired connection 1");
        assert_eq!(eth_type, NetworkType::Ethernet);
        assert_eq!(no_ssid, None);

        let (disc_type, _) = NetworkModule::parse_nm_connection_type_and_id("unknown", "none");
        assert_eq!(disc_type, NetworkType::Disconnected);
    }

    #[test]
    fn test_current_state_uses_cached_last() {
        // `current_state` must be lock-only (no D-Bus on GTK thread).
        let m = NetworkModule::new(NetworkConfig::default());
        let (t, s) = crate::util::lock(&m.last).clone();
        let expected = NetworkModule::format_state_with_config(&m.config, &t, s.as_deref(), Orientation::Vertical);
        let got = m.current_state(Orientation::Vertical);
        assert_eq!(got.text, expected.text);
        assert_eq!(got.tooltip, expected.tooltip);
    }

    #[test]
    fn test_update_best_route() {
        let mut best = None;
        NetworkModule::update_best_route(&mut best, "eth0", 0x3, 100);
        assert_eq!(best, Some(("eth0".to_string(), 100)));

        // Lower metric wins
        NetworkModule::update_best_route(&mut best, "wlan0", 0x3, 50);
        assert_eq!(best, Some(("wlan0".to_string(), 50)));

        // Higher metric ignored
        NetworkModule::update_best_route(&mut best, "eth1", 0x3, 200);
        assert_eq!(best, Some(("wlan0".to_string(), 50)));

        // Route not UP (flags & 0x1 == 0) ignored even with lower metric
        NetworkModule::update_best_route(&mut best, "eth2", 0x2, 10);
        assert_eq!(best, Some(("wlan0".to_string(), 50)));
    }
}
