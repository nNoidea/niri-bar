use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, Notify, Semaphore};
use tokio::task::JoinHandle;
use zbus::fdo::DBusProxy;
use zbus::object_server::SignalEmitter;
use zbus::{interface, Connection};

use super::item_client::{handle_activate_request, process_and_track_item};
use super::protocol::parse_item_address;
use super::types::{ActivateRequest, TrayEvent, TrayItem, TrayItemMap, TrayItemSnapshot, TrayMenu};

type RegTx = mpsc::Sender<(String, String, String)>;
type RegRx = mpsc::Receiver<(String, String, String)>;

pub struct TrayService {
    subscribers: Arc<Mutex<Vec<async_channel::Sender<TrayEvent>>>>,
    act_tx: mpsc::Sender<ActivateRequest>,
    items: Arc<Mutex<TrayItemMap>>,
    stopped: Arc<AtomicBool>,
    stop_notify: Arc<Notify>,
}

#[derive(Clone)]
struct SessionState {
    items: Arc<Mutex<TrayItemMap>>,
    subs: Arc<Mutex<Vec<async_channel::Sender<TrayEvent>>>>,
    registered_items: Arc<Mutex<HashSet<String>>>,
    item_tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    probe_sem: Arc<Semaphore>,
    stopped: Arc<AtomicBool>,
    stop_notify: Arc<Notify>,
}

impl SessionState {
    fn remove_item(&self, key: &str) {
        crate::util::lock(&self.items).remove(key);
        crate::util::lock(&self.registered_items).remove(key);
        if let Some(handle) = crate::util::lock(&self.item_tasks).remove(key) {
            handle.abort();
        }
        TrayService::broadcast(&self.subs, TrayEvent::Remove(key.to_string()));
    }

    fn abort_item_tasks(&self) {
        let mut tasks = crate::util::lock(&self.item_tasks);
        for (_, h) in tasks.drain() {
            h.abort();
        }
    }
}

struct AbortOnDrop(Vec<JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for h in self.0.drain(..) {
            h.abort();
        }
    }
}

impl Drop for TrayService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl TrayService {
    /// Single shared instance: spawns its supervisor task on the shared process-wide Tokio runtime.
    pub fn new() -> Self {
        let (act_tx, mut act_rx) = mpsc::channel::<ActivateRequest>(128);
        let items = Arc::new(Mutex::new(HashMap::new()));
        let subscribers: Arc<Mutex<Vec<async_channel::Sender<TrayEvent>>>> = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let stop_notify = Arc::new(Notify::new());

        let state = SessionState {
            items: Arc::clone(&items),
            subs: Arc::clone(&subscribers),
            registered_items: Arc::new(Mutex::new(HashSet::new())),
            item_tasks: Arc::new(Mutex::new(HashMap::new())),
            probe_sem: Arc::new(Semaphore::new(8)),
            stopped: Arc::clone(&stopped),
            stop_notify: Arc::clone(&stop_notify),
        };

        crate::runtime::spawn(async move {
            let mut backoff_secs: u64 = 1;
            while !state.stopped.load(Ordering::Acquire) {
                let session_result = run_tray_session(&state, &mut act_rx).await;
                state.abort_item_tasks();
                let Some(reason) = session_result else {
                    break;
                };
                log_warn!("tray", "Tray session lost ({reason}); reconnecting in {backoff_secs}s");
                let sleep = tokio::time::sleep(Duration::from_secs(backoff_secs));
                tokio::pin!(sleep);
                loop {
                    if state.stopped.load(Ordering::Acquire) {
                        return;
                    }
                    tokio::select! {
                        _ = state.stop_notify.notified() => return,
                        _ = &mut sleep => break,
                        maybe_req = act_rx.recv() => {
                            if maybe_req.is_none() {
                                return;
                            }
                            log_debug!("tray", "Activation dropped: no live session bus");
                        }
                    }
                }
                backoff_secs = (backoff_secs * 2).min(30);
            }
        });

        Self {
            subscribers,
            act_tx,
            items,
            stopped,
            stop_notify,
        }
    }

    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        self.stop_notify.notify_one();
    }

    pub(super) fn broadcast(subscribers: &Arc<Mutex<Vec<async_channel::Sender<TrayEvent>>>>, ev: TrayEvent) {
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

    pub fn get_item(&self, key: &str) -> Option<TrayItem> {
        crate::util::lock(&self.items).get(key).map(|(item, _)| item.clone())
    }

    pub fn get_item_and_menu(&self, key: &str) -> (Option<TrayItem>, Option<TrayMenu>) {
        let guard = crate::util::lock(&self.items);
        if let Some((item, menu)) = guard.get(key) {
            (Some(item.clone()), menu.clone())
        } else {
            (None, None)
        }
    }

    #[cfg(test)]
    pub fn new_for_test() -> Self {
        let (act_tx, _act_rx) = mpsc::channel::<ActivateRequest>(16);
        Self {
            subscribers: Arc::new(Mutex::new(Vec::new())),
            act_tx,
            items: Arc::new(Mutex::new(HashMap::new())),
            stopped: Arc::new(AtomicBool::new(false)),
            stop_notify: Arc::new(Notify::new()),
        }
    }

    #[cfg(test)]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub fn insert_test_item(&self, key: String, item: TrayItem) {
        crate::util::lock(&self.items).insert(key, (item, None));
    }

    #[cfg(test)]
    pub fn subscriber_count(&self) -> usize {
        crate::util::lock(&self.subscribers)
            .iter()
            .filter(|tx| !tx.is_closed())
            .count()
    }
}

/// DBus Watcher Object exporting `org.kde.StatusNotifierWatcher`
struct StatusNotifierWatcher {
    registered_items: Arc<Mutex<HashSet<String>>>,
    item_registered_tx: RegTx,
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
        let key = format!("{effective_bus}{path}");

        log_info!(
            "tray",
            "Watcher: RegisterStatusNotifierItem requested: {} (resolved key: {})",
            service,
            key
        );

        if !crate::util::lock(&self.registered_items).insert(key.clone()) {
            return Ok(());
        }

        let _ = Self::status_notifier_item_registered(&emitter, &key).await;
        let _ = self.registered_status_notifier_items_changed(&emitter).await;
        // Signal emissions have no listeners when nobody watches — silent by
        // convention. The registration itself must not be lost, so log it.
        if let Err(e) = self
            .item_registered_tx
            .try_send((effective_bus.to_string(), path, key.clone()))
        {
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
        let _ = Self::status_notifier_host_registered(&emitter).await;
        let _ = self.is_status_notifier_host_registered_changed(&emitter).await;
        Ok(())
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        crate::util::lock(&self.registered_items).iter().cloned().collect()
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
    async fn status_notifier_host_registered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// One live session-bus connection: watcher, probes, NameOwnerChanged.
/// Returns `None` on clean shutdown or `Some(reason)` when the session was lost.
async fn run_tray_session(state: &SessionState, act_rx: &mut mpsc::Receiver<ActivateRequest>) -> Option<String> {
    if state.stopped.load(Ordering::Acquire) {
        return None;
    }

    let conn = match zbus::connection::Builder::session().build().await {
        Ok(c) => c,
        Err(e) => {
            log_warn!("tray", "Failed to connect to DBus session: {}", e);
            return Some(format!("connect failed: {e}"));
        }
    };

    crate::util::lock(&state.registered_items).clear();
    let (reg_item_tx, mut reg_item_rx): (RegTx, RegRx) = mpsc::channel(128);

    let watcher = StatusNotifierWatcher {
        registered_items: Arc::clone(&state.registered_items),
        item_registered_tx: reg_item_tx.clone(),
    };

    if let Err(e) = conn.object_server().at("/StatusNotifierWatcher", watcher).await {
        log_warn!("tray", "Failed to register /StatusNotifierWatcher object: {}", e);
    }

    for name in ["org.kde.StatusNotifierWatcher", "org.freedesktop.StatusNotifierWatcher"] {
        if let Err(e) = conn.request_name(name).await {
            log_warn!("tray", "Could not own {name}: {e}");
        }
    }

    let host_name = format!("org.freedesktop.StatusNotifierHost-{}-1", std::process::id());
    if let Err(e) = conn.request_name(host_name.as_str()).await {
        log_warn!("tray", "Could not register host {host_name}: {e}");
    }

    let dbus_proxy = match DBusProxy::new(&conn).await {
        Ok(p) => Arc::new(p),
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

    let mut _session_tasks = AbortOnDrop(Vec::new());

    // Proactive discovery: Probe all already-running applications on DBus concurrently
    {
        let conn_p = conn.clone();
        let proxy_p = Arc::clone(&dbus_proxy);
        let reg_tx_p = reg_item_tx.clone();
        let state_p = state.clone();
        _session_tasks.0.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            probe_and_register_all(&conn_p, &proxy_p, &reg_tx_p, &state_p).await;
            tokio::time::sleep(Duration::from_millis(450)).await;
            probe_and_register_all(&conn_p, &proxy_p, &reg_tx_p, &state_p).await;
        }));
    }

    // Reconcile after discovery settles: drop entries whose bus has no owner
    // (apps that died during an outage leave no NameOwnerChanged behind).
    {
        let proxy_r = Arc::clone(&dbus_proxy);
        let state_r = state.clone();
        _session_tasks.0.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            reconcile_item_owners(&proxy_r, &state_r).await;
        }));
    }

    use futures_util::StreamExt;
    loop {
        if state.stopped.load(Ordering::Acquire) {
            return None;
        }
        tokio::select! {
            _ = state.stop_notify.notified() => {
                return None;
            }
            Some((bus_name, object_path, key)) = reg_item_rx.recv() => {
                if bus_name.is_empty() || object_path.is_empty() {
                    continue;
                }
                if let Some(old) = crate::util::lock(&state.item_tasks).remove(&key) {
                    old.abort();
                }
                let conn_c = conn.clone();
                let items_c = Arc::clone(&state.items);
                let subs_c = Arc::clone(&state.subs);
                let k_task = key.clone();
                let handle = tokio::spawn(async move {
                    process_and_track_item(conn_c, bus_name, object_path, k_task, items_c, subs_c).await;
                });
                crate::util::lock(&state.item_tasks).insert(key, handle);
            }
            maybe_req = act_rx.recv() => {
                let req = maybe_req?;
                let conn_c = conn.clone();
                tokio::spawn(async move {
                    handle_activate_request(&conn_c, req).await;
                });
            }
            signal_opt = name_owner_changed_stream.next() => {
                let Some(signal) = signal_opt else {
                    break;
                };
                let Ok(args) = signal.args() else {
                    continue;
                };
                let name = args.name.as_str();
                let old_owner = args.old_owner.as_deref().unwrap_or("");
                let new_owner = args.new_owner.as_deref().unwrap_or("");

                if !old_owner.is_empty() && new_owner.is_empty() {
                    let removed_keys: Vec<String> = {
                        let my_items = crate::util::lock(&state.items);
                        my_items
                            .iter()
                            .filter(|(k, (item, _))| belongs_to_disconnected_owner(&item.bus_name, k, name, old_owner))
                            .map(|(k, _)| k.clone())
                            .collect()
                    };
                    for k in removed_keys {
                        log_info!("tray", "Process terminated, removing tray item: {}", k);
                        state.remove_item(&k);
                    }
                } else if old_owner.is_empty() && !new_owner.is_empty() && is_probe_candidate(name) {
                    let conn_new = conn.clone();
                    let proxy_new = Arc::clone(&dbus_proxy);
                    let reg_tx_new = reg_item_tx.clone();
                    let reg_items_new = Arc::clone(&state.registered_items);
                    let n = name.to_string();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        probe_single_service(&conn_new, &proxy_new, &n, &reg_tx_new, &reg_items_new).await;
                    });
                }
            }
        }
    }

    Some("NameOwnerChanged stream ended (bus restart?)".to_string())
}

async fn reconcile_item_owners(dbus_proxy: &DBusProxy<'_>, state: &SessionState) {
    let keys: Vec<String> = crate::util::lock(&state.items).keys().cloned().collect();
    let mut removed = 0u32;
    for key in keys {
        let Some(bus_name) = crate::util::lock(&state.items)
            .get(&key)
            .map(|(item, _)| item.bus_name.clone())
        else {
            continue;
        };
        let Ok(name) = zbus::names::BusName::try_from(bus_name.as_str()) else {
            continue;
        };
        if dbus_proxy.get_name_owner(name).await.is_err() {
            state.remove_item(&key);
            removed += 1;
        }
    }
    if removed > 0 {
        log_info!("tray", "Owner reconcile removed {removed} stale item(s)");
    }
}

async fn probe_and_register_all(
    conn: &Connection,
    dbus_proxy: &Arc<DBusProxy<'static>>,
    reg_tx: &RegTx,
    state: &SessionState,
) {
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
        let proxy_c = Arc::clone(dbus_proxy);
        let reg_tx_c = reg_tx.clone();
        let reg_set_c = Arc::clone(&state.registered_items);
        let sem_c = Arc::clone(&state.probe_sem);
        tokio::spawn(async move {
            // Bound login-storm fan-out; excess probes wait.
            let _permit = sem_c.acquire_owned().await.ok()?;
            probe_single_service(&conn_c, &proxy_c, &s, &reg_tx_c, &reg_set_c).await;
            Some(())
        });
    }
}

async fn probe_prop(conn: &Connection, service: &str, path: &str, iface: &str, prop: &str) -> bool {
    tokio::time::timeout(
        Duration::from_millis(150),
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

async fn probe_single_service(
    conn: &Connection,
    dbus_proxy: &DBusProxy<'_>,
    service: &str,
    reg_tx: &RegTx,
    reg_items_set: &Arc<Mutex<HashSet<String>>>,
) {
    // Resolve well-known names to unique bus name to prevent duplicate registration
    let unique_bus = if !service.starts_with(':') {
        match zbus::names::BusName::try_from(service) {
            Ok(bn) => match tokio::time::timeout(Duration::from_millis(100), dbus_proxy.get_name_owner(bn)).await {
                Ok(Ok(owner)) => owner.to_string(),
                _ => service.to_string(),
            },
            Err(_) => service.to_string(),
        }
    } else {
        service.to_string()
    };

    let mut candidate_paths = candidate_sni_paths(&[]);

    if let Ok(Ok(res)) = tokio::time::timeout(
        Duration::from_millis(100),
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
            if !crate::util::lock(reg_items_set).insert(key.clone()) {
                return;
            }
            log_info!("tray", "Proactively discovered SNI service: {}", key);
            if let Err(e) = reg_tx.try_send((unique_bus, path, key.clone())) {
                log_warn!("tray", "Dropped proactive discovery for {key}: {e}");
            }
            break;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tray_service_get_item_and_shutdown() {
        let svc = TrayService::new_for_test();
        let item = TrayItem {
            bus_name: "org.example.App".into(),
            object_path: "/StatusNotifierItem".into(),
            id: "org.example.App".into(),
            title: Some("Example".into()),
            icon_name: None,
            icon_theme_path: None,
            icon_pixmap: None,
            tool_tip: None,
            item_is_menu: false,
            menu_path: None,
        };
        svc.insert_test_item("org.example.App/StatusNotifierItem".into(), item);
        let fetched = svc
            .get_item("org.example.App/StatusNotifierItem")
            .expect("item present");
        assert_eq!(fetched.id, "org.example.App");
        assert!(svc.get_item("missing").is_none());
        assert!(!svc.is_stopped());
        svc.shutdown();
        assert!(svc.is_stopped());
    }

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
