use futures_util::future::{select, Either};
use glib::Propagation;
use gtk::prelude::*;
use gtk::{Box as GtkBox, Button, Image, Label, Orientation, Popover, Separator, Window};
use std::collections::HashMap;
use std::pin::pin;
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
    #[cfg(test)]
    scale_tx: async_channel::Sender<i32>,
}

enum TrayLoopEvent {
    Service(TrayEvent),
    Scale(i32),
}

impl TrayWidget {
    pub fn new(
        tray_service: &Arc<TrayService>,
        orientation: Orientation,
        position: BarPosition,
        icon_size: i32,
        spacing: i32,
        initial_scale: i32,
    ) -> Self {
        let container = GtkBox::new(orientation, spacing);
        container.add_css_class("tray");
        container.set_halign(gtk::Align::Center);
        container.set_valign(gtk::Align::Center);

        let icon_sz = if icon_size <= 0 { 22 } else { icon_size };
        let mut current_scale = initial_scale.max(1);
        let (rx, initial_items) = tray_service.subscribe();
        let (scale_tx, scale_rx) = async_channel::unbounded::<i32>();
        let service = Arc::clone(tray_service);

        let mut buttons: HashMap<String, (Button, Image)> = HashMap::new();

        // Add all existing items from the service snapshot for this monitor
        for (key, item, _) in initial_items {
            let (btn, img) = Self::create_button(&key, &item, icon_sz, current_scale, &service, position);
            container.append(&btn);
            buttons.insert(key, (btn, img));
        }

        #[cfg(test)]
        let scale_tx_notify = scale_tx.clone();
        #[cfg(not(test))]
        let scale_tx_notify = scale_tx;
        container.connect_scale_factor_notify(move |c| {
            let _ = scale_tx_notify.try_send(c.scale_factor().max(1));
        });

        // Listen for live tray events and dynamic scale changes for this specific bar/monitor
        let service_events = Arc::clone(&service);
        let rx_for_destroy = rx.clone();
        let scale_rx_for_destroy = scale_rx.clone();
        container.connect_destroy(move |_| {
            rx_for_destroy.close();
            scale_rx_for_destroy.close();
        });

        let container_weak = container.downgrade();
        glib::MainContext::default().spawn_local(async move {
            loop {
                let next = {
                    let svc_fut = pin!(rx.recv());
                    let scale_fut = pin!(scale_rx.recv());
                    match select(svc_fut, scale_fut).await {
                        Either::Left((res, _)) => match res {
                            Ok(ev) => Some(TrayLoopEvent::Service(ev)),
                            Err(_) => None,
                        },
                        Either::Right((res, _)) => match res {
                            Ok(s) => Some(TrayLoopEvent::Scale(s)),
                            Err(_) => None,
                        },
                    }
                };
                let Some(ev) = next else {
                    break;
                };
                let Some(cont) = container_weak.upgrade() else {
                    break;
                };
                match ev {
                    TrayLoopEvent::Service(TrayEvent::Add(key, item)) => {
                        if let Some((old_btn, _)) = buttons.remove(&key) {
                            cont.remove(&old_btn);
                        }
                        let (btn, img) =
                            Self::create_button(&key, &item, icon_sz, current_scale, &service_events, position);
                        cont.append(&btn);
                        buttons.insert(key, (btn, img));
                    }
                    TrayLoopEvent::Service(TrayEvent::Update(key, item)) => {
                        if let Some((button, image)) = buttons.get(&key) {
                            Self::update_item_icon(image, &item, icon_sz, current_scale);
                            let tooltip_text = Self::compute_tooltip(item.title.as_deref(), item.tool_tip.as_ref());
                            button.set_tooltip_text(tooltip_text.as_deref());
                        }
                    }
                    TrayLoopEvent::Service(TrayEvent::Remove(key)) => {
                        if let Some((btn, _)) = buttons.remove(&key) {
                            cont.remove(&btn);
                        }
                    }
                    TrayLoopEvent::Scale(new_scale) => {
                        let clamped = new_scale.max(1);
                        if clamped != current_scale {
                            current_scale = clamped;
                            for (key, (_, image)) in &buttons {
                                if let (Some(item), _) = service_events.get_item_and_menu(key) {
                                    Self::update_item_icon(image, &item, icon_sz, current_scale);
                                }
                            }
                        }
                    }
                }
            }
            rx.close();
            scale_rx.close();
        });

        Self {
            container,
            #[cfg(test)]
            scale_tx,
        }
    }

    /// Update the active HiDPI scale factor used for tray icon rendering.
    #[cfg(test)]
    pub fn set_scale_factor(&self, scale: i32) {
        let _ = self.scale_tx.try_send(scale.max(1));
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
        scale_factor: i32,
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
        Self::update_item_icon(&image, item, icon_size, scale_factor);

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

    fn update_item_icon(image: &Image, item: &TrayItem, icon_size: i32, scale_factor: i32) {
        update_tray_image(
            image,
            item.icon_name.as_deref(),
            item.icon_pixmap.as_deref(),
            item.icon_theme_path.as_deref(),
            &item.id,
            icon_size,
            scale_factor.max(1),
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
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let user_css_path = manifest_dir.join(".config/niri-bar/style.css");
        let user_css = std::fs::read_to_string(&user_css_path).ok();

        let mut targets = vec![("style.default.css", default_css.to_string())];
        if let Some(css) = user_css {
            targets.push(("user style.css", css));
        }

        for (name, css) in targets {
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

    #[test]
    fn test_tray_widget_destroys_and_unsubscribes_on_window_close() {
        crate::util::run_gtk_test(|| {
            use std::cell::Cell;
            use std::rc::Rc;

            let svc = Arc::new(TrayService::new_for_test());
            svc.insert_test_item(
                "org.example.TrayApp/StatusNotifierItem".to_string(),
                TrayItem {
                    bus_name: "org.example.TrayApp".to_string(),
                    object_path: "/StatusNotifierItem".to_string(),
                    id: "org.example.TrayApp".to_string(),
                    title: Some("Example".to_string()),
                    icon_name: None,
                    icon_pixmap: None,
                    icon_theme_path: None,
                    tool_tip: None,
                    item_is_menu: false,
                    menu_path: None,
                },
            );

            let tray = TrayWidget::new(&svc, Orientation::Vertical, BarPosition::Left, 22, 4, 1);
            assert_eq!(svc.subscriber_count(), 1);
            let children = crate::util::widget_children(&tray.container);
            assert_eq!(children.len(), 1);

            let container_destroyed = Rc::new(Cell::new(false));
            let btn_destroyed = Rc::new(Cell::new(false));
            let c_flag = Rc::clone(&container_destroyed);
            let b_flag = Rc::clone(&btn_destroyed);
            tray.container.connect_destroy(move |_| c_flag.set(true));
            children[0].connect_destroy(move |_| b_flag.set(true));
            drop(children);

            let win = Window::new();
            win.set_child(Some(&tray.container));
            drop(tray);
            win.set_visible(true);
            win.close();
            drop(win);

            let ctx = glib::MainContext::default();
            for _ in 0..10 {
                while ctx.pending() {
                    ctx.iteration(false);
                }
            }

            assert!(container_destroyed.get(), "Tray container must emit destroy on close");
            assert!(btn_destroyed.get(), "Tray button must emit destroy without ref cycle");
            assert_eq!(
                svc.subscriber_count(),
                0,
                "TrayWidget subscriber channel must close when container is destroyed"
            );
        });
    }

    #[test]
    fn test_tray_widget_uses_explicit_scale_and_updates_on_scale_change() {
        crate::util::run_gtk_test(|| {
            let svc = Arc::new(TrayService::new_for_test());
            let pixmap = crate::icon::RawIconPixmap {
                width: 44,
                height: 44,
                pixels: vec![255; 44 * 44 * 4],
            };
            svc.insert_test_item(
                "org.example.ScaleTray/StatusNotifierItem".to_string(),
                TrayItem {
                    bus_name: "org.example.ScaleTray".to_string(),
                    object_path: "/StatusNotifierItem".to_string(),
                    id: "org.example.ScaleTray".to_string(),
                    title: Some("Scale Tray".to_string()),
                    icon_name: None,
                    icon_pixmap: Some(vec![pixmap]),
                    icon_theme_path: None,
                    tool_tip: None,
                    item_is_menu: false,
                    menu_path: None,
                },
            );

            let tray_1x = TrayWidget::new(&svc, Orientation::Vertical, BarPosition::Left, 22, 4, 1);
            let btn_1x = crate::util::widget_children(&tray_1x.container)
                .into_iter()
                .next()
                .unwrap()
                .downcast::<Button>()
                .unwrap();
            let img_1x = btn_1x.child().unwrap().downcast::<Image>().unwrap();
            assert_eq!(img_1x.pixel_size(), 22);
            assert_eq!(
                img_1x.paintable().unwrap().intrinsic_width(),
                22,
                "TrayWidget initialized with scale 1 must render 22px icon buffer even before map"
            );

            let tray_2x = TrayWidget::new(&svc, Orientation::Vertical, BarPosition::Left, 22, 4, 2);
            let btn_2x = crate::util::widget_children(&tray_2x.container)
                .into_iter()
                .next()
                .unwrap()
                .downcast::<Button>()
                .unwrap();
            let img_2x = btn_2x.child().unwrap().downcast::<Image>().unwrap();
            assert_eq!(img_2x.pixel_size(), 22);
            assert_eq!(
                img_2x.paintable().unwrap().intrinsic_width(),
                44,
                "TrayWidget initialized with scale 2 must render 44px icon buffer even before map"
            );

            // Attach containers so window close emits destroy and closes async channels cleanly.
            let win_1x = Window::new();
            win_1x.set_child(Some(&tray_1x.container));
            let win_2x = Window::new();
            win_2x.set_child(Some(&tray_2x.container));

            // Dynamic scale transition 1 -> 2 must re-render existing tray buttons at 44px
            tray_1x.set_scale_factor(2);
            let ctx = glib::MainContext::default();
            for _ in 0..10 {
                while ctx.pending() {
                    ctx.iteration(false);
                }
            }
            assert_eq!(img_1x.pixel_size(), 22);
            assert_eq!(
                img_1x.paintable().unwrap().intrinsic_width(),
                44,
                "TrayWidget scale transition 1 -> 2 must refresh existing icon buffer to 44px"
            );

            // Dynamic scale transition 2 -> 1 must re-render back down to 22px
            tray_1x.set_scale_factor(1);
            for _ in 0..10 {
                while ctx.pending() {
                    ctx.iteration(false);
                }
            }
            assert_eq!(
                img_1x.paintable().unwrap().intrinsic_width(),
                22,
                "TrayWidget scale transition 2 -> 1 must refresh existing icon buffer back to 22px"
            );

            win_1x.close();
            win_2x.close();
            for _ in 0..10 {
                while ctx.pending() {
                    ctx.iteration(false);
                }
            }
        });
    }
}
