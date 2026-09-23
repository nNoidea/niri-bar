use gdk::prelude::*;
use glib::Propagation;
use gtk::prelude::*;
use gtk::{Box as GtkBox, Button, Image, Menu, Orientation, Window};
use std::collections::HashMap;
use std::sync::Arc;

use super::protocol::clean_menu_label;
use super::service::TrayService;
use super::types::{ActivateRequest, MenuItem, ToggleType, TrayEvent, TrayItem, TrayTooltip};
use crate::config::BarPosition;
use crate::icon::update_tray_image;
use crate::layer_shell::{KeyboardMode, LayerShell};
use crate::modules::enable_hover_cursor;

pub struct TrayWidget {
    pub container: GtkBox,
}

impl TrayWidget {
    pub fn new(
        tray_service: &Arc<TrayService>,
        orientation: Orientation,
        position: BarPosition,
        icon_size: i32,
        spacing: i32,
    ) -> Self {
        let container = GtkBox::new(orientation, spacing);
        container.style_context().add_class("tray");
        container.set_halign(gtk::Align::Center);
        container.set_valign(gtk::Align::Center);

        let icon_sz = if icon_size <= 0 { 22 } else { icon_size };
        let (rx, initial_items) = tray_service.subscribe();
        let service = Arc::clone(tray_service);
        let container_clone = container.clone();

        let mut buttons: HashMap<String, (Button, Image)> = HashMap::new();

        // Add all existing items from the service snapshot for this monitor
        for (key, item, _) in initial_items {
            let (btn, img) = Self::create_button(&key, &item, icon_sz, &service, position);
            container_clone.add(&btn);
            buttons.insert(key, (btn, img));
        }

        // Listen for live tray events for this specific bar/monitor
        let service_events = Arc::clone(&service);
        let rx_for_destroy = rx.clone();
        container.connect_destroy(move |_| {
            rx_for_destroy.close();
        });

        glib::MainContext::default().spawn_local(async move {
            while let Ok(msg) = rx.recv().await {
                match msg {
                    TrayEvent::Add(key, item) => {
                        if let Some((old_btn, _)) = buttons.remove(&key) {
                            container_clone.remove(&old_btn);
                        }
                        let (btn, img) = Self::create_button(&key, &item, icon_sz, &service_events, position);
                        container_clone.add(&btn);
                        buttons.insert(key, (btn, img));
                    }
                    TrayEvent::Update(key, item) => {
                        if let Some((button, image)) = buttons.get(&key) {
                            Self::update_item_icon(image, &item, icon_sz);
                            let tooltip_text = Self::compute_tooltip(item.title.as_deref(), item.tool_tip.as_ref());
                            button.set_tooltip_text(tooltip_text.as_deref());
                        }
                    }
                    TrayEvent::Remove(key) => {
                        if let Some((btn, _)) = buttons.remove(&key) {
                            container_clone.remove(&btn);
                        }
                    }
                }
            }
        });

        Self { container }
    }

    pub fn compute_tooltip(title: Option<&str>, tooltip: Option<&TrayTooltip>) -> Option<String> {
        if let Some(tt) = tooltip {
            let t = tt.title.trim();
            let d = tt.description.trim();
            if !t.is_empty() && !d.is_empty() {
                if t.eq_ignore_ascii_case(d) {
                    Some(t.to_string())
                } else {
                    Some(format!("{t}: {d}"))
                }
            } else if !d.is_empty() {
                Some(d.to_string())
            } else if !t.is_empty() {
                Some(t.to_string())
            } else if let Some(title_str) = title {
                let trimmed = title_str.trim();
                if !trimmed.is_empty() {
                    Some(trimmed.to_string())
                } else {
                    None
                }
            } else {
                None
            }
        } else if let Some(title_str) = title {
            let trimmed = title_str.trim();
            if !trimmed.is_empty() {
                Some(trimmed.to_string())
            } else {
                None
            }
        } else {
            None
        }
    }

    fn create_button(
        key: &str,
        item: &TrayItem,
        icon_size: i32,
        service: &Arc<TrayService>,
        position: BarPosition,
    ) -> (Button, Image) {
        let button = Button::new();
        button.style_context().add_class("tray-item");
        button.set_relief(gtk::ReliefStyle::None);
        button.set_halign(gtk::Align::Center);
        button.set_valign(gtk::Align::Center);
        button.set_size_request(icon_size + 6, icon_size + 6);

        enable_hover_cursor(&button);

        let image = Image::new();
        image.set_halign(gtk::Align::Center);
        image.set_valign(gtk::Align::Center);
        image.set_pixel_size(icon_size);
        image.set_size_request(icon_size, icon_size);
        Self::update_item_icon(&image, item, icon_size);

        let tooltip_text = Self::compute_tooltip(item.title.as_deref(), item.tool_tip.as_ref());
        button.set_tooltip_text(tooltip_text.as_deref());

        button.set_image(Some(&image));
        button.set_always_show_image(true);

        let svc = Arc::clone(service);
        let k = key.to_string();
        let btn_weak = button.downgrade();

        button.connect_button_press_event(move |_, ev| {
            let (rx, ry) = ev.root();
            let (item_opt, tray_menu_opt) = svc.get_item_and_menu(&k);
            let item = match item_opt {
                Some(i) => i,
                None => return Propagation::Proceed,
            };

            match ev.button() {
                1 => {
                    // Left Click:
                    if item.item_is_menu {
                        if let (Some(menu_path), Some(tray_menu)) = (item.menu_path.clone(), tray_menu_opt.clone()) {
                            if !tray_menu.submenus.is_empty() {
                                if let Some(btn) = btn_weak.upgrade() {
                                    svc.send_activate(ActivateRequest::AboutToShow {
                                        bus_name: item.bus_name.clone(),
                                        menu_path: menu_path.clone(),
                                        id: 0,
                                    });
                                    let gtk_menu =
                                        build_gtk_menu(&item.bus_name, &menu_path, &tray_menu.submenus, &svc);
                                    Self::popup_tray_menu(&btn, &gtk_menu, ev, position);
                                    return Propagation::Stop;
                                }
                            }
                        }
                    }

                    // Otherwise, send primary Activation
                    svc.send_activate(ActivateRequest::Default {
                        bus_name: item.bus_name.clone(),
                        object_path: item.object_path.clone(),
                        x: rx as i32,
                        y: ry as i32,
                    });
                    return Propagation::Stop;
                }
                2 => {
                    // Middle Click: Secondary Action
                    svc.send_activate(ActivateRequest::Secondary {
                        bus_name: item.bus_name.clone(),
                        object_path: item.object_path.clone(),
                        x: rx as i32,
                        y: ry as i32,
                    });
                    return Propagation::Stop;
                }
                3 => {
                    // Right Click: Show DBusMenu or fallback to ContextMenu
                    let mut showed_popup = false;

                    if let (Some(ref menu_path), Some(ref tray_menu)) = (item.menu_path, tray_menu_opt) {
                        if !tray_menu.submenus.is_empty() {
                            if let Some(btn) = btn_weak.upgrade() {
                                svc.send_activate(ActivateRequest::AboutToShow {
                                    bus_name: item.bus_name.clone(),
                                    menu_path: menu_path.clone(),
                                    id: 0,
                                });
                                let gtk_menu = build_gtk_menu(&item.bus_name, menu_path, &tray_menu.submenus, &svc);
                                Self::popup_tray_menu(&btn, &gtk_menu, ev, position);
                                showed_popup = true;
                            }
                        }
                    }

                    if !showed_popup {
                        svc.send_activate(ActivateRequest::ContextMenu {
                            bus_name: item.bus_name.clone(),
                            object_path: item.object_path.clone(),
                            x: rx as i32,
                            y: ry as i32,
                        });
                    }
                    return Propagation::Stop;
                }
                _ => {}
            }
            Propagation::Proceed
        });

        // Scroll event support
        let svc_scroll = Arc::clone(service);
        let k_scroll = key.to_string();
        button.connect_scroll_event(move |_, ev| {
            let (item_opt, _) = svc_scroll.get_item_and_menu(&k_scroll);
            if let Some(item) = item_opt {
                let (delta, orient) = match ev.direction() {
                    gdk::ScrollDirection::Up => (-1, "vertical"),
                    gdk::ScrollDirection::Down => (1, "vertical"),
                    gdk::ScrollDirection::Left => (-1, "horizontal"),
                    gdk::ScrollDirection::Right => (1, "horizontal"),
                    _ => (0, "vertical"),
                };
                if delta != 0 {
                    svc_scroll.send_activate(ActivateRequest::Scroll {
                        bus_name: item.bus_name,
                        object_path: item.object_path,
                        delta,
                        orientation: orient.to_string(),
                    });
                    return Propagation::Stop;
                }
            }
            Propagation::Proceed
        });

        button.show_all();
        (button, image)
    }

    fn popup_tray_menu(btn: &Button, gtk_menu: &Menu, ev: &gdk::EventButton, position: BarPosition) {
        let toplevel_opt = btn.toplevel().and_then(|w| w.downcast::<Window>().ok());

        if let Some(ref top) = toplevel_opt {
            top.set_keyboard_mode(KeyboardMode::OnDemand);
            let top_weak = top.downgrade();
            gtk_menu.connect_deactivate(move |_| {
                if let Some(w) = top_weak.upgrade() {
                    w.set_keyboard_mode(KeyboardMode::None);
                }
            });
        }

        let (widget_gravity, menu_gravity) = match position {
            BarPosition::Left => (gdk::Gravity::East, gdk::Gravity::West),
            BarPosition::Right => (gdk::Gravity::West, gdk::Gravity::East),
            BarPosition::Top => (gdk::Gravity::South, gdk::Gravity::North),
            BarPosition::Bottom => (gdk::Gravity::North, gdk::Gravity::South),
        };

        gtk_menu.set_attach_widget(Some(btn));
        gtk_menu.show_all();
        gtk_menu.popup_at_widget(btn, widget_gravity, menu_gravity, Some(ev));
    }

    fn update_item_icon(image: &Image, item: &TrayItem, icon_size: i32) {
        let scale = image.scale_factor().max(1);
        update_tray_image(
            image,
            item.icon_name.as_deref(),
            item.icon_pixmap.as_deref(),
            item.icon_theme_path.as_deref(),
            &item.id,
            icon_size,
            scale,
        );
    }
}

pub fn enable_menu_item_hover_cursor(item: &gtk::MenuItem) {
    item.connect_select(|mi| {
        if mi.is_sensitive() {
            if let Some(w) = mi.window() {
                let display = w.display();
                let pointer = gdk::Cursor::from_name(&display, "pointer");
                w.set_cursor(pointer.as_ref());
            }
        }
    });
    item.connect_deselect(|mi| {
        if let Some(w) = mi.window() {
            w.set_cursor(None);
        }
    });
}

fn build_gtk_menu(bus_name: &str, menu_path: &str, submenus: &[MenuItem], service: &Arc<TrayService>) -> Menu {
    let menu = Menu::new();
    menu.style_context().add_class("tray-menu");

    menu.connect_leave_notify_event(|m, _| {
        if let Some(w) = m.window() {
            w.set_cursor(None);
        }
        glib::Propagation::Proceed
    });
    menu.connect_deactivate(|m| {
        if let Some(w) = m.window() {
            w.set_cursor(None);
        }
    });

    let mut last_label: Option<String> = None;
    let mut last_was_sep = true; // prevent leading separator

    for item in submenus {
        if !item.visible {
            continue;
        }

        if item.is_separator {
            if !last_was_sep {
                let sep = gtk::SeparatorMenuItem::new();
                sep.style_context().add_class("tray-menu-separator");
                sep.show();
                menu.append(&sep);
                last_was_sep = true;
            }
            continue;
        }

        let raw_label = item.label.as_deref().unwrap_or("");
        let clean_label = clean_menu_label(raw_label);

        if clean_label.is_empty() && item.submenu.is_empty() {
            continue;
        }

        // If this item is an identical consecutive duplicate of the last item, skip it
        if let Some(ref prev) = last_label {
            if prev == &clean_label && item.submenu.is_empty() {
                continue;
            }
        }

        if !item.submenu.is_empty() {
            // Check if item.submenu contains a single wrapper with the same name
            let effective_submenus = if item.submenu.len() == 1 {
                let child = &item.submenu[0];
                let child_label = clean_menu_label(child.label.as_deref().unwrap_or(""));

                if (child_label == clean_label || child_label.is_empty()) && !child.submenu.is_empty() {
                    &child.submenu
                } else {
                    &item.submenu
                }
            } else {
                &item.submenu
            };

            let mi = gtk::MenuItem::with_label(&clean_label);
            mi.style_context().add_class("tray-menu-item");
            let sub = build_gtk_menu(bus_name, menu_path, effective_submenus, service);
            mi.set_submenu(Some(&sub));
            mi.set_sensitive(item.enabled);
            enable_menu_item_hover_cursor(&mi);
            mi.show();
            menu.append(&mi);
            last_label = Some(clean_label);
            last_was_sep = false;
        } else {
            let mi = match item.toggle_type {
                Some(ToggleType::Checkmark) | Some(ToggleType::Radio) => {
                    let cmi = gtk::CheckMenuItem::with_label(&clean_label);
                    cmi.set_active(item.toggle_state == 1);
                    cmi.upcast::<gtk::MenuItem>()
                }
                _ => gtk::MenuItem::with_label(&clean_label),
            };

            mi.style_context().add_class("tray-menu-item");
            mi.set_sensitive(item.enabled);
            enable_menu_item_hover_cursor(&mi);

            let item_id = item.id;
            let svc = Arc::clone(service);
            let b_name = bus_name.to_string();
            let m_path = menu_path.to_string();

            mi.connect_activate(move |_| {
                svc.send_activate(ActivateRequest::MenuItem {
                    bus_name: b_name.clone(),
                    menu_path: m_path.clone(),
                    submenu_id: item_id,
                });
            });

            mi.show();
            menu.append(&mi);
            last_label = Some(clean_label);
            last_was_sep = false;
        }
    }

    menu
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_tooltip() {
        // Both title and description present and distinct
        let tt = TrayTooltip {
            title: "Dropbox".to_string(),
            description: "Up to date".to_string(),
        };
        assert_eq!(
            TrayWidget::compute_tooltip(Some("Dropbox App"), Some(&tt)),
            Some("Dropbox: Up to date".to_string())
        );

        // Same title and description
        let tt_same = TrayTooltip {
            title: "Steam".to_string(),
            description: "Steam".to_string(),
        };
        assert_eq!(
            TrayWidget::compute_tooltip(Some("Steam"), Some(&tt_same)),
            Some("Steam".to_string())
        );

        // Only description
        let tt_desc = TrayTooltip {
            title: "".to_string(),
            description: "Synchronizing 5 files".to_string(),
        };
        assert_eq!(
            TrayWidget::compute_tooltip(None, Some(&tt_desc)),
            Some("Synchronizing 5 files".to_string())
        );

        // Only title fallback
        assert_eq!(
            TrayWidget::compute_tooltip(Some("Obsidian"), None),
            Some("Obsidian".to_string())
        );

        // None
        assert_eq!(TrayWidget::compute_tooltip(None, None), None);
    }

    #[test]
    fn test_menu_item_hover_cursor_behavior() {
        if gtk::is_initialized() {
            if !gtk::is_initialized_main_thread() {
                eprintln!("Skipping GTK test: running on non-main GTK thread");
                return;
            }
        } else if gtk::init().is_err() {
            eprintln!("Skipping GTK test: display not available");
            return;
        }

        use std::sync::Mutex;
        let menu = Menu::new();
        let item1 = gtk::MenuItem::with_label("Item 1");
        let item2 = gtk::MenuItem::with_label("Item 2");
        enable_menu_item_hover_cursor(&item1);
        enable_menu_item_hover_cursor(&item2);
        menu.append(&item1);
        menu.append(&item2);

        // Check has_window is false
        assert!(!gtk::prelude::WidgetExt::has_window(&item1));

        let events = Arc::new(Mutex::new(Vec::new()));
        let e1 = Arc::clone(&events);
        item1.connect_select(move |_| {
            crate::util::lock(&e1).push("item1_select");
        });
        let e2 = Arc::clone(&events);
        item1.connect_deselect(move |_| {
            crate::util::lock(&e2).push("item1_deselect");
        });
        let e3 = Arc::clone(&events);
        item2.connect_select(move |_| {
            crate::util::lock(&e3).push("item2_select");
        });
        let e4 = Arc::clone(&events);
        item2.connect_deselect(move |_| {
            crate::util::lock(&e4).push("item2_deselect");
        });

        menu.show_all();
        menu.realize();
        item1.realize();
        item2.realize();

        // Simulate moving from item1 to item2
        item1.emit_by_name::<()>("select", &[]);
        if let Some(w) = item1.window() {
            assert!(w.cursor().is_some());
        }

        item1.emit_by_name::<()>("deselect", &[]);
        if let Some(w) = item1.window() {
            assert!(w.cursor().is_none());
        }

        item2.emit_by_name::<()>("select", &[]);
        if let Some(w) = item2.window() {
            assert!(w.cursor().is_some());
        }

        let logged = crate::util::lock(&events).clone();
        assert_eq!(logged, vec!["item1_select", "item1_deselect", "item2_select"]);

        // Insensitive items should not set pointer cursor
        item1.set_sensitive(false);
        item1.emit_by_name::<()>("select", &[]);
        if let Some(w) = item1.window() {
            assert!(w.cursor().is_none());
        }
    }
}
