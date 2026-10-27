use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use zbus::Connection;

use super::service::TrayService;
use super::types::{TrayEvent, TrayItemMap};

/// Drop items whose bus currently has no owner.
///
/// Runs once per session after discovery settles. Apps that exit during a
/// bus outage (or between our probe and tracking) never emit
/// `NameOwnerChanged`, so without this their icons linger until restart.
pub(super) async fn reconcile_item_owners(
    conn: &Connection,
    items_map: &Arc<Mutex<TrayItemMap>>,
    subs_map: &Arc<Mutex<Vec<async_channel::Sender<TrayEvent>>>>,
    reg_items_set: &Arc<Mutex<HashSet<String>>>,
    tasks_map: &Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
) {
    let Ok(dbus_proxy) = zbus::fdo::DBusProxy::new(conn).await else {
        log_debug!("tray", "Owner reconcile skipped: no DBusProxy");
        return;
    };
    let keys: Vec<String> = {
        let guard = crate::util::lock(items_map);
        guard.keys().cloned().collect()
    };
    let mut removed = 0u32;
    for key in keys {
        let bus = {
            crate::util::lock(items_map)
                .get(&key)
                .map(|(item, _)| item.bus_name.clone())
        };
        let Some(bus_name) = bus else { continue };
        let Ok(name) = zbus::names::BusName::try_from(bus_name.as_str()) else {
            continue;
        };
        if dbus_proxy.get_name_owner(name).await.is_err() {
            crate::util::lock(items_map).remove(&key);
            crate::util::lock(reg_items_set).remove(&key);
            if let Some(handle) = crate::util::lock(tasks_map).remove(&key) {
                handle.abort();
            }
            TrayService::broadcast(subs_map, TrayEvent::Remove(key.clone()));
            removed += 1;
        }
    }
    if removed > 0 {
        log_info!("tray", "Owner reconcile removed {removed} stale item(s)");
    }
}

pub(super) async fn probe_and_register_all(
    conn: &Connection,
    reg_tx: &tokio::sync::mpsc::Sender<String>,
    reg_items_set: &Arc<Mutex<HashSet<String>>>,
    sem: &Arc<tokio::sync::Semaphore>,
) {
    let Ok(dbus_proxy) = zbus::fdo::DBusProxy::new(conn).await else {
        log_debug!("tray", "Probe skipped: no DBusProxy");
        return;
    };
    let Ok(names) = dbus_proxy.list_names().await else {
        log_debug!("tray", "Probe skipped: list_names failed");
        return;
    };
    for name in names {
        let s = name.to_string();
        if !is_probe_candidate(&s) {
            continue;
        }
        let conn_c = conn.clone();
        let reg_tx_c = reg_tx.clone();
        let reg_set_c = Arc::clone(reg_items_set);
        let sem_c = Arc::clone(sem);
        tokio::spawn(async move {
            // Bound login-storm fan-out; excess probes wait.
            let _permit = sem_c.acquire_owned().await.ok()?;
            probe_single_service(&conn_c, &s, &reg_tx_c, &reg_set_c).await;
            Some(())
        });
    }
}

async fn probe_prop(conn: &Connection, service: &str, path: &str, iface: &str, prop: &str) -> bool {
    tokio::time::timeout(
        tokio::time::Duration::from_millis(150),
        conn.call_method(
            Some(service),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(iface, prop),
        ),
    )
    .await
    .is_ok_and(|r| r.is_ok())
}

/// Well-known buses that can never be tray items (hosts/watchers themselves).
pub fn is_probe_candidate(name: &str) -> bool {
    !(name.starts_with("org.freedesktop.") || name.starts_with("org.kde."))
}

/// Whether a tray item belongs to a disconnected D-Bus owner from a
/// `NameOwnerChanged` signal (`name` = well-known name, `old_owner` = unique
/// name that vanished). Pure so the cleanup matching is unit-testable.
pub fn belongs_to_disconnected_owner(item_bus: &str, key: &str, name: &str, old_owner: &str) -> bool {
    let key_bus = key.split_once('/').map_or(key, |(b, _)| b);
    (!name.is_empty() && (item_bus == name || key_bus == name))
        || (!old_owner.is_empty() && (item_bus == old_owner || key_bus == old_owner))
}

/// Parse Ayatana `/org/ayatana/NotificationItem` introspection XML into full
/// child item paths. Pure so the string scraping is unit-testable.
pub fn parse_ayatana_children(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in xml.lines() {
        if let Some(pos) = line.find("<node name=\"") {
            let rest = &line[pos + 12..];
            if let Some(end) = rest.find('"') {
                let child_name = &rest[..end];
                if !child_name.is_empty() {
                    out.push(format!("/org/ayatana/NotificationItem/{child_name}"));
                }
            }
        }
    }
    out
}

/// Object paths worth probing for StatusNotifierItem properties.
pub fn candidate_sni_paths(ayatana_children: &[String]) -> Vec<String> {
    let mut paths = vec![
        "/StatusNotifierItem".to_string(),
        "/org/ayatana/NotificationItem/steam".to_string(),
        "/MenuBar".to_string(),
    ];
    paths.extend(ayatana_children.iter().cloned());
    paths
}

pub(super) async fn probe_single_service(
    conn: &Connection,
    service: &str,
    reg_tx: &tokio::sync::mpsc::Sender<String>,
    reg_items_set: &Arc<Mutex<HashSet<String>>>,
) {
    // Resolve well-known names to unique bus name to prevent duplicate registration
    let unique_bus = if !service.starts_with(':') {
        if let Ok(Ok(owner_res)) = tokio::time::timeout(
            tokio::time::Duration::from_millis(100),
            conn.call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetNameOwner",
                &(service,),
            ),
        )
        .await
        {
            if let Ok(owner) = owner_res.body().deserialize::<String>() {
                owner
            } else {
                service.to_string()
            }
        } else {
            service.to_string()
        }
    } else {
        service.to_string()
    };

    let mut candidate_paths = candidate_sni_paths(&[]);

    // Check if service has any Ayatana child items with a fast timeout
    if let Ok(Ok(res)) = tokio::time::timeout(
        tokio::time::Duration::from_millis(100),
        conn.call_method(
            Some(service),
            "/org/ayatana/NotificationItem",
            Some("org.freedesktop.DBus.Introspectable"),
            "Introspect",
            &(),
        ),
    )
    .await
    {
        if let Ok(xml) = res.body().deserialize::<String>() {
            candidate_paths.extend(parse_ayatana_children(&xml));
        }
    }

    for path in candidate_paths {
        let is_sni = probe_prop(conn, service, path.as_str(), "org.kde.StatusNotifierItem", "Id").await
            || probe_prop(conn, service, path.as_str(), "org.freedesktop.StatusNotifierItem", "Id").await
            || probe_prop(conn, service, path.as_str(), "org.kde.StatusNotifierItem", "Status").await;

        if is_sni {
            let key = format!("{unique_bus}{path}");
            {
                let mut set = crate::util::lock(reg_items_set);
                if !set.insert(key.clone()) {
                    return; // Already registered
                }
            }
            log_info!("tray", "Proactively discovered SNI service: {}", key);
            if let Err(e) = reg_tx.try_send(key.clone()) {
                log_warn!("tray", "Dropped proactive discovery for {key}: {e}");
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_probe_candidate() {
        assert!(!is_probe_candidate("org.freedesktop.StatusNotifierWatcher"));
        assert!(!is_probe_candidate("org.kde.StatusNotifierItem-123-1"));
        assert!(is_probe_candidate(":1.6300"));
        assert!(is_probe_candidate("org.mozilla.firefox"));
    }

    #[test]
    fn test_parse_ayatana_children() {
        let xml = "<node>\n<node name=\"steam\"/>\n<node name=\"\"/>\n<node name=\"vlc\"/>\n</node>";
        assert_eq!(
            parse_ayatana_children(xml),
            vec![
                "/org/ayatana/NotificationItem/steam".to_string(),
                "/org/ayatana/NotificationItem/vlc".to_string(),
            ]
        );
        assert!(parse_ayatana_children("<node></node>").is_empty());
    }

    #[test]
    fn test_candidate_sni_paths() {
        let base = candidate_sni_paths(&[]);
        assert_eq!(base.len(), 3);
        assert_eq!(base[0], "/StatusNotifierItem");
        let extended = candidate_sni_paths(&["/org/ayatana/NotificationItem/vlc".to_string()]);
        assert_eq!(extended.len(), 4);
        assert_eq!(extended[3], "/org/ayatana/NotificationItem/vlc");
    }

    #[test]
    fn test_belongs_to_disconnected_owner() {
        // Well-known name match.
        assert!(belongs_to_disconnected_owner(
            "spotify",
            "spotify/StatusNotifierItem",
            "spotify",
            ":1.50"
        ));
        // Unique (old owner) match.
        assert!(belongs_to_disconnected_owner(
            ":1.50",
            ":1.50/StatusNotifierItem",
            "spotify",
            ":1.50"
        ));
        // Key prefix match (registered under the unique name).
        assert!(belongs_to_disconnected_owner(
            ":1.99",
            ":1.50/MenuBar",
            "spotify",
            ":1.50"
        ));
        // Unrelated item survives.
        assert!(!belongs_to_disconnected_owner(
            "vlc",
            "vlc/StatusNotifierItem",
            "spotify",
            ":1.50"
        ));
        assert!(!belongs_to_disconnected_owner(
            ":1.60",
            ":1.60/StatusNotifierItem",
            "spotify",
            ":1.50"
        ));
        // Numeric suffix collision (e.g. `:1.6` or `:1.61` disconnecting
        // when `:1.616/org/ayatana/NotificationItem/steam` or `:1.636/StatusNotifierItem`
        // is registered) must NOT match!
        assert!(!belongs_to_disconnected_owner(
            ":1.616",
            ":1.616/org/ayatana/NotificationItem/steam",
            ":1.6",
            ":1.6"
        ));
        assert!(!belongs_to_disconnected_owner(
            ":1.616",
            ":1.616/org/ayatana/NotificationItem/steam",
            ":1.61",
            ":1.61"
        ));
        assert!(!belongs_to_disconnected_owner(
            ":1.636",
            ":1.636/StatusNotifierItem",
            ":1.63",
            ":1.63"
        ));
    }
}
