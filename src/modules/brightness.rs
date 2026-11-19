use gtk::Orientation;
use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{resolve_level, BrightnessConfig};
use crate::modules::{system_bus, BarModule, ModuleCore, ModuleState, ScrollDirection};

pub fn compute_brightness_units(target_pct: u32, max_units: u64) -> u64 {
    ((max_units as f64 * (f64::from(target_pct) / 100.0)).round() as u64).clamp(1, max_units.max(1))
}

pub fn brightness_poll_interval(has_device: bool) -> Duration {
    if has_device {
        Duration::from_millis(500)
    } else {
        Duration::from_secs(10)
    }
}

fn validate_backlight_dev_name(dev_name: &str) -> bool {
    !dev_name.is_empty()
        && dev_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':' || c == '.')
}

pub struct BrightnessModule {
    config: BrightnessConfig,
    current_brightness: Arc<AtomicU32>,
    pending_target: Arc<AtomicU32>,
    cmd_tx: async_channel::Sender<()>,
    core: ModuleCore,
}

impl BrightnessModule {
    pub fn new(config: BrightnessConfig) -> Self {
        // `None` means no backlight device (e.g. desktop): keep sentinel
        // u32::MAX so the UI never fabricates `100%`.
        let b = Self::read_system_brightness().unwrap_or(u32::MAX);
        let current_brightness = Arc::new(AtomicU32::new(b));
        let pending_target = Arc::new(AtomicU32::new(u32::MAX));
        let (cmd_tx, cmd_rx) = async_channel::bounded::<()>(1);

        let cur_b = Arc::clone(&current_brightness);
        let pending_worker = Arc::clone(&pending_target);
        let cfg = config.clone();
        let core = ModuleCore::new();
        let core_worker = core.clone();

        crate::runtime::spawn(async move {
            let mut logged_no_device = false;
            let mut suppress_until: Option<tokio::time::Instant> = None;
            loop {
                if core_worker.is_stopped() {
                    break;
                }
                let poll_dur = brightness_poll_interval(cur_b.load(Ordering::Relaxed) != u32::MAX);
                tokio::select! {
                    biased;
                    () = core_worker.wait_stopped() => break,
                    recv = cmd_rx.recv() => {
                        if recv.is_err() {
                            break;
                        }
                        let target = pending_worker.swap(u32::MAX, Ordering::AcqRel);
                        if target != u32::MAX {
                            BrightnessModule::apply_set_brightness_dbus(target).await;
                            suppress_until = Some(tokio::time::Instant::now() + Duration::from_millis(250));
                        }
                    }
                    () = tokio::time::sleep(poll_dur) => {
                        if let Some(until) = suppress_until {
                            if tokio::time::Instant::now() < until {
                                continue;
                            }
                            suppress_until = None;
                        }
                        let reading = BrightnessModule::read_system_brightness();
                        BrightnessModule::apply_brightness_reading(
                            &cur_b,
                            reading,
                            &cfg,
                            &core_worker,
                            &mut logged_no_device,
                        );
                    }
                }
            }
        });

        Self {
            config,
            current_brightness,
            pending_target,
            cmd_tx,
            core,
        }
    }

    #[cfg(test)]
    fn new_for_test(config: BrightnessConfig, initial: u32) -> (Self, async_channel::Receiver<()>) {
        let (cmd_tx, cmd_rx) = async_channel::bounded::<()>(1);
        let core = ModuleCore::new();
        core.shutdown();
        (
            Self {
                config,
                current_brightness: Arc::new(AtomicU32::new(initial)),
                pending_target: Arc::new(AtomicU32::new(u32::MAX)),
                cmd_tx,
                core,
            },
            cmd_rx,
        )
    }

    pub(crate) fn apply_brightness_reading(
        cur_b: &AtomicU32,
        reading: Option<u32>,
        cfg: &BrightnessConfig,
        core: &ModuleCore,
        logged_no_device: &mut bool,
    ) {
        match reading {
            Some(b) => {
                let clamped = b.min(100);
                *logged_no_device = false;
                let old_b = cur_b.load(Ordering::Relaxed);
                if clamped != old_b {
                    cur_b.store(clamped, Ordering::Relaxed);
                    core.broadcast(|orient| Self::format_state_with_config(cfg, clamped, orient));
                }
            }
            None => {
                if !*logged_no_device {
                    log_debug!("brightness", "No backlight device; hiding brightness value");
                    *logged_no_device = true;
                }
                let old_b = cur_b.swap(u32::MAX, Ordering::Relaxed);
                if old_b != u32::MAX {
                    core.broadcast(|_| Self::no_device_state());
                }
            }
        }
    }

    async fn apply_set_brightness_dbus(target_pct: u32) {
        let Some((dev_name, _, max)) = Self::get_backlight_device() else {
            return;
        };
        if !validate_backlight_dev_name(&dev_name) {
            log_warn!("brightness", "Refusing to use unexpected backlight device name");
            return;
        }
        let target_units = compute_brightness_units(target_pct.min(100), max).min(u64::from(u32::MAX)) as u32;
        let Some(conn) = system_bus().await else {
            log_warn!("brightness", "System bus unavailable for SetBrightness");
            return;
        };
        match tokio::time::timeout(
            Duration::from_millis(500),
            conn.call_method(
                Some("org.freedesktop.login1"),
                "/org/freedesktop/login1/session/auto",
                Some("org.freedesktop.login1.Session"),
                "SetBrightness",
                &("backlight", dev_name.as_str(), target_units),
            ),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => log_warn!("brightness", "SetBrightness failed: {e}"),
            Err(_) => log_warn!("brightness", "SetBrightness timed out"),
        }
    }

    fn get_backlight_device() -> Option<(String, u64, u64)> {
        if let Ok(entries) = fs::read_dir("/sys/class/backlight") {
            let mut sorted: Vec<_> = entries.flatten().collect();
            sorted.sort_by_key(|e| e.file_name());
            for entry in sorted {
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
            return Some((((cur as f64 / max as f64) * 100.0).round() as u32).min(100));
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
        // No device (desktop): never touch sysfs or optimistically update UI on GTK thread.
        let cur = self.current_brightness.load(Ordering::Relaxed);
        if cur == u32::MAX {
            log_debug!("brightness", "Scroll ignored: no backlight device");
            return;
        }
        let step = if self.config.step == 0 { 10 } else { self.config.step };
        let new_b = match direction {
            ScrollDirection::Up => cur.saturating_add(step).min(100),
            ScrollDirection::Down => cur.saturating_sub(step),
        };

        // 1. Instant local UI update (0ms latency)
        self.current_brightness.store(new_b, Ordering::Relaxed);
        self.dispatch_immediate_update();

        // 2. Coalesce onto the serialized background worker channel (`SetBrightness`).
        self.pending_target.store(new_b, Ordering::Release);
        let _ = self.cmd_tx.try_send(());
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

    #[test]
    fn test_brightness_runs_on_tokio_without_new_os_threads() {
        let _ = crate::runtime::handle();
        crate::runtime::warm_up_shared_test_threads();
        let before = crate::runtime::min_thread_count();
        let modules: Vec<_> = (0..10)
            .map(|_| BrightnessModule::new(BrightnessConfig::default()))
            .collect();
        let during = crate::runtime::min_thread_count();
        for m in &modules {
            m.shutdown();
        }
        assert!(
            during <= before + 4,
            "BrightnessModule should run on shared Tokio runtime without spawning OS threads (before: {before}, during: {during})"
        );
    }

    #[test]
    fn test_brightness_units_and_absent_backoff() {
        assert_eq!(compute_brightness_units(0, 1000), 1);
        assert_eq!(compute_brightness_units(50, 1000), 500);
        assert_eq!(compute_brightness_units(100, 1000), 1000);
        assert_eq!(brightness_poll_interval(true), Duration::from_millis(500));
        assert_eq!(brightness_poll_interval(false), Duration::from_secs(10));
    }

    #[test]
    fn test_brightness_handle_scroll_no_device_is_noop_and_valid_device_queues_command() {
        let (no_dev, rx_none) = BrightnessModule::new_for_test(BrightnessConfig::default(), u32::MAX);
        no_dev.handle_scroll(ScrollDirection::Up);
        assert_eq!(no_dev.current_brightness.load(Ordering::Relaxed), u32::MAX);
        assert_eq!(no_dev.pending_target.load(Ordering::Relaxed), u32::MAX);
        assert!(rx_none.try_recv().is_err());

        let (valid_dev, rx_valid) = BrightnessModule::new_for_test(
            BrightnessConfig {
                step: 10,
                ..Default::default()
            },
            50,
        );
        valid_dev.handle_scroll(ScrollDirection::Up);
        valid_dev.handle_scroll(ScrollDirection::Up);
        assert_eq!(valid_dev.current_brightness.load(Ordering::Relaxed), 70);
        assert_eq!(valid_dev.pending_target.load(Ordering::Relaxed), 70);
        // Channel bounded(1) coalesces multiple rapid scrolls to one wakeup carrying the latest target.
        assert!(rx_valid.try_recv().is_ok());
        assert!(rx_valid.try_recv().is_err());
    }

    #[test]
    fn test_brightness_apply_reading_resets_to_no_device_on_none() {
        let cfg = BrightnessConfig::default();
        let core = ModuleCore::new();
        let rx = core.subscribe(Orientation::Vertical);
        let cur = AtomicU32::new(60);
        let mut logged = false;

        BrightnessModule::apply_brightness_reading(&cur, None, &cfg, &core, &mut logged);
        assert_eq!(cur.load(Ordering::Relaxed), u32::MAX);
        assert!(logged);
        let state = rx.try_recv().expect("must broadcast no-device state");
        assert!(state.text.is_none());
        assert!(state.css_classes.contains(&"no-device".to_string()));

        // Repeated None must not re-broadcast unnecessarily.
        BrightnessModule::apply_brightness_reading(&cur, None, &cfg, &core, &mut logged);
        assert!(rx.try_recv().is_err());
    }
}
