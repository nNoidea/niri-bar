use gtk::Orientation;
use std::fs;
use std::thread;
use std::time::Duration;

use crate::config::MemoryConfig;
use crate::modules::{BarModule, ModuleCore, ModuleState, ScrollDirection};

pub struct MemoryModule {
    config: MemoryConfig,
    core: ModuleCore,
}

impl MemoryModule {
    pub fn new(config: MemoryConfig) -> Self {
        let core = ModuleCore::new();
        let core_worker = core.clone();
        let cfg = config.clone();

        thread::spawn(move || {
            let mut last_used_tenth = -1i64;

            loop {
                if core_worker.is_stopped() {
                    break;
                }
                let (used_gb, total_gb) = MemoryModule::read_memory_static();

                if total_gb > 0.0 {
                    let used_tenth = (used_gb * 10.0).round() as i64;

                    if used_tenth != last_used_tenth {
                        last_used_tenth = used_tenth;

                        core_worker
                            .broadcast(|orient| MemoryModule::format_state_static(&cfg, used_gb, total_gb, orient));
                    }
                }

                if core_worker.wait_timeout(Duration::from_secs(2)) {
                    break;
                }
            }
        });

        Self { config, core }
    }

    fn read_memory_static() -> (f64, f64) {
        let content = match fs::read_to_string("/proc/meminfo") {
            Ok(c) => c,
            Err(e) => {
                log_debug!("memory", "Failed to read /proc/meminfo: {e}");
                return (0.0, 0.0);
            }
        };
        let mut total = 0.0;
        let mut avail = 0.0;

        for line in content.lines() {
            if line.starts_with("MemTotal:") {
                if let Some(val) = line.split_whitespace().nth(1).and_then(|v| v.parse::<f64>().ok()) {
                    total = val;
                }
            } else if line.starts_with("MemAvailable:") {
                if let Some(val) = line.split_whitespace().nth(1).and_then(|v| v.parse::<f64>().ok()) {
                    avail = val;
                }
            }
        }

        if total > 0.0 {
            // /proc/meminfo values are in KiB (1024 bytes).
            // Report GiB (1024^3) to match `free`, `btop`, GNOME, etc.
            let used_bytes = (total - avail) * 1024.0;
            let total_bytes = total * 1024.0;
            let used_gb = used_bytes / 1_073_741_824.0;
            let total_gb = total_bytes / 1_073_741_824.0;
            return (used_gb, total_gb);
        }
        log_debug!("memory", "Unparseable /proc/meminfo (no MemTotal)");
        (0.0, 0.0)
    }

    fn format_state_static(
        config: &MemoryConfig,
        used_gb: f64,
        total_gb: f64,
        _orientation: Orientation,
    ) -> ModuleState {
        let used_str = format!("{used_gb:.1}");
        let total_str = format!("{total_gb:.1}");
        let pct = if total_gb > 0.0 {
            (used_gb / total_gb) * 100.0
        } else {
            0.0
        };
        let pct_str = format!("{pct:.0}");

        let formatted = config
            .format
            .replace("%u", &used_str)
            .replace("%t", &total_str)
            .replace("%p", &pct_str);

        ModuleState {
            icon: None,
            text: Some(formatted),
            tooltip: Some(format!("{used_gb:.1}G / {total_gb:.1}G ({pct:.0}%)")),
            css_classes: Vec::new(),
        }
    }
}

impl BarModule for MemoryModule {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn current_state(&self, orientation: Orientation) -> ModuleState {
        let (used, total) = Self::read_memory_static();
        if total <= 0.0 {
            return ModuleState {
                icon: None,
                text: Some("—".to_string()),
                tooltip: Some("Memory unavailable".to_string()),
                css_classes: vec!["unavailable".to_string()],
            };
        }
        Self::format_state_static(&self.config, used, total, orientation)
    }

    fn core(&self) -> &ModuleCore {
        &self.core
    }

    fn click_commands(&self) -> (Option<&str>, Option<&str>, Option<&str>) {
        self.config.click.commands()
    }

    fn handle_scroll(&self, _direction: ScrollDirection) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_format() {
        let config = MemoryConfig::default();
        let state = MemoryModule::format_state_static(&config, 5.5, 16.0, Orientation::Vertical);
        assert_eq!(state.text.as_deref(), Some("󰍛\n5.5G"));
        assert_eq!(state.tooltip.as_deref(), Some("5.5G / 16.0G (34%)"));

        let custom_config = MemoryConfig {
            format: "RAM: %u/%t (%p%)".to_string(),
            ..Default::default()
        };
        let custom_state = MemoryModule::format_state_static(&custom_config, 5.5, 16.0, Orientation::Vertical);
        assert_eq!(custom_state.text.as_deref(), Some("RAM: 5.5/16.0 (34%)"));
    }

    #[test]
    fn test_memory_clickability() {
        let unclickable = MemoryModule::new(MemoryConfig::default());
        assert!(!unclickable.is_clickable());

        let clickable = MemoryModule::new(MemoryConfig {
            click: crate::config::ClickActions {
                on_click_left: Some("alacritty -e btop".to_string()),
                ..Default::default()
            },
            ..Default::default()
        });
        assert!(clickable.is_clickable());
    }
}
