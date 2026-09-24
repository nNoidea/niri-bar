use gtk::Orientation;
use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::config::{resolve_level, BrightnessConfig};
use crate::modules::{spawn_command_argv, BarModule, ModuleCore, ModuleState, ScrollDirection};

pub struct BrightnessModule {
    config: BrightnessConfig,
    current_brightness: Arc<AtomicU32>,
    core: ModuleCore,
}

impl BrightnessModule {
    pub fn new(config: BrightnessConfig) -> Self {
        // `None` means no backlight device (e.g. desktop): keep sentinel
        // u32::MAX so the UI never fabricates `100%`.
        let b = Self::read_system_brightness().unwrap_or(u32::MAX);
        let current_brightness = Arc::new(AtomicU32::new(b));

        let cur_b = Arc::clone(&current_brightness);
        let cfg = config.clone();
        let core = ModuleCore::new();
        let core_worker = core.clone();

        thread::spawn(move || {
            let mut logged_no_device = false;
            loop {
                if core_worker.is_stopped() {
                    break;
                }
                match BrightnessModule::read_system_brightness() {
                    Some(b) => {
                        let old_b = cur_b.load(Ordering::Relaxed);
                        if b != old_b {
                            cur_b.store(b, Ordering::Relaxed);
                            core_worker.broadcast(|orient| BrightnessModule::format_state_with_config(&cfg, b, orient));
                        }
                    }
                    None if !logged_no_device => {
                        log_debug!("brightness", "No backlight device; hiding brightness value");
                        logged_no_device = true;
                    }
                    None => {}
                }
                if core_worker.wait_timeout(Duration::from_millis(500)) {
                    break;
                }
            }
        });

        Self {
            config,
            current_brightness,
            core,
        }
    }

    fn get_backlight_device() -> Option<(String, u64, u64)> {
        if let Ok(entries) = fs::read_dir("/sys/class/backlight") {
            for entry in entries.flatten() {
                let path = entry.path();
                let dev_name = entry.file_name().to_string_lossy().to_string();
                let cur = fs::read_to_string(path.join("brightness"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok());
                let max = fs::read_to_string(path.join("max_brightness"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok());

                if let (Some(cur), Some(max)) = (cur, max) {
                    if max > 0 {
                        return Some((dev_name, cur, max));
                    }
                }
            }
        }
        None
    }

    fn read_system_brightness() -> Option<u32> {
        if let Some((_, cur, max)) = Self::get_backlight_device() {
            return Some(((cur as f64 / max as f64) * 100.0).round() as u32);
        }
        None
    }

    /// Empty state for machines without backlight (never fake `100%`).
    fn no_device_state() -> ModuleState {
        ModuleState {
            icon: None,
            text: None,
            tooltip: Some("No backlight".to_string()),
            css_classes: vec!["no-device".to_string()],
        }
    }

    fn format_state_with_config(config: &BrightnessConfig, brightness: u32, _orientation: Orientation) -> ModuleState {
        let mut css_classes = Vec::new();
        let (formatted, threshold_opt) = resolve_level(&config.levels, brightness, None);

        if let Some(t) = threshold_opt {
            css_classes.push(format!("level-{t}"));
        }

        ModuleState {
            icon: None,
            text: Some(formatted),
            tooltip: Some(format!("{brightness}%")),
            css_classes,
        }
    }

    fn dispatch_immediate_update(&self) {
        let b = self.current_brightness.load(Ordering::Relaxed);
        if b == u32::MAX {
            return;
        }
        self.core
            .broadcast(|orient| Self::format_state_with_config(&self.config, b, orient));
    }
}

impl BarModule for BrightnessModule {
    fn name(&self) -> &'static str {
        "brightness"
    }

    fn current_state(&self, orientation: Orientation) -> ModuleState {
        let b = self.current_brightness.load(Ordering::Relaxed);
        if b == u32::MAX {
            return Self::no_device_state();
        }
        Self::format_state_with_config(&self.config, b, orientation)
    }

    fn core(&self) -> &ModuleCore {
        &self.core
    }

    fn click_commands(&self) -> (Option<&str>, Option<&str>, Option<&str>) {
        self.config.click.commands()
    }

    fn handle_scroll(&self, direction: ScrollDirection) {
        // No device (desktop): never optimistically update the UI.
        if self.current_brightness.load(Ordering::Relaxed) == u32::MAX && Self::get_backlight_device().is_none() {
            log_debug!("brightness", "Scroll ignored: no backlight device");
            return;
        }
        let step = if self.config.step == 0 { 10 } else { self.config.step };
        let cur = self.current_brightness.load(Ordering::Relaxed);
        let cur = if cur == u32::MAX { 100 } else { cur };

        let new_b = match direction {
            ScrollDirection::Up => (cur + step).min(100),
            ScrollDirection::Down => cur.saturating_sub(step),
        };

        // 1. Instant local UI update (0ms latency)
        self.current_brightness.store(new_b, Ordering::Relaxed);
        self.dispatch_immediate_update();

        // 2. Non-blocking system execution (argv, no shell: device name
        // never touches a shell parser even though it is charset-validated).
        if let Some((dev_name, _, max)) = BrightnessModule::get_backlight_device() {
            // Device names come from /sys/class/backlight directory names.
            // Keep the charset check as defense-in-depth; argv exec below
            // is what actually prevents shell injection.
            if !dev_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':' || c == '.')
                || dev_name.is_empty()
            {
                log_warn!("brightness", "Refusing to use unexpected backlight device name");
                return;
            }
            // Hardware needs >= 1 unit; UI may still show 0%.
            let target_units = (max as f64 * (f64::from(new_b) / 100.0)).round() as u64;
            let target_units = target_units.clamp(1, max);
            let units_str = target_units.to_string();

            spawn_command_argv(
                "brightness",
                "busctl",
                &[
                    "call",
                    "org.freedesktop.login1",
                    "/org/freedesktop/login1/session/auto",
                    "org.freedesktop.login1.Session",
                    "SetBrightness",
                    "ssu",
                    "backlight",
                    &dev_name,
                    &units_str,
                ],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_brightness_state_and_levels() {
        let config = BrightnessConfig::default();

        let state_low = BrightnessModule::format_state_with_config(&config, 10, Orientation::Vertical);
        assert_eq!(state_low.text.as_deref(), Some("󰃞\n10%"));
        assert_eq!(state_low.tooltip.as_deref(), Some("10%"));
        assert!(state_low.css_classes.contains(&"level-0".to_string()));

        let state_mid = BrightnessModule::format_state_with_config(&config, 50, Orientation::Vertical);
        assert_eq!(state_mid.text.as_deref(), Some("󰃟\n50%"));
        assert!(state_mid.css_classes.contains(&"level-33".to_string()));

        let state_high = BrightnessModule::format_state_with_config(&config, 90, Orientation::Vertical);
        assert_eq!(state_high.text.as_deref(), Some("󰃠\n90%"));
        assert!(state_high.css_classes.contains(&"level-66".to_string()));

        let state_max = BrightnessModule::format_state_with_config(&config, 100, Orientation::Vertical);
        assert_eq!(state_max.text.as_deref(), Some("󰃡\n100%"));
        assert!(state_max.css_classes.contains(&"level-100".to_string()));
    }

    #[test]
    fn test_brightness_clickability() {
        let unclickable = BrightnessModule::new(BrightnessConfig::default());
        assert!(!unclickable.is_clickable());

        let clickable = BrightnessModule::new(BrightnessConfig {
            click: crate::config::ClickActions {
                on_click_left: Some("brightnessctl set 50%".to_string()),
                ..Default::default()
            },
            ..Default::default()
        });
        assert!(clickable.is_clickable());
    }
}
