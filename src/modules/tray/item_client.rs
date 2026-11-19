use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::Connection;

use super::protocol::{parse_dbusmenu_tuple, parse_icon_pixmap_dbus_body, parse_tooltip};
use super::service::TrayService;
use super::types::{ActivateKind, ActivateRequest, TrayEvent, TrayItem, TrayItemMap, TrayMenu};
use crate::modules::dbus_get_property;

async fn try_sni_call<A: serde::Serialize + zbus::zvariant::DynamicType>(
    conn: &Connection,
    bus: &str,
    path: &str,
    iface: &str,
    method: &str,
    args: &A,
) -> Result<(), String> {
    match tokio::time::timeout(
        Duration::from_secs(1),
        conn.call_method(Some(bus), path, Some(iface), method, args),
    )
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("timed out".to_string()),
    }
}

struct ActivateCallPlan {
    primary_method: &'static str,
    fallback_iface: &'static str,
    fallback_method: &'static str,
    fallback_u32_zero: bool,
}

fn sni_activate_plan(kind: ActivateKind) -> ActivateCallPlan {
    match kind {
        ActivateKind::Default => ActivateCallPlan {
            primary_method: "Activate",
            fallback_iface: "org.freedesktop.StatusNotifierItem",
            fallback_method: "Activate",
            fallback_u32_zero: false,
        },
        ActivateKind::Secondary => ActivateCallPlan {
            primary_method: "SecondaryActivate",
            fallback_iface: "org.kde.StatusNotifierItem",
            fallback_method: "XAyatanaSecondaryActivate",
            fallback_u32_zero: true,
        },
        ActivateKind::ContextMenu => ActivateCallPlan {
            primary_method: "ContextMenu",
            fallback_iface: "org.kde.StatusNotifierItem",
            fallback_method: "SecondaryActivate",
            fallback_u32_zero: false,
        },
    }
}

pub(super) async fn handle_activate_request(conn: &Connection, req: ActivateRequest) {
    match req {
        ActivateRequest::Activate {
            kind,
            bus_name,
            object_path,
            x,
            y,
        } => {
            let plan = sni_activate_plan(kind);
            log_debug!("tray", "{} on tray item: {bus_name} at ({x}, {y})", plan.primary_method);
            if let Err(e) = try_sni_call(
                conn,
                &bus_name,
                &object_path,
                "org.kde.StatusNotifierItem",
                plan.primary_method,
                &(x, y),
            )
            .await
            {
                log_debug!(
                    "tray",
                    "{} failed for {bus_name}, trying {}: {e}",
                    plan.primary_method,
                    plan.fallback_method
                );
                let fallback_res = if plan.fallback_u32_zero {
                    try_sni_call(
                        conn,
                        &bus_name,
                        &object_path,
                        plan.fallback_iface,
                        plan.fallback_method,
                        &(0u32,),
                    )
                    .await
                } else {
                    try_sni_call(
                        conn,
                        &bus_name,
                        &object_path,
                        plan.fallback_iface,
                        plan.fallback_method,
                        &(x, y),
                    )
                    .await
                };
                if let Err(e2) = fallback_res {
                    log_warn!(
                        "tray",
                        "{} failed for {bus_name} {object_path}: {e2}",
                        plan.primary_method
                    );
                }
            }
        }
        ActivateRequest::Scroll {
            bus_name,
            object_path,
            delta,
            orientation,
        } => {
            if let Err(e) = try_sni_call(
                conn,
                &bus_name,
                &object_path,
                "org.kde.StatusNotifierItem",
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
            log_debug!("tray", "MenuItem clicked: {bus_name} -> {menu_path} (id: {submenu_id})");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32;
            if let Err(e) = try_sni_call(
                conn,
                &bus_name,
                &menu_path,
                "com.canonical.dbusmenu",
                "Event",
                &(submenu_id, "clicked", zbus::Variant::I32(0), now),
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
            if let Err(e) = try_sni_call(
                conn,
                &bus_name,
                &menu_path,
                "com.canonical.dbusmenu",
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

async fn fetch_sni_icon_pixmap(
    conn: &Connection,
    bus: &str,
    path: &str,
    iface: &str,
) -> Option<Vec<super::types::TrayIconPixmap>> {
    let msg = tokio::time::timeout(
        Duration::from_millis(500),
        conn.call_method(
            Some(bus),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(iface, "IconPixmap"),
        ),
    )
    .await
    .ok()?
    .ok()?;
    let is_le = zbus::wire::Endian::from(msg.primary_header().endian_sig()) == zbus::wire::Endian::Little;
    let body = msg.body();
    parse_icon_pixmap_dbus_body(body.data(), is_le)
}

fn variant_str(opt: Option<&zbus::OwnedVariant>) -> Option<String> {
    opt.and_then(|v| crate::modules::dbus_variant_str_or_path(v.deref()).map(str::to_string))
}

fn variant_is_menu(opt: Option<&zbus::OwnedVariant>) -> bool {
    opt.and_then(|v| crate::modules::dbus_variant_bool(v.deref()))
        .unwrap_or(false)
}

fn normalize_menu_path(opt: Option<&zbus::OwnedVariant>) -> Option<String> {
    variant_str(opt).filter(|p| !p.is_empty() && p != "/NO_DBUSMENU" && p != "/")
}

struct RawSniProps {
    id: Option<zbus::OwnedVariant>,
    title: Option<zbus::OwnedVariant>,
    icon_name: Option<zbus::OwnedVariant>,
    icon_theme_path: Option<zbus::OwnedVariant>,
    icon_pixmap: Option<Vec<super::types::TrayIconPixmap>>,
    tool_tip: Option<zbus::OwnedVariant>,
    item_is_menu: Option<zbus::OwnedVariant>,
    menu_path: Option<zbus::OwnedVariant>,
}

impl RawSniProps {
    fn has_any_data(&self) -> bool {
        self.id.is_some()
            || self.title.is_some()
            || self.icon_name.is_some()
            || self.icon_theme_path.is_some()
            || self.icon_pixmap.is_some()
            || self.tool_tip.is_some()
            || self.item_is_menu.is_some()
            || self.menu_path.is_some()
    }
}

fn assemble_tray_item(bus: &str, path: &str, props: RawSniProps) -> TrayItem {
    TrayItem {
        bus_name: bus.to_string(),
        object_path: path.to_string(),
        id: variant_str(props.id.as_ref()).unwrap_or_else(|| bus.to_string()),
        title: variant_str(props.title.as_ref()),
        icon_name: variant_str(props.icon_name.as_ref()).filter(|s| !s.trim().is_empty()),
        icon_theme_path: variant_str(props.icon_theme_path.as_ref()).filter(|s| !s.trim().is_empty()),
        icon_pixmap: props.icon_pixmap,
        tool_tip: props.tool_tip.as_ref().and_then(|v| parse_tooltip(v.deref())),
        item_is_menu: variant_is_menu(props.item_is_menu.as_ref()),
        menu_path: normalize_menu_path(props.menu_path.as_ref()),
    }
}

async fn fetch_item_properties(
    conn: &Connection,
    bus: &str,
    path: &str,
    fetch_pixmap: bool,
) -> Option<(TrayItem, &'static str)> {
    let kde_iface = "org.kde.StatusNotifierItem";
    let fdo_iface = "org.freedesktop.StatusNotifierItem";

    let (iface, id_val) = match dbus_get_property(conn, bus, path, kde_iface, "Id").await {
        Some(v) => (kde_iface, Some(v)),
        None => match dbus_get_property(conn, bus, path, fdo_iface, "Id").await {
            Some(v) => (fdo_iface, Some(v)),
            None => (kde_iface, None),
        },
    };

    let (title_val, icon_name_val, theme_val, tooltip_val, is_menu_val, menu_val) = tokio::join!(
        dbus_get_property(conn, bus, path, iface, "Title"),
        dbus_get_property(conn, bus, path, iface, "IconName"),
        dbus_get_property(conn, bus, path, iface, "IconThemePath"),
        dbus_get_property(conn, bus, path, iface, "ToolTip"),
        dbus_get_property(conn, bus, path, iface, "ItemIsMenu"),
        dbus_get_property(conn, bus, path, iface, "Menu"),
    );

    let icon_pixmap = if fetch_pixmap {
        fetch_sni_icon_pixmap(conn, bus, path, iface).await
    } else {
        None
    };

    let props = RawSniProps {
        id: id_val,
        title: title_val,
        icon_name: icon_name_val,
        icon_theme_path: theme_val,
        icon_pixmap,
        tool_tip: tooltip_val,
        item_is_menu: is_menu_val,
        menu_path: menu_val,
    };

    if !props.has_any_data() {
        if dbus_get_property(conn, bus, path, kde_iface, "Status").await.is_some() {
            return Some((assemble_tray_item(bus, path, props), kde_iface));
        }
        return None;
    }

    Some((assemble_tray_item(bus, path, props), iface))
}

type RawMenuTuple = (i32, HashMap<String, zbus::OwnedVariant>, Vec<zbus::OwnedVariant>);

async fn fetch_menu_layout(conn: &Connection, bus: &str, menu_path: &str) -> Option<TrayMenu> {
    let empty_props: Vec<String> = Vec::new();
    let msg = match tokio::time::timeout(
        Duration::from_millis(500),
        conn.call_method(
            Some(bus),
            menu_path,
            Some("com.canonical.dbusmenu"),
            "GetLayout",
            &(0i32, -1i32, empty_props),
        ),
    )
    .await
    {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => {
            log_warn!("tray", "Failed to call GetLayout on {bus} {menu_path}: {e}");
            return None;
        }
        Err(_) => {
            log_warn!("tray", "GetLayout timed out on {bus} {menu_path}");
            return None;
        }
    };

    let (revision, (id, props, children)): (u32, RawMenuTuple) = match msg.body().deserialize() {
        Ok(pair) => pair,
        Err(e) => {
            log_warn!(
                "tray",
                "Failed to deserialize GetLayout response from {bus} {menu_path}: {e}"
            );
            return None;
        }
    };

    let parsed_root = parse_dbusmenu_tuple(id, &props, &children);
    log_info!(
        "tray",
        "Successfully parsed DBusMenu for {bus} {menu_path} (revision {revision}, items: {})",
        parsed_root.submenu.len()
    );

    Some(TrayMenu {
        submenus: parsed_root.submenu,
    })
}

async fn signal_stream(
    conn: &Connection,
    bus: &str,
    path: &str,
    iface: &'static str,
) -> Option<zbus::proxy::SignalStream<'static>> {
    // CacheProperties::No avoids GetAll caching multi-megabyte IconPixmap payloads forever.
    let proxy = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(conn)
        .destination(bus.to_string())
        .path(path.to_string())
        .interface(iface)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    proxy.receive_all_signals().await.ok()
}

#[derive(Debug, PartialEq, Eq)]
enum ThreeStreamEvent<A, B, C> {
    First(A),
    Second(B),
    Third(C),
}

async fn next_from_three_streams<S1, S2, S3>(
    s1: &mut Option<S1>,
    s2: &mut Option<S2>,
    s3: &mut Option<S3>,
) -> Option<ThreeStreamEvent<S1::Item, S2::Item, S3::Item>>
where
    S1: futures_util::Stream + Unpin,
    S2: futures_util::Stream + Unpin,
    S3: futures_util::Stream + Unpin,
{
    use futures_util::StreamExt;
    async fn next_opt<S: futures_util::Stream + Unpin>(opt: Option<&mut S>) -> Option<S::Item> {
        match opt {
            Some(s) => s.next().await,
            None => std::future::pending().await,
        }
    }
    loop {
        tokio::select! {
            res = next_opt(s1.as_mut()), if s1.is_some() => match res {
                Some(v) => return Some(ThreeStreamEvent::First(v)),
                None => *s1 = None,
            },
            res = next_opt(s2.as_mut()), if s2.is_some() => match res {
                Some(v) => return Some(ThreeStreamEvent::Second(v)),
                None => *s2 = None,
            },
            res = next_opt(s3.as_mut()), if s3.is_some() => match res {
                Some(v) => return Some(ThreeStreamEvent::Third(v)),
                None => *s3 = None,
            },
            else => return None,
        }
    }
}

fn apply_refreshed_item(items_map: &Arc<Mutex<TrayItemMap>>, key: &str, fresh_item: &TrayItem) {
    let mut guard = crate::util::lock(items_map);
    if let Some((it, _)) = guard.get_mut(key) {
        *it = fresh_item.clone();
    }
}

fn is_menu_layout_signal(member: &str) -> bool {
    member == "LayoutUpdated" || member == "ItemsPropertiesUpdated"
}

async fn refresh_item_and_broadcast(
    conn: &Connection,
    bus: &str,
    path: &str,
    key: &str,
    items_map: &Arc<Mutex<TrayItemMap>>,
    subs_map: &Arc<Mutex<Vec<async_channel::Sender<TrayEvent>>>>,
    fetch_pixmap: bool,
) -> Option<TrayItem> {
    let (mut fresh_item, _) = fetch_item_properties(conn, bus, path, fetch_pixmap).await?;
    if !fetch_pixmap {
        let guard = crate::util::lock(items_map);
        if let Some((existing, _)) = guard.get(key) {
            fresh_item.icon_pixmap.clone_from(&existing.icon_pixmap);
        }
    }
    apply_refreshed_item(items_map, key, &fresh_item);
    TrayService::broadcast(
        subs_map,
        TrayEvent::Update(key.to_string(), Box::new(fresh_item.clone())),
    );
    Some(fresh_item)
}

async fn sync_menu_stream_if_changed(
    conn: &Connection,
    bus_name: &str,
    key: &str,
    items_map: &Arc<Mutex<TrayItemMap>>,
    current_menu_path: &mut Option<String>,
    stream_menu: &mut Option<zbus::proxy::SignalStream<'static>>,
    new_menu_path: Option<String>,
) {
    if new_menu_path == *current_menu_path {
        return;
    }
    *current_menu_path = new_menu_path;
    match current_menu_path {
        Some(m_path) => {
            *stream_menu = signal_stream(conn, bus_name, m_path, "com.canonical.dbusmenu").await;
            if let Some(fresh_menu) = fetch_menu_layout(conn, bus_name, m_path).await {
                if let Some((_, m)) = crate::util::lock(items_map).get_mut(key) {
                    *m = Some(fresh_menu);
                }
            }
        }
        None => {
            *stream_menu = None;
            if let Some((_, m)) = crate::util::lock(items_map).get_mut(key) {
                *m = None;
            }
        }
    }
}

pub(super) async fn process_and_track_item(
    conn: Connection,
    bus_name: String,
    object_path: String,
    key: String,
    items_map: Arc<Mutex<TrayItemMap>>,
    subs_map: Arc<Mutex<Vec<async_channel::Sender<TrayEvent>>>>,
    registered_items: Arc<Mutex<HashSet<String>>>,
) {
    let Some((item, sni_iface)) = fetch_item_properties(&conn, &bus_name, &object_path, true).await else {
        log_debug!("tray", "Ignoring unreachable or disconnected tray item: {key}");
        crate::util::lock(&registered_items).remove(&key);
        return;
    };
    if !crate::util::lock(&registered_items).contains(&key) {
        return;
    }

    let menu = if let Some(ref m_path) = item.menu_path {
        fetch_menu_layout(&conn, &bus_name, m_path).await
    } else {
        None
    };

    if !crate::util::lock(&registered_items).contains(&key) {
        return;
    }

    log_info!(
        "tray",
        "Discovered tray item: {} -> ID: '{}', Title: {:?}, Icon: {:?}, Menu: {:?}",
        key,
        item.id,
        item.title,
        item.icon_name,
        item.menu_path
    );

    let mut current_menu_path = item.menu_path.clone();
    crate::util::lock(&items_map).insert(key.clone(), (item.clone(), menu));
    TrayService::broadcast(&subs_map, TrayEvent::Add(key.clone(), Box::new(item.clone())));

    let mut stream_sni = signal_stream(&conn, &bus_name, &object_path, sni_iface).await;
    let mut stream_props = signal_stream(&conn, &bus_name, &object_path, "org.freedesktop.DBus.Properties").await;
    let mut stream_menu = if let Some(ref m_path) = current_menu_path {
        signal_stream(&conn, &bus_name, m_path, "com.canonical.dbusmenu").await
    } else {
        None
    };

    while let Some(ev) = next_from_three_streams(&mut stream_sni, &mut stream_props, &mut stream_menu).await {
        match ev {
            ThreeStreamEvent::First(msg) => {
                let hdr = msg.header();
                let member = hdr.member().map(|m| m.as_str()).unwrap_or("");
                log_debug!("tray", "Received SNI signal '{member}' for item {key}");
                let fetch_pixmap = !matches!(member, "NewTitle" | "NewToolTip");
                if let Some(fresh) = refresh_item_and_broadcast(
                    &conn,
                    &bus_name,
                    &object_path,
                    &key,
                    &items_map,
                    &subs_map,
                    fetch_pixmap,
                )
                .await
                {
                    sync_menu_stream_if_changed(
                        &conn,
                        &bus_name,
                        &key,
                        &items_map,
                        &mut current_menu_path,
                        &mut stream_menu,
                        fresh.menu_path,
                    )
                    .await;
                }
            }
            ThreeStreamEvent::Second(msg) => {
                if msg.header().member().is_some_and(|m| m.as_str() == "PropertiesChanged") {
                    log_debug!("tray", "Received PropertiesChanged for item {key}");
                    if let Some(fresh) =
                        refresh_item_and_broadcast(&conn, &bus_name, &object_path, &key, &items_map, &subs_map, true)
                            .await
                    {
                        sync_menu_stream_if_changed(
                            &conn,
                            &bus_name,
                            &key,
                            &items_map,
                            &mut current_menu_path,
                            &mut stream_menu,
                            fresh.menu_path,
                        )
                        .await;
                    }
                }
            }
            ThreeStreamEvent::Third(msg) => {
                let hdr = msg.header();
                let member = hdr.member().map(|m| m.as_str()).unwrap_or("");
                if is_menu_layout_signal(member) {
                    log_debug!("tray", "Received DBusMenu signal '{member}' for item {key}");
                    let m_path_opt = crate::util::lock(&items_map)
                        .get(&key)
                        .and_then(|(it, _)| it.menu_path.clone());
                    if let Some(m_path) = m_path_opt {
                        if let Some(fresh_menu) = fetch_menu_layout(&conn, &bus_name, &m_path).await {
                            if let Some((_, m)) = crate::util::lock(&items_map).get_mut(&key) {
                                *m = Some(fresh_menu);
                            }
                        } else {
                            // Transient GetLayout failure: one delayed retry.
                            let conn_r = conn.clone();
                            let bus_r = bus_name.clone();
                            let path_r = m_path.clone();
                            let items_r = Arc::clone(&items_map);
                            let key_r = key.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_millis(500)).await;
                                if let Some(retry_menu) = fetch_menu_layout(&conn_r, &bus_r, &path_r).await {
                                    if let Some((_, m)) = crate::util::lock(&items_r).get_mut(&key_r) {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::OwnedVariant;

    fn ov_str(s: &str) -> Option<OwnedVariant> {
        Some(OwnedVariant::try_from(zbus::Variant::from(s)).unwrap())
    }

    fn ov_bool(b: bool) -> Option<OwnedVariant> {
        Some(OwnedVariant::try_from(zbus::Variant::from(b)).unwrap())
    }

    fn ov_i32(i: i32) -> Option<OwnedVariant> {
        Some(OwnedVariant::try_from(zbus::Variant::from(i)).unwrap())
    }

    fn ov_u32(u: u32) -> Option<OwnedVariant> {
        Some(OwnedVariant::try_from(zbus::Variant::from(u)).unwrap())
    }

    #[test]
    fn test_sni_activate_plan_for_all_kinds() {
        let def = sni_activate_plan(ActivateKind::Default);
        assert_eq!(def.primary_method, "Activate");
        assert_eq!(def.fallback_iface, "org.freedesktop.StatusNotifierItem");
        assert_eq!(def.fallback_method, "Activate");
        assert!(!def.fallback_u32_zero);

        let sec = sni_activate_plan(ActivateKind::Secondary);
        assert_eq!(sec.primary_method, "SecondaryActivate");
        assert_eq!(sec.fallback_iface, "org.kde.StatusNotifierItem");
        assert_eq!(sec.fallback_method, "XAyatanaSecondaryActivate");
        assert!(sec.fallback_u32_zero);

        let menu = sni_activate_plan(ActivateKind::ContextMenu);
        assert_eq!(menu.primary_method, "ContextMenu");
        assert_eq!(menu.fallback_iface, "org.kde.StatusNotifierItem");
        assert_eq!(menu.fallback_method, "SecondaryActivate");
        assert!(!menu.fallback_u32_zero);
    }

    #[test]
    fn test_variant_extractors_and_menu_path_normalization() {
        let obj = OwnedVariant::try_from(zbus::Variant::from(
            zbus::ObjectPath::try_from("/org/example/Menu").unwrap(),
        ))
        .unwrap();
        assert_eq!(variant_str(Some(&obj)).as_deref(), Some("/org/example/Menu"));
        assert_eq!(variant_str(ov_str("hello").as_ref()).as_deref(), Some("hello"));
        assert_eq!(variant_str(ov_i32(7).as_ref()), None);

        assert!(variant_is_menu(ov_bool(true).as_ref()));
        assert!(!variant_is_menu(ov_bool(false).as_ref()));
        assert!(variant_is_menu(ov_i32(1).as_ref()));
        assert!(!variant_is_menu(ov_i32(0).as_ref()));
        assert!(variant_is_menu(ov_u32(2).as_ref()));
        assert!(!variant_is_menu(ov_str("true").as_ref()));
        assert!(!variant_is_menu(None));

        assert_eq!(normalize_menu_path(ov_str("").as_ref()), None);
        assert_eq!(normalize_menu_path(ov_str("/").as_ref()), None);
        assert_eq!(normalize_menu_path(ov_str("/NO_DBUSMENU").as_ref()), None);
        assert_eq!(normalize_menu_path(Some(&obj)).as_deref(), Some("/org/example/Menu"));
    }

    #[test]
    fn test_assemble_tray_item_defaults_and_overrides() {
        let item = assemble_tray_item(
            ":1.42",
            "/StatusNotifierItem",
            RawSniProps {
                id: None,
                title: ov_str("Example Tray"),
                icon_name: ov_str("   "),
                icon_theme_path: ov_str("\t"),
                icon_pixmap: None,
                tool_tip: None,
                item_is_menu: ov_u32(1),
                menu_path: ov_str("/NO_DBUSMENU"),
            },
        );
        assert_eq!(item.bus_name, ":1.42");
        assert_eq!(item.object_path, "/StatusNotifierItem");
        assert_eq!(item.id, ":1.42", "Missing Id must fall back to bus name");
        assert_eq!(item.title.as_deref(), Some("Example Tray"));
        assert_eq!(item.icon_name, None, "Blank IconName must become None");
        assert_eq!(item.icon_theme_path, None, "Blank IconThemePath must become None");
        assert!(item.item_is_menu);
        assert_eq!(item.menu_path, None);
    }

    #[test]
    fn test_apply_refreshed_item_and_signal_filters() {
        let items: Arc<Mutex<TrayItemMap>> = Arc::new(Mutex::new(HashMap::new()));
        let key = ":1.42/StatusNotifierItem";
        let initial = TrayItem {
            bus_name: ":1.42".into(),
            object_path: "/StatusNotifierItem".into(),
            id: "old".into(),
            title: Some("Old".into()),
            icon_name: None,
            icon_theme_path: None,
            icon_pixmap: None,
            tool_tip: None,
            item_is_menu: false,
            menu_path: Some("/Menu".into()),
        };
        let menu = TrayMenu { submenus: vec![] };
        crate::util::lock(&items).insert(key.to_string(), (initial.clone(), Some(menu)));

        let mut updated = initial;
        updated.title = Some("Updated".into());
        apply_refreshed_item(&items, key, &updated);
        apply_refreshed_item(&items, ":1.99/removed", &updated);

        let guard = crate::util::lock(&items);
        let (stored, stored_menu) = guard.get(key).expect("existing item kept");
        assert_eq!(stored.title.as_deref(), Some("Updated"));
        assert!(stored_menu.is_some(), "Refreshing item properties must keep menu cache");
        assert!(!guard.contains_key(":1.99/removed"));

        assert!(is_menu_layout_signal("LayoutUpdated"));
        assert!(is_menu_layout_signal("ItemsPropertiesUpdated"));
        assert!(!is_menu_layout_signal("ActivationRequested"));
    }

    #[test]
    fn test_process_and_track_item_drops_disconnected_peer_without_adding_ghost_item() {
        crate::runtime::handle().block_on(async {
            let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
            let conn = zbus::connection::Builder::authenticated_socket(
                super::super::test_socket::TestUnixSocket::new(client_stream),
                "0123456789abcdef0123456789abcdef",
            )
            .build()
            .await
            .unwrap();
            drop(server_stream);

            let key = ":1.99/StatusNotifierItem".to_string();
            let items_map: Arc<Mutex<TrayItemMap>> = Arc::new(Mutex::new(HashMap::new()));
            let (tx, rx) = async_channel::unbounded();
            let subs_map: Arc<Mutex<Vec<async_channel::Sender<TrayEvent>>>> = Arc::new(Mutex::new(vec![tx]));
            let registered_items = Arc::new(Mutex::new(std::collections::HashSet::from([key.clone()])));

            process_and_track_item(
                conn,
                ":1.99".to_string(),
                "/StatusNotifierItem".to_string(),
                key.clone(),
                Arc::clone(&items_map),
                subs_map,
                Arc::clone(&registered_items),
            )
            .await;

            assert!(
                !crate::util::lock(&items_map).contains_key(&key),
                "Disconnected peer must not produce a ghost item in items_map"
            );
            assert!(
                !crate::util::lock(&registered_items).contains(&key),
                "Disconnected peer key must be cleared from registered_items"
            );
            assert!(
                rx.try_recv().is_err(),
                "Disconnected peer must not broadcast TrayEvent::Add"
            );
        });
    }

    struct CountingEmptyStream {
        polls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl futures_util::Stream for CountingEmptyStream {
        type Item = u8;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            self.polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::task::Poll::Ready(None)
        }
    }

    #[test]
    fn test_stream_loop_exits_when_optional_stream_is_none_and_active_streams_end() {
        crate::runtime::handle().block_on(async {
            let mut s1 = Some(futures_util::stream::iter(vec![1u8]));
            let mut s2 = Some(futures_util::stream::iter(Vec::<u8>::new()));
            let mut s3: Option<futures_util::stream::Iter<std::vec::IntoIter<u8>>> = None;

            let mut received = Vec::new();
            let res = tokio::time::timeout(Duration::from_millis(200), async {
                while let Some(ev) = next_from_three_streams(&mut s1, &mut s2, &mut s3).await {
                    received.push(ev);
                }
            })
            .await;

            assert!(
                res.is_ok(),
                "Stream loop must exit once active streams end even when s3 is None"
            );
            assert_eq!(received, vec![ThreeStreamEvent::First(1)]);
        });
    }

    #[test]
    fn test_stream_loop_does_not_repoll_exhausted_stream() {
        crate::runtime::handle().block_on(async {
            let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut s1 = Some(CountingEmptyStream {
                polls: Arc::clone(&polls),
            });
            let (tx2, rx2) = async_channel::unbounded::<u8>();
            let mut s2 = Some(Box::pin(rx2));
            let mut s3: Option<CountingEmptyStream> = None;

            tx2.send(10).await.unwrap();
            tx2.send(20).await.unwrap();
            tx2.close();

            let mut received = Vec::new();
            let res = tokio::time::timeout(Duration::from_millis(200), async {
                while let Some(ev) = next_from_three_streams(&mut s1, &mut s2, &mut s3).await {
                    if let ThreeStreamEvent::Second(v) = ev {
                        received.push(v);
                    }
                }
            })
            .await;

            assert!(res.is_ok(), "Stream loop must exit once all streams terminate");
            assert_eq!(received, vec![10, 20]);
            assert_eq!(
                polls.load(std::sync::atomic::Ordering::Relaxed),
                1,
                "Exhausted stream must be cleared and polled only once"
            );
        });
    }

    #[test]
    fn test_variant_extractors_unwrap_nested_variants() {
        let nested_str =
            zbus::OwnedVariant::try_from(zbus::Variant::Variant(Box::new(zbus::Variant::from("nested-icon")))).unwrap();
        assert_eq!(variant_str(Some(&nested_str)).as_deref(), Some("nested-icon"));

        let nested_bool =
            zbus::OwnedVariant::try_from(zbus::Variant::Variant(Box::new(zbus::Variant::from(true)))).unwrap();
        assert!(variant_is_menu(Some(&nested_bool)));
    }
}
