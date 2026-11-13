use gtk::Window;
pub use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

/// Human-readable install hint when `libgtk4-layer-shell.so.0` is missing.
pub fn missing_library_error_message() -> &'static str {
    "Missing required runtime library: gtk4-layer-shell (libgtk4-layer-shell.so.0).\n\
    niri-bar requires gtk4-layer-shell to anchor itself as a Wayland desktop panel.\n\
    Please install it using your package manager:\n\
      Fedora:        sudo dnf install gtk4-layer-shell\n\
      Arch Linux:    sudo pacman -S gtk4-layer-shell\n\
      Debian/Ubuntu: sudo apt install libgtk4-layer-shell0"
}

/// True when layer-shell is supported on this display/session.
pub fn is_available() -> bool {
    gtk4_layer_shell::is_supported()
}

/// Configure `window` as a fullscreen transparent overlay (drag tracking).
pub fn setup_overlay_window(window: &Window, monitor: Option<&gdk::Monitor>) {
    window.init_layer_shell();
    window.set_monitor(monitor);
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Right, true);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Bottom, true);
    window.set_exclusive_zone(-1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_missing_library_error_message_contains_package_managers() {
        let msg = missing_library_error_message();
        assert!(msg.contains("gtk4-layer-shell"));
        assert!(msg.contains("libgtk4-layer-shell.so.0"));
        assert!(msg.contains("dnf install gtk4-layer-shell"));
        assert!(msg.contains("pacman -S gtk4-layer-shell"));
        assert!(msg.contains("apt install libgtk4-layer-shell0"));
    }

    #[test]
    fn test_layer_shell_enums() {
        assert_eq!(Layer::Overlay, gtk4_layer_shell::Layer::Overlay);
        assert_eq!(Edge::Top, gtk4_layer_shell::Edge::Top);
        assert_eq!(KeyboardMode::Exclusive, gtk4_layer_shell::KeyboardMode::Exclusive);
    }
}
