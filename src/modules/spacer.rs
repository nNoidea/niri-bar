use crate::config::SpacerConfig;
use crate::modules::{BarModule, ModuleState, ScrollDirection};
use gtk::Orientation;

pub struct SpacerModule {
    config: SpacerConfig,
    // Keeps the broadcast channel alive: without a stored Sender,
    // `subscribe()` would hand out an immediately-closed Receiver.
    _keepalive_tx: async_channel::Sender<ModuleState>,
    template_rx: async_channel::Receiver<ModuleState>,
}

impl SpacerModule {
    pub fn new(config: SpacerConfig) -> Self {
        let (tx, rx) = async_channel::unbounded();
        Self {
            config,
            _keepalive_tx: tx,
            template_rx: rx,
        }
    }
}

impl BarModule for SpacerModule {
    fn name(&self) -> &'static str {
        "spacer"
    }

    fn current_state(&self, _orientation: Orientation) -> ModuleState {
        ModuleState {
            icon: None,
            text: None,
            tooltip: None,
            css_classes: vec!["module-spacer".to_string()],
        }
    }

    fn subscribe(&self, _orientation: Orientation) -> async_channel::Receiver<ModuleState> {
        self.template_rx.clone()
    }

    fn click_commands(&self) -> (Option<&str>, Option<&str>, Option<&str>) {
        self.config.click.commands()
    }

    fn handle_scroll(&self, _direction: ScrollDirection) {}
}

pub fn configure_spacer_button(button: &gtk::Button, size: i32, is_vertical: bool) {
    use gtk::prelude::*;
    if size <= 0 {
        button.set_no_show_all(true);
        button.hide();
        button.set_size_request(0, 0);
    } else {
        button.set_no_show_all(false);
        button.show();
        if is_vertical {
            button.set_size_request(-1, size);
        } else {
            button.set_size_request(size, -1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::prelude::*;

    #[test]
    fn test_spacer_state_and_clickability() {
        let default_spacer = SpacerModule::new(SpacerConfig::default());
        assert_eq!(default_spacer.name(), "spacer");
        assert!(!default_spacer.is_clickable());

        let state = default_spacer.current_state(Orientation::Vertical);
        assert!(state.icon.is_none());
        assert!(state.text.is_none());
        assert_eq!(state.css_classes, vec!["module-spacer"]);

        let clickable_spacer = SpacerModule::new(SpacerConfig {
            size: 20,
            click: crate::config::ClickActions {
                on_click_left: Some("echo clicked".into()),
                ..Default::default()
            },
        });
        assert!(clickable_spacer.is_clickable());
    }

    #[test]
    fn test_configure_spacer_button_visibility_and_sizing() {
        if gtk::is_initialized() {
            if !gtk::is_initialized_main_thread() {
                eprintln!("Skipping GTK test: running on non-main GTK thread");
                return;
            }
        } else if gtk::init().is_err() {
            eprintln!("Skipping GTK test: display not available");
            return;
        }
        let button = gtk::Button::new();

        // When size <= 0: hidden and no_show_all
        configure_spacer_button(&button, 0, true);
        assert!(button.is_no_show_all());
        assert!(!button.is_visible());

        // When size > 0 and vertical: visible, height = size, width = -1
        configure_spacer_button(&button, 15, true);
        assert!(!button.is_no_show_all());
        assert!(button.is_visible());
        assert_eq!((button.width_request(), button.height_request()), (-1, 15));

        // When size > 0 and horizontal: visible, width = size, height = -1
        configure_spacer_button(&button, 25, false);
        assert!(!button.is_no_show_all());
        assert!(button.is_visible());
        assert_eq!((button.width_request(), button.height_request()), (25, -1));
    }
}
