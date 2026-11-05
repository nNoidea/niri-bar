use glib::Propagation;
use gtk::prelude::*;
use gtk::{Box as GtkBox, Button, Image, Label, Orientation, Popover, Separator, Window};
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
        container.add_css_class("tray");
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
            container_clone.append(&btn);
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
                        container_clone.append(&btn);
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
        button.add_css_class("tray-item");
        button.set_has_frame(false);
        button.set_focusable(false);
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

        button.set_child(Some(&image));

        let svc = Arc::clone(service);
        let k = key.to_string();
        let btn_weak = button.downgrade();

        let click = gtk::GestureClick::builder()
            .button(0)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        click.connect_pressed(move |gesture, n_press, x, y| {
            if n_press <= 0 {
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let Some(btn) = btn_weak.upgrade() else {
                return;
            };
            let (rx, ry) = crate::util::widget_to_toplevel_coords(&btn, x, y);
            let (item_opt, tray_menu_opt) = svc.get_item_and_menu(&k);
            let Some(item) = item_opt else {
                return;
            };

            match gesture.current_button() {
                1 => {
                    // Left Click:
                    if item.item_is_menu {
                        if let (Some(menu_path), Some(tray_menu)) = (item.menu_path.clone(), tray_menu_opt) {
                            if !tray_menu.submenus.is_empty() {
                                svc.send_activate(ActivateRequest::AboutToShow {
                                    bus_name: item.bus_name.clone(),
                                    menu_path: menu_path.clone(),
                                    id: 0,
                                });
                                let popover = build_tray_popover(&item.bus_name, &menu_path, &tray_menu.submenus, &svc);
                                Self::popup_tray_menu(&btn, &popover, position);
                                return;
                            }
                        }
                    }

                    // Otherwise, send primary Activation
                    svc.send_activate(ActivateRequest::Default {
                        bus_name: item.bus_name.clone(),
                        object_path: item.object_path,
                        x: rx as i32,
                        y: ry as i32,
                    });
                }
                2 => {
                    // Middle Click: Secondary Action
                    svc.send_activate(ActivateRequest::Secondary {
                        bus_name: item.bus_name.clone(),
                        object_path: item.object_path,
                        x: rx as i32,
                        y: ry as i32,
                    });
                }
                3 => {
                    // Right Click: Show DBusMenu or fallback to ContextMenu
                    let mut showed_popup = false;

                    if let (Some(ref menu_path), Some(ref tray_menu)) = (item.menu_path, tray_menu_opt) {
                        if !tray_menu.submenus.is_empty() {
                            svc.send_activate(ActivateRequest::AboutToShow {
                                bus_name: item.bus_name.clone(),
                                menu_path: menu_path.clone(),
                                id: 0,
                            });
                            let popover = build_tray_popover(&item.bus_name, menu_path, &tray_menu.submenus, &svc);
                            Self::popup_tray_menu(&btn, &popover, position);
                            showed_popup = true;
                        }
                    }

                    if !showed_popup {
                        svc.send_activate(ActivateRequest::ContextMenu {
                            bus_name: item.bus_name.clone(),
                            object_path: item.object_path,
                            x: rx as i32,
                            y: ry as i32,
                        });
                    }
                }
                _ => {}
            }
        });
        button.add_controller(click);

        // Scroll event support
        let svc_scroll = Arc::clone(service);
        let k_scroll = key.to_string();
        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::BOTH_AXES | gtk::EventControllerScrollFlags::DISCRETE,
        );
        scroll.connect_scroll(move |_, dx, dy| {
            let (item_opt, _) = svc_scroll.get_item_and_menu(&k_scroll);
            if let Some(item) = item_opt {
                let (delta, orient) = if dy < 0.0 {
                    (-1, "vertical")
                } else if dy > 0.0 {
                    (1, "vertical")
                } else if dx < 0.0 {
                    (-1, "horizontal")
                } else if dx > 0.0 {
                    (1, "horizontal")
                } else {
                    (0, "vertical")
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
        button.add_controller(scroll);

        (button, image)
    }

    fn popup_tray_menu(btn: &Button, popover: &Popover, position: BarPosition) {
        // If an existing popover is currently visible on this button, toggle it closed.
        let mut child = btn.first_child();
        let mut had_open_popover = false;
        while let Some(c) = child {
            let next = c.next_sibling();
            if let Some(existing_popover) = c.downcast_ref::<Popover>() {
                if existing_popover.is_visible() {
                    existing_popover.popdown();
                    had_open_popover = true;
                }
            }
            child = next;
        }
        if had_open_popover {
            return;
        }

        let toplevel_opt = btn.root().and_then(|w| w.downcast::<Window>().ok());

        let pos_type = match position {
            BarPosition::Left => gtk::PositionType::Right,
            BarPosition::Right => gtk::PositionType::Left,
            BarPosition::Top => gtk::PositionType::Bottom,
            BarPosition::Bottom => gtk::PositionType::Top,
        };
        popover.set_position(pos_type);
        popover.set_parent(btn);

        let top_weak = toplevel_opt.as_ref().map(|t| t.downgrade());
        if let Some(ref top) = toplevel_opt {
            top.set_keyboard_mode(KeyboardMode::OnDemand);
        }

        popover.connect_closed(move |p| {
            if let Some(ref tw) = top_weak {
                if let Some(w) = tw.upgrade() {
                    w.set_keyboard_mode(KeyboardMode::None);
                }
            }
            let p_clone = p.clone();
            glib::idle_add_local_once(move || {
                p_clone.unparent();
            });
        });

        popover.popup();
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

pub fn enable_menu_item_hover_cursor(item: &Button) {
    let apply = |btn: &Button| {
        if btn.is_sensitive() {
            btn.set_cursor_from_name(Some("pointer"));
        } else {
            btn.set_cursor_from_name(None);
        }
    };
    apply(item);
    item.connect_sensitive_notify(apply);
}

fn build_tray_popover(bus_name: &str, menu_path: &str, submenus: &[MenuItem], service: &Arc<TrayService>) -> Popover {
    let popover = Popover::new();
    popover.add_css_class("tray-menu");
    popover.set_has_arrow(false);
    popover.set_autohide(true);

    let vbox = GtkBox::new(Orientation::Vertical, 1);
    append_menu_items(&vbox, &popover, bus_name, menu_path, submenus, service);
    popover.set_child(Some(&vbox));
    popover
}

fn append_menu_items(
    container: &GtkBox,
    root_popover: &Popover,
    bus_name: &str,
    menu_path: &str,
    submenus: &[MenuItem],
    service: &Arc<TrayService>,
) {
    let mut last_label: Option<String> = None;
    let mut last_was_sep = true; // prevent leading separator

    for item in submenus {
        if !item.visible {
            continue;
        }

        if item.is_separator {
            if !last_was_sep {
                let sep = Separator::new(Orientation::Horizontal);
                sep.add_css_class("tray-menu-separator");
                container.append(&sep);
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

            let mi = Button::new();
            mi.set_has_frame(false);
            mi.set_focusable(false);
            mi.add_css_class("tray-menu-item");
            let lbl = Label::new(Some(&format!("{clean_label} ▸")));
            lbl.set_xalign(0.0);
            mi.set_child(Some(&lbl));
            mi.set_sensitive(item.enabled);
            enable_menu_item_hover_cursor(&mi);

            let sub_box = GtkBox::new(Orientation::Vertical, 1);
            sub_box.set_margin_start(10);
            sub_box.set_visible(false);
            append_menu_items(&sub_box, root_popover, bus_name, menu_path, effective_submenus, service);

            let sub_toggle = sub_box.clone();
            mi.connect_clicked(move |_| {
                sub_toggle.set_visible(!sub_toggle.is_visible());
            });

            container.append(&mi);
            container.append(&sub_box);
            last_label = Some(clean_label);
            last_was_sep = false;
        } else {
            let display_text = match item.toggle_type {
                Some(ToggleType::Checkmark) | Some(ToggleType::Radio) => {
                    if item.toggle_state == 1 {
                        format!("✓ {clean_label}")
                    } else {
                        format!("   {clean_label}")
                    }
                }
                _ => clean_label.clone(),
            };

            let mi = Button::new();
            mi.set_has_frame(false);
            mi.set_focusable(false);
            mi.add_css_class("tray-menu-item");
            let lbl = Label::new(Some(&display_text));
            lbl.set_xalign(0.0);
            mi.set_child(Some(&lbl));
            mi.set_sensitive(item.enabled);
            enable_menu_item_hover_cursor(&mi);

            let item_id = item.id;
            let svc = Arc::clone(service);
            let b_name = bus_name.to_string();
            let m_path = menu_path.to_string();
            let pop_weak = root_popover.downgrade();

            mi.connect_clicked(move |_| {
                if let Some(p) = pop_weak.upgrade() {
                    p.popdown();
                }
                svc.send_activate(ActivateRequest::MenuItem {
                    bus_name: b_name.clone(),
                    menu_path: m_path.clone(),
                    submenu_id: item_id,
                });
            });

            container.append(&mi);
            last_label = Some(clean_label);
            last_was_sep = false;
        }
    }
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
        crate::util::run_gtk_test(|| {
            let item1 = Button::with_label("Item 1");
            let item2 = Button::with_label("Item 2");
            enable_menu_item_hover_cursor(&item1);
            enable_menu_item_hover_cursor(&item2);

            assert!(item1.cursor().is_some());
            assert!(item2.cursor().is_some());

            // Insensitive items should not set pointer cursor
            item1.set_sensitive(false);
            assert!(item1.cursor().is_none());

            // Re-enabling sensitivity restores pointer cursor
            item1.set_sensitive(true);
            assert!(item1.cursor().is_some());
        });
    }

    #[test]
    fn test_tray_popover_and_default_css_do_not_double_card_background() {
        let default_css = include_str!("../../../resources/style.default.css");
        let user_css = include_str!("../../../.config/niri-bar/style.css");
        for (name, css) in [("style.default.css", default_css), ("user style.css", user_css)] {
            assert!(
                css.contains("popover.tray-menu > contents"),
                "{name} must style `popover.tray-menu > contents` as the single card container"
            );
            assert!(
                css.contains("popover.tray-menu,\npopover.tray-menu.background,\npopover.tray-menu:backdrop {\n    background: transparent;"),
                "{name} must explicitly reset outer `popover.tray-menu` to transparent so it is not rendered inside a second card"
            );
            assert!(
                !css.contains("popover.tray-menu,\npopover.tray-menu contents"),
                "{name} must not apply the card background/border to both outer `popover.tray-menu` and inner `contents`"
            );
        }
    }

    #[test]
    fn test_tray_popover_toggle_behavior() {
        crate::util::run_gtk_test(|| {
            let win = Window::new();
            let btn = Button::new();
            win.set_child(Some(&btn));
            win.present();

            let pop1 = Popover::new();
            TrayWidget::popup_tray_menu(&btn, &pop1, BarPosition::Left);
            assert!(pop1.is_visible());

            // Second popup call on the same button while pop1 is visible should dismiss pop1 and not open pop2
            let pop2 = Popover::new();
            TrayWidget::popup_tray_menu(&btn, &pop2, BarPosition::Left);
            assert!(!pop1.is_visible());
            assert!(!pop2.is_visible());
        });
    }
}
