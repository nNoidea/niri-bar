use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use zbus::interface;
use zbus::object_server::SignalEmitter;

use super::protocol::parse_item_address;

/// DBus Watcher Object exporting `org.kde.StatusNotifierWatcher`
pub(super) struct StatusNotifierWatcher {
    pub(super) registered_items: Arc<Mutex<HashSet<String>>>,
    pub(super) registered_hosts: Arc<Mutex<HashSet<String>>>,
    pub(super) item_registered_tx: tokio::sync::mpsc::Sender<String>,
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
