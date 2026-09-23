use std::collections::HashMap;

use crate::icon::RawIconPixmap;

pub type TrayIconPixmap = RawIconPixmap;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayTooltip {
    pub title: String,
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct TrayItem {
    pub bus_name: String,
    pub object_path: String,
    pub id: String,
    pub title: Option<String>,
    pub icon_name: Option<String>,
    pub icon_theme_path: Option<String>,
    pub icon_pixmap: Option<Vec<TrayIconPixmap>>,
    pub tool_tip: Option<TrayTooltip>,
    pub item_is_menu: bool,
    pub menu_path: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToggleType {
    Checkmark,
    Radio,
}

#[derive(Clone, Debug)]
pub struct MenuItem {
    pub id: i32,
    pub label: Option<String>,
    pub enabled: bool,
    pub visible: bool,
    pub is_separator: bool,
    pub toggle_type: Option<ToggleType>,
    pub toggle_state: i32,
    pub submenu: Vec<MenuItem>,
}

#[derive(Clone, Debug, Default)]
pub struct TrayMenu {
    pub submenus: Vec<MenuItem>,
}

#[derive(Clone, Debug)]
pub enum TrayEvent {
    Add(String, Box<TrayItem>),
    Update(String, Box<TrayItem>),
    Remove(String),
}

#[derive(Clone, Debug)]
pub enum ActivateRequest {
    Default {
        bus_name: String,
        object_path: String,
        x: i32,
        y: i32,
    },
    Secondary {
        bus_name: String,
        object_path: String,
        x: i32,
        y: i32,
    },
    ContextMenu {
        bus_name: String,
        object_path: String,
        x: i32,
        y: i32,
    },
    Scroll {
        bus_name: String,
        object_path: String,
        delta: i32,
        orientation: String,
    },
    MenuItem {
        bus_name: String,
        menu_path: String,
        submenu_id: i32,
    },
    AboutToShow {
        bus_name: String,
        menu_path: String,
        id: i32,
    },
}

pub type TrayItemMap = HashMap<String, (TrayItem, Option<TrayMenu>)>;
pub type TrayItemSnapshot = (String, TrayItem, Option<TrayMenu>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tooltip_default_empty() {
        let t = TrayTooltip::default();
        assert_eq!(t.title, "");
        assert_eq!(t.description, "");
    }

    #[test]
    fn test_tray_tooltip_fields() {
        let t = TrayTooltip {
            title: "App".to_string(),
            description: "Running".to_string(),
        };
        assert_eq!(t.title, "App");
        assert_eq!(t.description, "Running");
    }

    #[test]
    fn test_menu_default_empty() {
        let m = TrayMenu::default();
        assert!(m.submenus.is_empty());
    }
}
