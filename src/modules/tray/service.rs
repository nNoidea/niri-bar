use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use zbus::Connection;

use super::discovery::{
    belongs_to_disconnected_owner, is_probe_candidate, probe_and_register_all, probe_single_service,
    reconcile_item_owners,
};
use super::item_client::{handle_activate_request, process_and_track_item};
use super::protocol::parse_item_address;
use super::types::{ActivateRequest, TrayEvent, TrayItem, TrayItemMap, TrayItemSnapshot, TrayMenu};
use super::watcher::StatusNotifierWatcher;

pub struct TrayService {
    subscribers: Arc<Mutex<Vec<async_channel::Sender<TrayEvent>>>>,
    act_tx: tokio::sync::mpsc::Sender<ActivateRequest>,
    items: Arc<Mutex<TrayItemMap>>,
}

impl TrayService {
    /// Single shared instance (see `SharedModules`): spawns its supervisor
    /// and activation tasks on the shared process-wide Tokio runtime.
    /// Do not construct per-monitor.
    pub fn new() -> Self {
        let (act_tx, mut act_rx) = tokio::sync::mpsc::channel::<ActivateRequest>(128);
        let items = Arc::new(Mutex::new(HashMap::new()));
        let subscribers: Arc<Mutex<Vec<async_channel::Sender<TrayEvent>>>> = Arc::new(Mutex::new(Vec::new()));

        let items_clone = Arc::clone(&items);
        let subs_clone = Arc::clone(&subscribers);

        crate::runtime::spawn(async move {
            // Current live connection for activation requests. Updated on
            // every (re)connect so GTK clicks survive bus restarts.
            let current_conn: Arc<tokio::sync::Mutex<Option<Connection>>> = Arc::new(tokio::sync::Mutex::new(None));
            let conn_for_act = Arc::clone(&current_conn);
            crate::runtime::spawn(async move {
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
                let session_result = run_tray_session(
                    Arc::clone(&items_clone),
                    Arc::clone(&subs_clone),
                    Arc::clone(&current_conn),
                    Arc::clone(&item_tasks),
                    Arc::clone(&probe_sem),
                )
                .await;
                // Session ended (bus loss or fatal setup error): drop stale
                // tasks holding dead proxies, clear live conn, back off.
                {
                    let mut tasks = crate::util::lock(&item_tasks);
                    for (_, h) in tasks.drain() {
                        h.abort();
                    }
                }
                *current_conn.lock().await = None;
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
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(30);
            }
        });

        Self {
            subscribers,
            act_tx,
            items,
        }
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
