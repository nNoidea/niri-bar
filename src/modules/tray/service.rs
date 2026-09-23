use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::Connection;

use super::protocol::{parse_icon_pixmap, parse_item_address, parse_raw_menu_item, parse_tooltip, RawMenuItem};
use super::types::{ActivateRequest, TrayEvent, TrayItem, TrayItemMap, TrayItemSnapshot, TrayMenu};

/// DBus Watcher Object exporting org.kde.StatusNotifierWatcher
struct StatusNotifierWatcher {
    registered_items: Arc<Mutex<HashSet<String>>>,
    registered_hosts: Arc<Mutex<HashSet<String>>>,
    item_registered_tx: tokio::sync::mpsc::Sender<String>,
}

#[interface(name = "org.kde.StatusNotifierWatcher")]
impl StatusNotifierWatcher {
    async fn register_status_notifier_item(
        &mut self,
        service: &str,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = hdr.sender().map(|s| s.as_str()).unwrap_or("");
        let (bus, path, _) = parse_item_address(service, Some(sender));
        let effective_bus = if !sender.is_empty() { sender } else { &bus };
        let key = format!("{}{}", effective_bus, path);

        log_info!(
            "tray",
            "Watcher: RegisterStatusNotifierItem requested: {} (resolved key: {})",
            service,
            key
        );

        {
            let mut items = crate::util::lock(&self.registered_items);
            if !items.insert(key.clone()) {
                return Ok(()); // Already registered
            }
        }

        let _ = Self::status_notifier_item_registered(&emitter, &key).await;
        let _ = self.registered_status_notifier_items_changed(&emitter).await;
        // Signal emissions have no listeners when nobody watches — silent by
        // convention. The registration itself must not be lost, so log it.
        if let Err(e) = self.item_registered_tx.try_send(key.clone()) {
            log_warn!("tray", "Dropped registration for {key}: {e}");
        }

        Ok(())
    }

    async fn register_status_notifier_host(
        &mut self,
        service: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        log_info!("tray", "Watcher: RegisterStatusNotifierHost requested: {}", service);
        {
            let mut hosts = crate::util::lock(&self.registered_hosts);
            hosts.insert(service.to_string());
        }
        let _ = Self::status_notifier_host_registered(&emitter).await;
        let _ = self.is_status_notifier_host_registered_changed(&emitter).await;
        Ok(())
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        let items = crate::util::lock(&self.registered_items);
        items.iter().cloned().collect()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(emitter: &SignalEmitter<'_>, service: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(emitter: &SignalEmitter<'_>, service: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

pub struct TrayService {
    subscribers: Arc<Mutex<Vec<async_channel::Sender<TrayEvent>>>>,
    act_tx: tokio::sync::mpsc::Sender<ActivateRequest>,
    items: Arc<Mutex<TrayItemMap>>,
}

impl TrayService {
    /// Single shared instance (see `SharedModules`): spawns one background
    /// thread with its own tokio runtime for the session bus.
    /// Do not construct per-monitor.
    pub fn new() -> Self {
        let (act_tx, mut act_rx) = tokio::sync::mpsc::channel::<ActivateRequest>(128);
        let items = Arc::new(Mutex::new(HashMap::new()));
        let subscribers: Arc<Mutex<Vec<async_channel::Sender<TrayEvent>>>> = Arc::new(Mutex::new(Vec::new()));

        let items_clone = Arc::clone(&items);
        let subs_clone = Arc::clone(&subscribers);

        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(r) => r,
                Err(e) => {
                    log_warn!("tray", "Failed to build tokio runtime for tray: {}", e);
                    return;
                }
            };

            // Current live connection for activation requests. Updated on
            // every (re)connect so GTK clicks survive bus restarts.
            let current_conn: Arc<tokio::sync::Mutex<Option<Connection>>> = Arc::new(tokio::sync::Mutex::new(None));
            let conn_for_act = Arc::clone(&current_conn);
            rt.spawn(async move {
                while let Some(req) = act_rx.recv().await {
                    let conn_opt = conn_for_act.lock().await.clone();
                    if let Some(c) = conn_opt {
                        handle_activate_request(&c, req).await;
                    } else {
                        log_debug!("tray", "Activation dropped: no live session bus");
                    }
                }
            });

            // Per-item signal tasks across reconnects; aborted on Remove
            // and on session loss.
            let item_tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>> =
                Arc::new(Mutex::new(HashMap::new()));
            // Bound login-storm probe fan-out.
            let probe_sem: Arc<tokio::sync::Semaphore> = Arc::new(tokio::sync::Semaphore::new(8));

            let mut backoff_secs: u64 = 1;
            loop {
                let session_result = rt.block_on(run_tray_session(
                    Arc::clone(&items_clone),
                    Arc::clone(&subs_clone),
                    Arc::clone(&current_conn),
                    Arc::clone(&item_tasks),
                    Arc::clone(&probe_sem),
                ));
                // Session ended (bus loss or fatal setup error): drop stale
                // tasks holding dead proxies, clear live conn, back off.
                {
                    let mut tasks = crate::util::lock(&item_tasks);
                    for (_, h) in tasks.drain() {
                        h.abort();
                    }
                }
                *current_conn.blocking_lock() = None;
                match session_result {
                    None => {
                        // Clean shutdown path is unreachable today (no stop
                        // flag); treat as reconnectable.
                        log_warn!("tray", "Tray session ended; reconnecting in {backoff_secs}s");
                    }
                    Some(reason) => {
                        log_warn!("tray", "Tray session lost ({reason}); reconnecting in {backoff_secs}s");
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(backoff_secs));
                backoff_secs = (backoff_secs * 2).min(30);
            }
        });

        Self {
            subscribers,
            act_tx,
            items,
        }
    }

    fn broadcast(subscribers: &Arc<Mutex<Vec<async_channel::Sender<TrayEvent>>>>, ev: TrayEvent) {
        let mut subs = crate::util::lock(subscribers);
        subs.retain(|tx| tx.try_send(ev.clone()).is_ok() || !tx.is_closed());
    }

    pub fn subscribe(&self) -> (async_channel::Receiver<TrayEvent>, Vec<TrayItemSnapshot>) {
        let (tx, rx) = async_channel::unbounded();
        let current_items = {
            let guard = crate::util::lock(&self.items);
            guard
                .iter()
                .map(|(k, (item, menu))| (k.clone(), item.clone(), menu.clone()))
                .collect()
        };

        crate::util::lock(&self.subscribers).push(tx);
        (rx, current_items)
    }

    pub fn send_activate(&self, req: ActivateRequest) {
        if let Err(e) = self.act_tx.try_send(req) {
            log_warn!("tray", "Dropped tray activation (channel full/closed): {e}");
        }
    }

    pub fn get_item_and_menu(&self, key: &str) -> (Option<TrayItem>, Option<TrayMenu>) {
        let guard = crate::util::lock(&self.items);
        if let Some((item, menu)) = guard.get(key) {
            (Some(item.clone()), menu.clone())
        } else {
            (None, None)
        }
    }
}

/// One live session-bus connection: watcher, probes, NameOwnerChanged.
/// Returns `None` on clean end (unreachable today) or `Some(reason)` when
/// the session was lost and the caller should back off and reconnect.
async fn run_tray_session(
    items: Arc<Mutex<TrayItemMap>>,
    subs: Arc<Mutex<Vec<async_channel::Sender<TrayEvent>>>>,
    current_conn: Arc<tokio::sync::Mutex<Option<Connection>>>,
    item_tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    probe_sem: Arc<tokio::sync::Semaphore>,
) -> Option<String> {
    let conn = match zbus::connection::Builder::session() {
        Ok(builder) => match builder.build().await {
            Ok(c) => c,
            Err(e) => {
                log_warn!("tray", "Failed to connect to DBus session: {}", e);
                return Some(format!("connect failed: {e}"));
            }
        },
        Err(e) => {
            log_warn!("tray", "Failed to create DBus builder: {}", e);
            return Some(format!("builder failed: {e}"));
        }
    };
    *current_conn.lock().await = Some(conn.clone());

    let registered_items: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let registered_hosts: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let (reg_item_tx, mut reg_item_rx) = tokio::sync::mpsc::channel::<String>(128);

    let watcher = StatusNotifierWatcher {
        registered_items: Arc::clone(&registered_items),
        registered_hosts: Arc::clone(&registered_hosts),
        item_registered_tx: reg_item_tx.clone(),
    };

    if let Err(e) = conn.object_server().at("/StatusNotifierWatcher", watcher).await {
        log_warn!("tray", "Failed to register /StatusNotifierWatcher object: {}", e);
    }

    // Request well-known names. Log failures: in multi-host setups
    // (e.g. another bar running) the name may already be owned.
    if let Err(e) = conn.request_name("org.kde.StatusNotifierWatcher").await {
        log_warn!("tray", "Could not own org.kde.StatusNotifierWatcher: {e}");
    }
    if let Err(e) = conn.request_name("org.freedesktop.StatusNotifierWatcher").await {
        log_warn!("tray", "Could not own org.freedesktop.StatusNotifierWatcher: {e}");
    }

    // Register host
    let pid = std::process::id();
    let host_name = format!("org.freedesktop.StatusNotifierHost-{pid}-1");
    if let Err(e) = conn.request_name(host_name.as_str()).await {
        log_warn!("tray", "Could not register host {host_name}: {e}");
    }

    // Spawn listener for newly registered items
    let conn_items = conn.clone();
    let items_map = Arc::clone(&items);
    let subs_map = Arc::clone(&subs);
    let reg_items_set = Arc::clone(&registered_items);
    let tasks_map = Arc::clone(&item_tasks);

    let reg_item_tx_clone = reg_item_tx.clone();
    tokio::spawn(async move {
        while let Some(key) = reg_item_rx.recv().await {
            let (bus_name, object_path, _) = parse_item_address(&key, None);
            if bus_name.is_empty() || object_path.is_empty() {
                continue;
            }

            let conn_c = conn_items.clone();
            let items_c = Arc::clone(&items_map);
            let subs_c = Arc::clone(&subs_map);
            let reg_set_c = Arc::clone(&reg_items_set);
            let tasks_c = Arc::clone(&tasks_map);
            let k = key.clone();

            // Replace any stale task for this key (re-registration).
            if let Some(old) = crate::util::lock(&tasks_c).remove(&k) {
                old.abort();
            }
            let k2 = k.clone();
            let handle = tokio::spawn(async move {
                process_and_track_item(conn_c, bus_name, object_path, k2, items_c, subs_c, reg_set_c).await;
            });
            crate::util::lock(&tasks_c).insert(k, handle);
        }
    });

    // Proactive discovery: Probe all already-running applications on DBus concurrently
    let conn_probe = conn.clone();
    let reg_tx_probe = reg_item_tx_clone.clone();
    let reg_items_probe = Arc::clone(&registered_items);
    let sem_probe = Arc::clone(&probe_sem);
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        probe_and_register_all(&conn_probe, &reg_tx_probe, &reg_items_probe, &sem_probe).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(450)).await;
        probe_and_register_all(&conn_probe, &reg_tx_probe, &reg_items_probe, &sem_probe).await;
    });

    // Reconcile after discovery settles: drop entries whose bus has no owner
    // (apps that died during an outage leave no NameOwnerChanged behind).
    {
        let conn_rec = conn.clone();
        let items_rec = Arc::clone(&items);
        let subs_rec = Arc::clone(&subs);
        let reg_rec = Arc::clone(&registered_items);
        let tasks_rec = Arc::clone(&item_tasks);
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            reconcile_item_owners(&conn_rec, &items_rec, &subs_rec, &reg_rec, &tasks_rec).await;
        });
    }

    // Listen to org.freedesktop.DBus NameOwnerChanged to handle app startup & clean termination
    let dbus_proxy = match zbus::fdo::DBusProxy::new(&conn).await {
        Ok(p) => p,
        Err(e) => {
            log_warn!("tray", "Failed to create DBusProxy: {}", e);
            return Some(format!("DBusProxy failed: {e}"));
        }
    };

    let mut name_owner_changed_stream = match dbus_proxy.receive_name_owner_changed().await {
        Ok(s) => s,
        Err(e) => {
            log_warn!("tray", "Failed to receive name owner changed signals: {}", e);
            return Some(format!("NameOwnerChanged subscribe failed: {e}"));
        }
    };

    use futures_util::StreamExt;
    while let Some(signal) = name_owner_changed_stream.next().await {
        if let Ok(args) = signal.args() {
            let name = args.name.as_str();
            let old_owner = args.old_owner.as_deref().unwrap_or("");
            let new_owner = args.new_owner.as_deref().unwrap_or("");

            if !old_owner.is_empty() && new_owner.is_empty() {
                // Process disconnected: remove all associated tray items
                let removed_keys: Vec<String> = {
                    let mut my_items = crate::util::lock(&items);
                    let mut to_remove = Vec::new();
                    for (k, (item, _)) in my_items.iter() {
                        if belongs_to_disconnected_owner(&item.bus_name, k, name, old_owner) {
                            to_remove.push(k.clone());
                        }
                    }
                    for k in &to_remove {
                        my_items.remove(k);
                    }
                    to_remove
                };

                for k in removed_keys {
                    log_info!("tray", "Process terminated, removing tray item: {}", k);
                    {
                        let mut reg = crate::util::lock(&registered_items);
                        reg.remove(&k);
                    }
                    if let Some(handle) = crate::util::lock(&item_tasks).remove(&k) {
                        handle.abort();
                    }
                    TrayService::broadcast(&subs, TrayEvent::Remove(k.clone()));
                }
            } else if old_owner.is_empty() && !new_owner.is_empty() {
                // Newly started application: probe if it exposes StatusNotifierItem
                if is_probe_candidate(name) {
                    let conn_new = conn.clone();
                    let reg_tx_new = reg_item_tx_clone.clone();
                    let reg_items_new = Arc::clone(&registered_items);
                    let n = name.to_string();
                    tokio::spawn(async move {
                        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
                        probe_single_service(&conn_new, &n, &reg_tx_new, &reg_items_new).await;
                    });
                }
            }
        }
    }
    Some("NameOwnerChanged stream ended (bus restart?)".to_string())
}

async fn handle_activate_request(conn: &Connection, req: ActivateRequest) {
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

async fn process_and_track_item(
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

/// Drop items whose bus currently has no owner.
///
/// Runs once per session after discovery settles. Apps that exit during a
/// bus outage (or between our probe and tracking) never emit
/// `NameOwnerChanged`, so without this their icons linger until restart.
async fn reconcile_item_owners(
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

async fn probe_and_register_all(
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
    item_bus == name || item_bus == old_owner || key.starts_with(name) || key.starts_with(old_owner)
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

async fn probe_single_service(
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
            let key = format!("{}{}", unique_bus, path);
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
    }
}
