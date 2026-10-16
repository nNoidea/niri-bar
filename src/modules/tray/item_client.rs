use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use zbus::Connection;

use super::protocol::{parse_icon_pixmap, parse_raw_menu_item, parse_tooltip, RawMenuItem};
use super::service::TrayService;
use super::types::{ActivateRequest, TrayEvent, TrayItem, TrayItemMap, TrayMenu};

pub(super) async fn handle_activate_request(conn: &Connection, req: ActivateRequest) {
    match req {
        ActivateRequest::Default {
            bus_name,
            object_path,
            x,
            y,
        } => {
            log_debug!("tray", "Activating tray item: {} at ({}, {})", bus_name, x, y);
            let res = conn
                .call_method(
                    Some(bus_name.as_str()),
                    object_path.as_str(),
                    Some("org.kde.StatusNotifierItem"),
                    "Activate",
                    &(x, y),
                )
                .await;

            if let Err(e) = res {
                log_debug!("tray", "KDE Activate failed for {bus_name}, trying FDO: {e}");
                if let Err(e2) = conn
                    .call_method(
                        Some(bus_name.as_str()),
                        object_path.as_str(),
                        Some("org.freedesktop.StatusNotifierItem"),
                        "Activate",
                        &(x, y),
                    )
                    .await
                {
                    log_warn!("tray", "Activate failed for {bus_name} {object_path}: {e2}");
                }
            }
        }
        ActivateRequest::Secondary {
            bus_name,
            object_path,
            x,
            y,
        } => {
            log_debug!("tray", "SecondaryActivate on item: {} at ({}, {})", bus_name, x, y);
            let res = conn
                .call_method(
                    Some(bus_name.as_str()),
                    object_path.as_str(),
                    Some("org.kde.StatusNotifierItem"),
                    "SecondaryActivate",
                    &(x, y),
                )
                .await;

            if let Err(e) = res {
                log_debug!("tray", "SecondaryActivate failed for {bus_name}, trying Ayatana: {e}");
                if let Err(e2) = conn
                    .call_method(
                        Some(bus_name.as_str()),
                        object_path.as_str(),
                        Some("org.kde.StatusNotifierItem"),
                        "XAyatanaSecondaryActivate",
                        &(0u32,),
                    )
                    .await
                {
                    log_warn!("tray", "SecondaryActivate failed for {bus_name} {object_path}: {e2}");
                }
            }
        }
        ActivateRequest::ContextMenu {
            bus_name,
            object_path,
            x,
            y,
        } => {
            log_debug!("tray", "ContextMenu on item: {} at ({}, {})", bus_name, x, y);
            let res = conn
                .call_method(
                    Some(bus_name.as_str()),
                    object_path.as_str(),
                    Some("org.kde.StatusNotifierItem"),
                    "ContextMenu",
                    &(x, y),
                )
                .await;

            if let Err(e) = res {
                log_debug!(
                    "tray",
                    "ContextMenu failed for {bus_name}, trying SecondaryActivate: {e}"
                );
                if let Err(e2) = conn
                    .call_method(
                        Some(bus_name.as_str()),
                        object_path.as_str(),
                        Some("org.kde.StatusNotifierItem"),
                        "SecondaryActivate",
                        &(x, y),
                    )
                    .await
                {
                    log_warn!("tray", "ContextMenu fallback failed for {bus_name} {object_path}: {e2}");
                }
            }
        }
        ActivateRequest::Scroll {
            bus_name,
            object_path,
            delta,
            orientation,
        } => {
            if let Err(e) = conn
                .call_method(
                    Some(bus_name.as_str()),
                    object_path.as_str(),
                    Some("org.kde.StatusNotifierItem"),
                    "Scroll",
                    &(delta, orientation.as_str()),
                )
                .await
            {
                log_warn!("tray", "Scroll failed for {bus_name} {object_path}: {e}");
            }
        }
        ActivateRequest::MenuItem {
            bus_name,
            menu_path,
            submenu_id,
        } => {
            log_debug!(
                "tray",
                "MenuItem clicked: {} -> {} (id: {})",
                bus_name,
                menu_path,
                submenu_id
            );
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32;

            if let Err(e) = conn
                .call_method(
                    Some(bus_name.as_str()),
                    menu_path.as_str(),
                    Some("com.canonical.dbusmenu"),
                    "Event",
                    &(submenu_id, "clicked", zvariant::Value::I32(0), now),
                )
                .await
            {
                log_warn!(
                    "tray",
                    "Menu Event failed for {bus_name} {menu_path} id {submenu_id}: {e}"
                );
            }
        }
        ActivateRequest::AboutToShow {
            bus_name,
            menu_path,
            id,
        } => {
            if let Err(e) = conn
                .call_method(
                    Some(bus_name.as_str()),
                    menu_path.as_str(),
                    Some("com.canonical.dbusmenu"),
                    "AboutToShow",
                    &(id,),
                )
                .await
            {
                log_debug!("tray", "AboutToShow failed for {bus_name} {menu_path} id {id}: {e}");
            }
        }
    }
}

async fn fetch_item_properties(conn: &Connection, bus: &str, path: &str) -> TrayItem {
    let mut props_map: HashMap<String, zvariant::OwnedValue> = HashMap::new();

    // 1. Try GetAll on org.kde.StatusNotifierItem
    let get_all_res = conn
        .call_method(
            Some(bus),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &("org.kde.StatusNotifierItem",),
        )
        .await;

    if let Ok(msg) = get_all_res {
        if let Ok(map) = msg.body().deserialize::<HashMap<String, zvariant::OwnedValue>>() {
            props_map = map;
        }
    }

    // 2. If empty, try GetAll on org.freedesktop.StatusNotifierItem
    if props_map.is_empty() {
        if let Ok(msg) = conn
            .call_method(
                Some(bus),
                path,
                Some("org.freedesktop.DBus.Properties"),
                "GetAll",
                &("org.freedesktop.StatusNotifierItem",),
            )
            .await
        {
            if let Ok(map) = msg.body().deserialize::<HashMap<String, zvariant::OwnedValue>>() {
                props_map = map;
            }
        }
    }

    // Tolerant field extraction:
    let id = props_map
        .get("Id")
        .and_then(|v| match v.deref() {
            zvariant::Value::Str(s) => Some(s.as_str().to_string()),
            _ => None,
        })
        .unwrap_or_else(|| bus.to_string());

    let title = props_map.get("Title").and_then(|v| match v.deref() {
        zvariant::Value::Str(s) => Some(s.as_str().to_string()),
        _ => None,
    });

    let icon_name = props_map
        .get("IconName")
        .and_then(|v| match v.deref() {
            zvariant::Value::Str(s) => Some(s.as_str().to_string()),
            _ => None,
        })
        .filter(|s| !s.trim().is_empty());

    let icon_theme_path = props_map
        .get("IconThemePath")
        .and_then(|v| match v.deref() {
            zvariant::Value::Str(s) => Some(s.as_str().to_string()),
            _ => None,
        })
        .filter(|s| !s.trim().is_empty());

    let icon_pixmap = props_map.get("IconPixmap").and_then(|v| parse_icon_pixmap(v.deref()));
    let tool_tip = parse_tooltip(&props_map);

    let item_is_menu = props_map
        .get("ItemIsMenu")
        .and_then(|v| match v.deref() {
            zvariant::Value::Bool(b) => Some(*b),
            zvariant::Value::I32(i) => Some(*i != 0),
            zvariant::Value::U32(u) => Some(*u != 0),
            _ => None,
        })
        .unwrap_or(false);

    let menu_path = props_map
        .get("Menu")
        .and_then(|v| match v.deref() {
            zvariant::Value::ObjectPath(p) => Some(p.as_str().to_string()),
            zvariant::Value::Str(s) => Some(s.as_str().to_string()),
            _ => None,
        })
        .filter(|p| !p.is_empty() && p != "/NO_DBUSMENU" && p != "/");

    TrayItem {
        bus_name: bus.to_string(),
        object_path: path.to_string(),
        id,
        title,
        icon_name,
        icon_theme_path,
        icon_pixmap,
        tool_tip,
        item_is_menu,
        menu_path,
    }
}

async fn fetch_menu_layout(conn: &Connection, bus: &str, menu_path: &str) -> Option<TrayMenu> {
    let empty_props: Vec<String> = Vec::new();
    let res = conn
        .call_method(
            Some(bus),
            menu_path,
            Some("com.canonical.dbusmenu"),
            "GetLayout",
            &(0i32, -1i32, empty_props),
        )
        .await;

    let msg = match res {
        Ok(m) => m,
        Err(e) => {
            log_warn!("tray", "Failed to call GetLayout on {} {}: {}", bus, menu_path, e);
            return None;
        }
    };

    let body = msg.body();
    let (revision, root_item): (u32, RawMenuItem) = match body.deserialize() {
        Ok(pair) => pair,
        Err(e) => {
            log_warn!(
                "tray",
                "Failed to deserialize GetLayout response from {} {}: {}",
                bus,
                menu_path,
                e
            );
            return None;
        }
    };

    let parsed_root = parse_raw_menu_item(&root_item);
    log_info!(
        "tray",
        "Successfully parsed DBusMenu for {} {} (revision {}, items: {})",
        bus,
        menu_path,
        revision,
        parsed_root.submenu.len()
    );

    Some(TrayMenu {
        submenus: parsed_root.submenu,
    })
}

pub(super) async fn process_and_track_item(
    conn: Connection,
    bus_name: String,
    object_path: String,
    key: String,
    items_map: Arc<Mutex<TrayItemMap>>,
    subs_map: Arc<Mutex<Vec<async_channel::Sender<TrayEvent>>>>,
    _reg_items_set: Arc<Mutex<HashSet<String>>>,
) {
    let item = fetch_item_properties(&conn, &bus_name, &object_path).await;
    let menu = if let Some(ref m_path) = item.menu_path {
        fetch_menu_layout(&conn, &bus_name, m_path).await
    } else {
        None
    };

    log_info!(
        "tray",
        "Discovered tray item: {} -> ID: '{}', Title: {:?}, Icon: {:?}, Menu: {:?}",
        key,
        item.id,
        item.title,
        item.icon_name,
        item.menu_path
    );

    {
        let mut guard = crate::util::lock(&items_map);
        guard.insert(key.clone(), (item.clone(), menu.clone()));
    }

    TrayService::broadcast(&subs_map, TrayEvent::Add(key.clone(), Box::new(item.clone())));

    // Create Proxy signal listeners for PropertiesChanged & SNI signals
    let sni_proxy = zbus::proxy::Proxy::new(
        &conn,
        bus_name.as_str(),
        object_path.as_str(),
        "org.kde.StatusNotifierItem",
    )
    .await
    .ok();
    let mut stream_sni = if let Some(ref p) = sni_proxy {
        p.receive_all_signals().await.ok()
    } else {
        None
    };

    let props_proxy = zbus::proxy::Proxy::new(
        &conn,
        bus_name.as_str(),
        object_path.as_str(),
        "org.freedesktop.DBus.Properties",
    )
    .await
    .ok();
    let mut stream_props = if let Some(ref p) = props_proxy {
        p.receive_all_signals().await.ok()
    } else {
        None
    };

    let mut stream_menu = if let Some(ref m_path) = item.menu_path {
        let menu_proxy = zbus::proxy::Proxy::new(&conn, bus_name.as_str(), m_path.as_str(), "com.canonical.dbusmenu")
            .await
            .ok();
        if let Some(ref p) = menu_proxy {
            p.receive_all_signals().await.ok()
        } else {
            None
        }
    } else {
        None
    };

    use futures_util::StreamExt;
    loop {
        tokio::select! {
            Some(msg) = async {
                match stream_sni.as_mut() {
                    Some(s) => s.next().await,
                    None => std::future::pending().await,
                }
            } => {
                let hdr = msg.header();
                let member = hdr.member().map(|m| m.as_str()).unwrap_or("");
                log_debug!("tray", "Received SNI signal '{}' for item {}", member, key);
                let fresh_item = fetch_item_properties(&conn, &bus_name, &object_path).await;
                {
                    let mut guard = crate::util::lock(&items_map);
                    if let Some((it, _)) = guard.get_mut(&key) {
                        *it = fresh_item.clone();
                    }
                }
                TrayService::broadcast(&subs_map, TrayEvent::Update(key.clone(), Box::new(fresh_item)));
            }
            Some(msg) = async {
                match stream_props.as_mut() {
                    Some(s) => s.next().await,
                    None => std::future::pending().await,
                }
            } => {
                let hdr = msg.header();
                let member = hdr.member().map(|m| m.as_str()).unwrap_or("");
                if member == "PropertiesChanged" {
                    log_debug!("tray", "Received PropertiesChanged for item {}", key);
                    let fresh_item = fetch_item_properties(&conn, &bus_name, &object_path).await;
                    {
                        let mut guard = crate::util::lock(&items_map);
                        if let Some((it, _)) = guard.get_mut(&key) {
                            *it = fresh_item.clone();
                        }
                    }
                    TrayService::broadcast(&subs_map, TrayEvent::Update(key.clone(), Box::new(fresh_item)));
                }
            }
            Some(msg) = async {
                match stream_menu.as_mut() {
                    Some(s) => s.next().await,
                    None => std::future::pending().await,
                }
            } => {
                let hdr = msg.header();
                let member = hdr.member().map(|m| m.as_str()).unwrap_or("");
                if member == "LayoutUpdated" || member == "ItemsPropertiesUpdated" {
                    log_debug!("tray", "Received DBusMenu signal '{}' for item {}", member, key);
                    let m_path_opt = {
                        let guard = crate::util::lock(&items_map);
                        guard.get(&key).and_then(|(it, _)| it.menu_path.clone())
                    };
                    if let Some(m_path) = m_path_opt {
                        if let Some(fresh_menu) = fetch_menu_layout(&conn, &bus_name, &m_path).await {
                            let mut guard = crate::util::lock(&items_map);
                            if let Some((_, m)) = guard.get_mut(&key) {
                                *m = Some(fresh_menu);
                            }
                        } else {
                            // Transient GetLayout failure: one delayed retry
                            // (next signal also retries, so only one extra).
                            let conn_r = conn.clone();
                            let bus_r = bus_name.clone();
                            let path_r = m_path.clone();
                            let items_r = Arc::clone(&items_map);
                            let key_r = key.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                                if let Some(retry_menu) =
                                    fetch_menu_layout(&conn_r, &bus_r, &path_r).await
                                {
                                    let mut guard = crate::util::lock(&items_r);
                                    if let Some((_, m)) = guard.get_mut(&key_r) {
                                        *m = Some(retry_menu);
                                    }
                                } else {
                                    log_debug!("tray", "Menu retry failed for {bus_r} {path_r}");
                                }
                            });
                        }
                    }
                }
            }
            else => break,
        }
    }
}
