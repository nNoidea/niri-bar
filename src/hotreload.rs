use gio::prelude::*;
use gtk::prelude::*;
use std::cell::RefCell;
use std::fs;
use std::rc::Rc;

use crate::bar::BarWindow;
use crate::monitor::ReconcileCtx;

/// Live config/style error state. Config and style fail independently,
/// so each has its own slot — fixing one must not clear the other's badge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActiveError {
    pub config: Option<String>,
    pub style: Option<String>,
}

impl ActiveError {
    /// Badge text: config takes precedence when both are broken.
    pub fn message(&self) -> Option<&str> {
        self.config.as_deref().or(self.style.as_deref())
    }

    pub fn is_empty(&self) -> bool {
        self.config.is_none() && self.style.is_none()
    }
}

/// Pure filename filter for the hot-reload watcher: which watched file
/// (if any) does a `GFileMonitor` changed event refer to?
/// Checks both `file` and `other` so atomic renames (vim/helix write-tmp +
/// rename) are caught regardless of which side Gio reports.
pub fn watch_target_for_filenames(file_basename: Option<&str>, other_basename: Option<&str>) -> Option<&'static str> {
    for name in [file_basename, other_basename].into_iter().flatten() {
        if name == "config.toml" {
            return Some("config.toml");
        }
        if name == "style.css" {
            return Some("style.css");
        }
    }
    None
}

fn watch_target_for_event(file: &gio::File, other: Option<&gio::File>) -> Option<&'static str> {
    let file_base = file.basename();
    let other_base = other.and_then(|f| f.basename());
    watch_target_for_filenames(
        file_base.as_deref().and_then(|p| p.to_str()),
        other_base.as_deref().and_then(|p| p.to_str()),
    )
}

/// Re-apply the combined [`ActiveError`] to every bar: show the message
/// when any slot is set, clear only when both are `None`.
pub fn refresh_error_badges(bars: &Rc<RefCell<Vec<BarWindow>>>, err: &ActiveError) {
    let Ok(list) = bars.try_borrow() else { return };
    for b in list.iter() {
        match err.message() {
            Some(msg) => b.show_error(msg),
            None => b.clear_error(),
        }
    }
}

fn update_error_slot(ctx: &ReconcileCtx, is_style: bool, msg: Option<String>, refresh: bool) {
    if let Ok(mut err) = ctx.active_error.try_borrow_mut() {
        if is_style {
            err.style = msg;
        } else {
            err.config = msg;
        }
        if refresh {
            refresh_error_badges(&ctx.bars, &err);
        }
    }
}

pub fn setup_file_watcher(ctx: ReconcileCtx, schedule_reconcile: Rc<dyn Fn()>) -> Option<gio::FileMonitor> {
    let config_dir = crate::config::get_config_dir();
    let watch_path = fs::canonicalize(&config_dir).unwrap_or(config_dir);
    let gfile = gio::File::for_path(&watch_path);
    let monitor = match gfile.monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) {
        Ok(m) => m,
        Err(e) => {
            log_warn!("hotreload", "Failed to start directory monitor on {watch_path:?}: {e}");
            return None;
        }
    };

    log_info!(
        "hotreload",
        "Live config & style hot-reloading active on {watch_path:?}"
    );

    // Per-file debounce: a shared timer would drop one file when both are
    // saved within the same 100ms window (last event wins).
    let debounce_config: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let debounce_style: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    monitor.connect_changed(move |_mon, file, other, event| {
        use gio::FileMonitorEvent::*;
        let deleted = matches!(event, Deleted | MovedOut);
        if !deleted {
            match event {
                Changed | Created | ChangesDoneHint | AttributeChanged | Moved | MovedIn | Renamed => {}
                _ => return,
            }
        }

        let target_name = match watch_target_for_event(file, other) {
            Some(n) => n,
            None => return,
        };
        let is_style = target_name == "style.css";

        if deleted {
            let msg = format!("{target_name} was deleted");
            if is_style {
                log_warn!("style", "Live style reload error: {msg}");
            } else {
                log_warn!("config", "Live config reload error: {msg}");
            }
            update_error_slot(&ctx, is_style, Some(msg), true);
            return;
        }

        let deb = Rc::clone(if is_style { &debounce_style } else { &debounce_config });
        let ctx = ctx.clone();
        let sched = Rc::clone(&schedule_reconcile);

        if let Some(src_id) = deb.borrow_mut().take() {
            src_id.remove();
        }

        let deb_clone = Rc::clone(&deb);
        let new_src = glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
            *deb_clone.borrow_mut() = None;
            if is_style {
                match crate::style::reload_styles() {
                    Ok(()) => {
                        log_info!("style", "style.css live-reloaded successfully");
                        update_error_slot(&ctx, true, None, true);
                    }
                    Err(e) => {
                        log_warn!("style", "Live style reload error: {e}");
                        update_error_slot(&ctx, true, Some(e), true);
                    }
                }
            } else {
                match crate::config::reload_config() {
                    Ok(new_config) => {
                        log_info!("config", "config.toml live-reloaded successfully; updating layout");
                        if let Ok(mut cfg) = ctx.config.try_borrow_mut() {
                            *cfg = new_config.clone();
                        }
                        if let Ok(mut mods) = ctx.modules.try_borrow_mut() {
                            mods.shutdown();
                            *mods = crate::modules::SharedModules::new(&new_config);
                        }
                        crate::monitor::sync_tray_service(&ctx.tray_service, &new_config);

                        update_error_slot(&ctx, false, None, false);

                        if let Ok(mut bars) = ctx.bars.try_borrow_mut() {
                            for b in bars.drain(..) {
                                b.window.set_visible(false);
                                b.window.close();
                            }
                        }
                        sched();
                    }
                    Err(e) => {
                        log_warn!("config", "Live config reload error: {e}");
                        update_error_slot(&ctx, false, Some(e), true);
                    }
                }
            }
        });
        *deb.borrow_mut() = Some(new_src);
    });

    Some(monitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_watch_target_for_filenames() {
        // Direct hits on either side (covers atomic renames where Gio
        // reports the tmp name in `file` and the real name in `other`).
        assert_eq!(
            watch_target_for_filenames(Some("config.toml"), None),
            Some("config.toml")
        );
        assert_eq!(watch_target_for_filenames(Some("style.css"), None), Some("style.css"));
        assert_eq!(
            watch_target_for_filenames(Some("config.toml.tmp"), Some("config.toml")),
            Some("config.toml")
        );
        assert_eq!(
            watch_target_for_filenames(Some("goutputstream-XXXX"), Some("style.css")),
            Some("style.css")
        );
        // Unrelated files ignored.
        assert_eq!(watch_target_for_filenames(Some("other.txt"), None), None);
        assert_eq!(watch_target_for_filenames(None, None), None);
        assert_eq!(watch_target_for_filenames(Some("CONFIG.TOML"), None), None);
    }

    #[test]
    fn test_active_error_prefers_config_and_tracks_slots() {
        let mut err = ActiveError::default();
        assert!(err.is_empty());
        assert_eq!(err.message(), None);

        err.style = Some("css bad".into());
        assert!(!err.is_empty());
        assert_eq!(err.message(), Some("css bad"));

        // Config takes precedence but doesn't drop the style slot.
        err.config = Some("toml bad".into());
        assert_eq!(err.message(), Some("toml bad"));

        // Fixing style leaves config visible; fixing config reveals style.
        err.style = None;
        assert_eq!(err.message(), Some("toml bad"));
        err.config = None;
        assert!(err.is_empty());
    }
}
