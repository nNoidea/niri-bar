use chrono::{Local, Timelike};
use gtk::Orientation;
use std::time::Duration;

use crate::config::ClockConfig;
use crate::modules::{spawn_dbus_trigger, BarModule, DbusSubscriptions, ModuleCore, ModuleState};

pub struct ClockModule {
    config: ClockConfig,
    core: ModuleCore,
}

/// Computes duration until the next second (when `per_second` is true) or minute
/// boundary, adding a 5ms pad so `Local::now()` has rolled into the new tick.
pub fn duration_until_next_tick(subsec_nanos: u32, second: u32, per_second: bool) -> Duration {
    let ms_into_sec = u64::from(subsec_nanos.min(999_999_999)) / 1_000_000;
    let secs_until = if per_second { 1 } else { u64::from(60 - second.min(59)) };
    Duration::from_millis(secs_until * 1000 - ms_into_sec + 5)
}

/// Specifiers whose value can change within a minute and therefore require
/// per-second ticks instead of minute-aligned wakeups.
/// `%c` / `%+` are included: they render the full locale datetime including seconds.
pub fn needs_per_second_tick(fmt_vert: &str, fmt_horiz: &str) -> bool {
    ["%S", "%s", "%T", "%r", "%X", "%c", "%+", "%N", "%f"]
        .iter()
        .any(|pat| fmt_vert.contains(pat) || fmt_horiz.contains(pat))
}

impl ClockModule {
    pub fn new(config: ClockConfig) -> Self {
        let core = ModuleCore::new();
        let core_worker = core.clone();
        let fmt_vert = config.format_vertical.clone();
        let fmt_horiz = config.format_horizontal.clone();
        let has_seconds = needs_per_second_tick(&fmt_vert, &fmt_horiz);

        crate::runtime::spawn(async move {
            let mut last_tick = 999;
            let _sleep_sub = DbusSubscriptions::new(vec![spawn_dbus_trigger(
                &core_worker,
                "org.freedesktop.login1",
                "org.freedesktop.login1.Manager",
                Some("PrepareForSleep"),
            )]);

            loop {
                if core_worker.is_stopped() {
                    break;
                }
                let now = Local::now();
                let cur_tick = if has_seconds { now.second() } else { now.minute() };

                if cur_tick != last_tick {
                    last_tick = cur_tick;

                    let text_vert = now.format(&fmt_vert).to_string();
                    let text_horiz = now.format(&fmt_horiz).to_string();
                    let tooltip = now.format("%A, %d %B (%m) %Y, %H:%M").to_string();

                    core_worker.broadcast(|orient| {
                        let text = if orient == Orientation::Vertical {
                            text_vert.clone()
                        } else {
                            text_horiz.clone()
                        };
                        ModuleState {
                            icon: None,
                            text: Some(text),
                            tooltip: Some(tooltip.clone()),
                            css_classes: Vec::new(),
                        }
                    });
                }

                let wait = duration_until_next_tick(now.nanosecond(), now.second(), has_seconds);
                let (stopped, _) = core_worker.wait_or_notified_async(wait).await;
                if stopped {
                    break;
                }
            }
        });

        Self { config, core }
    }

    fn format_now(&self, orientation: Orientation) -> ModuleState {
        let now = Local::now();
        let is_vertical = orientation == Orientation::Vertical;

        let text = if is_vertical {
            now.format(&self.config.format_vertical).to_string()
        } else {
            now.format(&self.config.format_horizontal).to_string()
        };

        ModuleState {
            icon: None,
            text: Some(text),
            tooltip: Some(now.format("%A, %d %B (%m) %Y, %H:%M").to_string()),
            css_classes: Vec::new(),
        }
    }
}

impl BarModule for ClockModule {
    fn name(&self) -> &'static str {
        "clock"
    }

    fn current_state(&self, orientation: Orientation) -> ModuleState {
        self.format_now(orientation)
    }

    fn core(&self) -> &ModuleCore {
        &self.core
    }

    fn click_commands(&self) -> (Option<&str>, Option<&str>, Option<&str>) {
        self.config.click.commands()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clock_format_orientation() {
        let config = ClockConfig {
            format_vertical: "%H\n%M".to_string(),
            format_horizontal: "%H:%M".to_string(),
            ..Default::default()
        };
        let clock = ClockModule::new(config);
        let vert_state = clock.current_state(Orientation::Vertical);
        assert!(vert_state.text.unwrap().contains('\n'));

        let horiz_state = clock.current_state(Orientation::Horizontal);
        assert!(horiz_state.text.unwrap().contains(':'));
        assert!(horiz_state.tooltip.unwrap().contains('('));
    }

    #[test]
    fn test_clock_clickability() {
        let mut config = ClockConfig {
            format_vertical: "%H:%M".to_string(),
            format_horizontal: "%H:%M".to_string(),
            ..Default::default()
        };
        let clock_non_clickable = ClockModule::new(config.clone());
        assert!(!clock_non_clickable.is_clickable());

        config.click.on_click_left = Some("gnome-calendar".to_string());
        let clock_clickable = ClockModule::new(config);
        assert!(clock_clickable.is_clickable());
    }

    #[test]
    fn test_needs_per_second_tick() {
        assert!(!needs_per_second_tick("%H:%M", "%a %d %b %H:%M"));
        assert!(!needs_per_second_tick("%H\n%M", "%H:%M"));
        assert!(needs_per_second_tick("%H:%M:%S", "%H:%M"));
        assert!(needs_per_second_tick("%H:%M", "%T"));
        assert!(needs_per_second_tick("%c", "%H:%M"));
        assert!(needs_per_second_tick("%H:%M", "%+"));
        assert!(needs_per_second_tick("%r", "%X"));
    }

    #[test]
    fn test_clock_runs_on_tokio_without_new_os_threads() {
        let _ = crate::runtime::handle();
        crate::runtime::warm_up_shared_test_threads();
        let before = crate::runtime::min_thread_count();
        let modules: Vec<_> = (0..10).map(|_| ClockModule::new(ClockConfig::default())).collect();
        let during = crate::runtime::min_thread_count();
        for m in &modules {
            m.shutdown();
        }
        assert!(
            during <= before + 4,
            "ClockModule should run on shared Tokio runtime without spawning OS threads (before: {before}, during: {during})"
        );
    }

    #[test]
    fn test_duration_until_next_tick_aligns_to_boundary() {
        assert_eq!(
            duration_until_next_tick(250_000_000, 12, true),
            Duration::from_millis(755)
        );
        assert_eq!(
            duration_until_next_tick(500_000_000, 20, false),
            Duration::from_millis(39_505)
        );
        assert_eq!(duration_until_next_tick(0, 59, false), Duration::from_millis(1_005));
    }
}
