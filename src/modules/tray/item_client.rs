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

async fn get_sni_prop(
    conn: &Connection,
    bus: &str,
    path: &str,
    iface: &str,
    prop: &str,
) -> Option<zvariant::OwnedValue> {
    let msg = conn
        .call_method(
            Some(bus),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(iface, prop),
        )
        .await
        .ok()?;
    msg.body().deserialize::<zvariant::OwnedValue>().ok()
}

async fn fetch_sni_icon_pixmap(
    conn: &Connection,
    bus: &str,
    path: &str,
    iface: &str,
) -> Option<Vec<super::types::TrayIconPixmap>> {
    let msg = conn
        .call_method(
            Some(bus),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(iface, "IconPixmap"),
        )
        .await
        .ok()?;
    let is_le = zvariant::Endian::from(msg.primary_header().endian_sig()) == zvariant::Endian::Little;
    let body = msg.body();
    let raw_bytes: &[u8] = body.data();
    super::protocol::parse_icon_pixmap_dbus_body(raw_bytes, is_le).or_else(|| {
        body.deserialize::<zvariant::OwnedValue>()
            .ok()
            .and_then(|v| parse_icon_pixmap(v.deref()))
    })
}

async fn fetch_item_properties(conn: &Connection, bus: &str, path: &str) -> TrayItem {
    let kde_iface = "org.kde.StatusNotifierItem";
    let fdo_iface = "org.freedesktop.StatusNotifierItem";

    let (iface, id_val) = match get_sni_prop(conn, bus, path, kde_iface, "Id").await {
        Some(v) => (kde_iface, Some(v)),
        None => match get_sni_prop(conn, bus, path, fdo_iface, "Id").await {
            Some(v) => (fdo_iface, Some(v)),
            None => (kde_iface, None),
        },
    };

    let (title_val, icon_name_val, theme_val, tooltip_val, is_menu_val, menu_val) = tokio::join!(
        get_sni_prop(conn, bus, path, iface, "Title"),
        get_sni_prop(conn, bus, path, iface, "IconName"),
        get_sni_prop(conn, bus, path, iface, "IconThemePath"),
        get_sni_prop(conn, bus, path, iface, "ToolTip"),
        get_sni_prop(conn, bus, path, iface, "ItemIsMenu"),
        get_sni_prop(conn, bus, path, iface, "Menu"),
    );

    let id = id_val
        .as_ref()
        .and_then(|v| match v.deref() {
            zvariant::Value::Str(s) => Some(s.as_str().to_string()),
            _ => None,
        })
        .unwrap_or_else(|| bus.to_string());

    let title = title_val.as_ref().and_then(|v| match v.deref() {
        zvariant::Value::Str(s) => Some(s.as_str().to_string()),
        _ => None,
    });

    let icon_name = icon_name_val
        .as_ref()
        .and_then(|v| match v.deref() {
            zvariant::Value::Str(s) => Some(s.as_str().to_string()),
            _ => None,
        })
        .filter(|s| !s.trim().is_empty());

    let icon_theme_path = theme_val
        .as_ref()
        .and_then(|v| match v.deref() {
            zvariant::Value::Str(s) => Some(s.as_str().to_string()),
            _ => None,
        })
        .filter(|s| !s.trim().is_empty());

    // Only fetch and parse IconPixmap when no usable IconName is provided, or as fallback
    let icon_pixmap = fetch_sni_icon_pixmap(conn, bus, path, iface).await;

    let mut props_map = HashMap::new();
    if let Some(tt) = tooltip_val {
        props_map.insert("ToolTip".to_string(), tt);
    }
    let tool_tip = parse_tooltip(&props_map);

    let item_is_menu = is_menu_val
        .as_ref()
        .and_then(|v| match v.deref() {
            zvariant::Value::Bool(b) => Some(*b),
            zvariant::Value::I32(i) => Some(*i != 0),
            zvariant::Value::U32(u) => Some(*u != 0),
            _ => None,
        })
        .unwrap_or(false);

    let menu_path = menu_val
        .as_ref()
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

    async fn build_signal_proxy(
        conn: &Connection,
        bus: &str,
        path: &str,
        iface: &'static str,
    ) -> Option<zbus::proxy::Proxy<'static>> {
        zbus::proxy::Builder::new(conn)
            .destination(bus.to_string())
            .ok()?
            .path(path.to_string())
            .ok()?
            .interface(iface)
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .ok()
    }

    // Create Proxy signal listeners for PropertiesChanged & SNI signals
    // without property caching (CacheProperties::Yes would call GetAll and
    // hold multi-megabyte IconPixmap payloads in memory forever).
    let sni_proxy = build_signal_proxy(&conn, &bus_name, &object_path, "org.kde.StatusNotifierItem").await;
    let mut stream_sni = if let Some(ref p) = sni_proxy {
        p.receive_all_signals().await.ok()
    } else {
        None
    };

    let props_proxy = build_signal_proxy(&conn, &bus_name, &object_path, "org.freedesktop.DBus.Properties").await;
    let mut stream_props = if let Some(ref p) = props_proxy {
        p.receive_all_signals().await.ok()
    } else {
        None
    };

    let mut stream_menu = if let Some(ref m_path) = item.menu_path {
        let menu_proxy = build_signal_proxy(&conn, &bus_name, m_path, "com.canonical.dbusmenu").await;
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
