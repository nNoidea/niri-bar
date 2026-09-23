//! System tray (StatusNotifierItem): D-Bus watcher/service, pure protocol
//! parsing, and GTK widgets. Split from a single 2082-line file with no
//! behavior change. Only the service/widget entry points are re-exported;
//! internal items live in their submodules (`types`, `protocol`).

mod protocol;
mod service;
mod types;
mod widget;

pub use service::TrayService;
pub use widget::TrayWidget;
