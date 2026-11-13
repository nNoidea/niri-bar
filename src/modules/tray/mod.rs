//! System tray (StatusNotifierItem): D-Bus watcher/service, proactive discovery,
//! per-item client/menu tracking, pure protocol parsing, and GTK widgets.
//! Only the service/widget entry points are re-exported.

mod item_client;
mod protocol;
mod service;
mod types;
mod widget;

pub use service::TrayService;
pub use widget::TrayWidget;
