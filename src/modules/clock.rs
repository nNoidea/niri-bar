use chrono::{Local, Timelike};
use gtk::Orientation;
use std::thread;
use std::time::Duration;

use crate::config::ClockConfig;
use crate::modules::{BarModule, ModuleState, ModuleSubscribers, ScrollDirection};

use std::sync::{Arc, Mutex};

pub struct ClockModule {
    config: ClockConfig,
    subscribers: ModuleSubscribers,
}

/// Specifiers whose value can change within a minute and therefore require
/// per-second ticks (250ms poll) instead of per-minute ticks (1s poll).
/// `%c` / `%+` are included: they render the full locale datetime including seconds.
pub fn needs_per_second_tick(fmt_vert: &str, fmt_horiz: &str) -> bool {
    ["%S", "%s", "%T", "%r", "%X", "%c", "%+", "%N", "%f"]
        .iter()
        .any(|pat| fmt_vert.contains(pat) || fmt_horiz.contains(pat))
}

impl ClockModule {
    pub fn new(config: ClockConfig) -> Self {
        let subscribers: ModuleSubscribers = Arc::new(Mutex::new(Vec::new()));
        let subs_clone = Arc::clone(&subscribers);
        let cfg = config.clone();

        let fmt_vert = cfg.format_vertical.clone();
        let fmt_horiz = cfg.format_horizontal.clone();
        // Any specifier that changes within a minute requires per-second ticks.
        let has_seconds = needs_per_second_tick(&fmt_vert, &fmt_horiz);

        thread::spawn(move || {
            let mut last_tick = 999;

            loop {
                let now = Local::now();
                let cur_tick = if has_seconds { now.second() } else { now.minute() };

                if cur_tick != last_tick {
                    last_tick = cur_tick;

                    let text_vert = now.format(&fmt_vert).to_string();
                    let text_horiz = now.format(&fmt_horiz).to_string();
                    let tooltip = now.format("%A, %d %B (%m) %Y, %H:%M").to_string();

                    crate::modules::broadcast(&subs_clone, |orient| {
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

                thread::sleep(Duration::from_millis(if has_seconds { 250 } else { 1000 }));
            }
        });

        Self { config, subscribers }
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

    fn subscribe(&self, orientation: Orientation) -> async_channel::Receiver<ModuleState> {
        crate::modules::subscribe_to(&self.subscribers, orientation)
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
}
