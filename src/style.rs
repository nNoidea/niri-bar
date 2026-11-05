use crate::config::get_config_dir;
use glib::prelude::*;
use std::cell::RefCell;
use std::fs;
use std::rc::Rc;

const DEFAULT_CSS: &str = include_str!("../resources/style.default.css");

thread_local! {
    static ACTIVE_PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
}

/// Load `css` into `provider` while capturing any GTK4 CSS `parsing-error` signal.
pub fn load_css_checked(provider: &gtk::CssProvider, css: &str) -> Result<(), String> {
    let err_slot: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let err_clone = Rc::clone(&err_slot);
    let handler_id = provider.connect_parsing_error(move |_, section, err| {
        let mut slot = err_clone.borrow_mut();
        if slot.is_none() {
            let line = section.end_location().lines() + 1;
            *slot = Some(format!("line {line}: {err}"));
        }
    });
    provider.load_from_data(css);
    provider.disconnect(handler_id);
    let taken = err_slot.borrow_mut().take();
    match taken {
        Some(e) => Err(format!("CSS error: {e}")),
        None => Ok(()),
    }
}

/// Load GTK CSS, writing the default template on first launch.
///
/// Failures are non-fatal: missing files fall back to the embedded default
/// and CSS parse errors fall back with a warning while returning `Some(error_message)`
/// so the caller can display an error badge on startup.
pub fn init_styles() -> Option<String> {
    let config_dir = get_config_dir();
    let css_path = config_dir.join("style.css");

    if !css_path.exists() {
        if let Err(e) = fs::create_dir_all(&config_dir) {
            crate::logger::emit("WARN", "style", &format!("Failed to create {config_dir:?}: {e}"));
        } else if let Err(e) = fs::write(&css_path, DEFAULT_CSS) {
            crate::logger::emit("WARN", "style", &format!("Failed to write default {css_path:?}: {e}"));
        }
    }

    let (css_content, read_error) = match fs::read_to_string(&css_path) {
        Ok(c) => (c, None),
        Err(e) => {
            let msg = format!("Failed to read {}: {e}", css_path.display());
            crate::logger::emit("WARN", "style", &format!("{msg}. Using embedded default."));
            (DEFAULT_CSS.to_string(), Some(format!("CSS error: {msg}")))
        }
    };

    let mut startup_error = read_error;
    let provider = gtk::CssProvider::new();
    if let Err(err_msg) = load_css_checked(&provider, &css_content) {
        crate::logger::emit(
            "WARN",
            "style",
            &format!("Failed to parse CSS ({err_msg}). Using default theme."),
        );
        startup_error = Some(err_msg);
        // Embedded default is tested for balanced braces; if it also fails,
        // log so the broken build is visible instead of silently unstyled.
        if let Err(e2) = load_css_checked(&provider, DEFAULT_CSS) {
            crate::logger::emit("WARN", "style", &format!("Embedded default CSS also failed: {e2}"));
        }
    }

    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_USER);
    }
    ACTIVE_PROVIDER.with(|p| *p.borrow_mut() = Some(provider));

    startup_error
}

/// Validate CSS syntax. Checks brace balance in pure Rust, and validates with GTK CssProvider when GTK is initialized.
pub fn validate_css(css_content: &str) -> Result<(), String> {
    let open_braces = css_content.matches('{').count();
    let close_braces = css_content.matches('}').count();
    if open_braces != close_braces {
        return Err(format!(
            "CSS error: mismatched curly braces: {open_braces} '{{' vs {close_braces} '}}'"
        ));
    }

    if gtk::is_initialized_main_thread() {
        let test_provider = gtk::CssProvider::new();
        load_css_checked(&test_provider, css_content)?;
    }
    Ok(())
}

/// Reload CSS from disk. If valid, updates the active provider on screen and returns `Ok(())`.
/// If invalid or unreadable, returns `Err(error_details)` without touching the active styles.
pub fn reload_styles() -> Result<(), String> {
    let config_dir = get_config_dir();
    let css_path = config_dir.join("style.css");
    let content = fs::read_to_string(&css_path).map_err(|e| format!("Failed to read {}: {e}", css_path.display()))?;

    validate_css(&content)?;

    ACTIVE_PROVIDER.with(|p| {
        if let Some(provider) = p.borrow().as_ref() {
            provider.load_from_data(&content);
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_css_integrity() {
        assert!(!DEFAULT_CSS.is_empty());
        assert!(DEFAULT_CSS.contains(".niri-bar"));
        assert!(DEFAULT_CSS.contains(".app-button"));
        assert!(DEFAULT_CSS.contains(".module-item"));
        assert!(DEFAULT_CSS.contains(".tray"));
        assert!(DEFAULT_CSS.contains(".tray-item"));
        assert!(DEFAULT_CSS.contains(".tray-menu {\n    background-color: #313244;"));

        // Check balanced braces
        let open_braces = DEFAULT_CSS.matches('{').count();
        let close_braces = DEFAULT_CSS.matches('}').count();
        assert_eq!(open_braces, close_braces, "CSS has mismatched curly braces");
    }

    #[test]
    fn test_taskbar_buttons_disable_transition() {
        assert!(
            DEFAULT_CSS.contains(".taskbar button") && DEFAULT_CSS.contains("transition: none;"),
            "DEFAULT_CSS should explicitly disable transitions on taskbar buttons"
        );
    }

    #[test]
    fn test_taskbar_buttons_not_hardcoded_50px() {
        assert!(
            !DEFAULT_CSS.contains("min-width: 50px;"),
            "DEFAULT_CSS should not hardcode 50px dimensions on taskbar buttons"
        );
    }

    #[test]
    fn test_validate_css() {
        let valid_css = ".niri-bar { color: #ffffff; }";
        assert!(validate_css(valid_css).is_ok());

        let invalid_braces = ".niri-bar { color: #ffffff;";
        assert!(validate_css(invalid_braces).is_err());
    }

    #[test]
    fn test_default_css_valid_for_gtk() {
        crate::util::run_gtk_test(|| {
            let provider = gtk::CssProvider::new();
            let res = load_css_checked(&provider, DEFAULT_CSS);
            assert!(res.is_ok(), "DEFAULT_CSS failed to parse in GTK4: {:?}", res.err());
        });
    }

    #[test]
    fn test_tray_menu_popover_dark_mode_styling_no_double_card() {
        assert!(
            DEFAULT_CSS.contains("popover.tray-menu") && DEFAULT_CSS.contains("background: transparent;"),
            "Outer popover.tray-menu must have transparent background to prevent double card rendering"
        );
        assert!(
            DEFAULT_CSS.contains("popover.tray-menu > contents") && DEFAULT_CSS.contains("background-color: #313244;"),
            "Inner popover.tray-menu > contents must hold the solid card styling"
        );
    }

    #[test]
    fn test_css_contains_no_gtk3_legacy_menu_selectors() {
        for legacy in ["menuitem", "checkmenuitem", "radiomenuitem"] {
            assert!(
                !DEFAULT_CSS.contains(legacy),
                "DEFAULT_CSS should not contain legacy GTK3 selector '{legacy}'"
            );
        }
    }
}
