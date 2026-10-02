use glib::Propagation;
use gtk::prelude::*;
use gtk::{Box as GtkBox, Button, Orientation};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use crate::config::TaskbarConfig;
use crate::niri::drag::*;
use crate::niri::ipc;
use crate::niri::model::WindowInfo;
use crate::niri::{AppState, NiriService};

pub struct WindowWidget {
    pub button: Button,
    pub icon_key: Rc<RefCell<String>>,
    pub icon_scale: i32,
    pub is_focused: bool,
    pub tooltip: String,
}

pub struct TaskbarWidget {
    pub container: GtkBox,
    pub active_widgets: Rc<RefCell<HashMap<u64, WindowWidget>>>,
    pub orientation: Orientation,
    pub bar_position: crate::config::BarPosition,
    pub bar_size: i32,
    pub viewport: Rc<RefCell<TaskbarViewport>>,
}

/// Active workspace per output: focused > active > any. Pure for testing.
pub fn active_workspace_per_output(workspaces: &[crate::niri::model::WorkspaceInfo]) -> HashMap<String, u64> {
    let mut map: HashMap<String, u64> = HashMap::new();
    for ws in workspaces {
        if ws.is_focused {
            if let Some(ref out) = ws.output {
                map.insert(out.clone(), ws.id);
            }
        }
    }
    for ws in workspaces {
        if ws.is_active {
            if let Some(ref out) = ws.output {
                map.entry(out.clone()).or_insert(ws.id);
            }
        }
    }
    for ws in workspaces {
        if let Some(ref out) = ws.output {
            map.entry(out.clone()).or_insert(ws.id);
        }
    }
    map
}

/// Sort spatially by workspace order, then column index, then tile index. Windows without layout go last.
pub fn sort_windows_spatially(windows: &mut [WindowInfo], workspaces: &[crate::niri::model::WorkspaceInfo]) {
    let ws_order: HashMap<u64, usize> = workspaces
        .iter()
        .enumerate()
        .map(|(i, ws)| {
            let order = if ws.idx > 0 { ws.idx as usize } else { i + 1 };
            (ws.id, order)
        })
        .collect();

    windows.sort_by_key(|w| {
        let ws_idx = w
            .workspace_id
            .and_then(|id| ws_order.get(&id).copied())
            .unwrap_or(usize::MAX);
        let (col, tile) = w
            .layout
            .as_ref()
            .and_then(|l| l.pos_in_scrolling_layout)
            .unwrap_or((usize::MAX, usize::MAX));
        (ws_idx, col, tile, w.id)
    });
}

/// Filter to windows visible on `current_output`. `None` output shows
/// nothing (never leak other monitors' windows). Pure for testing.
pub fn filter_windows_for_output(
    windows: &[WindowInfo],
    workspaces: &[crate::niri::model::WorkspaceInfo],
    current_output: &Option<String>,
    only_current_workspace: bool,
) -> Vec<WindowInfo> {
    let active_map = active_workspace_per_output(workspaces);
    let mut filtered: Vec<WindowInfo> = windows
        .iter()
        .filter(|w| {
            let ws_id = match w.workspace_id {
                Some(id) => id,
                None => return false,
            };
            let ws = match workspaces.iter().find(|ws| ws.id == ws_id) {
                Some(ws) => ws,
                None => return false,
            };
            let win_out = match ws.output {
                Some(ref o) => o,
                None => return false,
            };

            if let Some(ref bar_out) = current_output {
                if win_out != bar_out {
                    return false;
                }
            } else {
                // Do not leak or mirror other monitors' windows if output is unresolved
                return false;
            }

            if only_current_workspace {
                if let Some(&active_ws_id) = active_map.get(win_out) {
                    if ws.id != active_ws_id {
                        return false;
                    }
                }
            }

            true
        })
        .cloned()
        .collect();
    sort_windows_spatially(&mut filtered, workspaces);
    filtered
}

/// Single Source of Truth (SSOT) for the taskbar's visible scrolling viewport.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskbarViewport {
    pub start: usize,
    pub last_focused_id: Option<u64>,
    pub total_count: usize,
    pub capacity: usize,
}

impl TaskbarViewport {
    pub fn new() -> Self {
        Self::default()
    }

    /// User-initiated scroll: moves `start` by `delta` (+1 or -1), strictly clamped to [0, max_start].
    /// Preserves manual scroll position without resetting `last_focused_id`.
    pub fn scroll_by(&mut self, delta: i32) {
        if self.total_count <= self.capacity || self.capacity == 0 {
            self.start = 0;
            return;
        }
        let max_start = self.total_count.saturating_sub(self.capacity);
        if delta > 0 {
            self.start = (self.start + delta as usize).min(max_start);
        } else if delta < 0 {
            self.start = self.start.saturating_sub((-delta) as usize);
        }
    }

    /// Update viewport during reconcile.
    /// - Clamps `start` if window count or capacity shrank.
    /// - Only shifts `start` based on `focused_idx` if `focused_id` changed (user selected another window).
    pub fn update(
        &mut self,
        focused_id: Option<u64>,
        focused_idx: Option<usize>,
        total_count: usize,
        capacity: usize,
        safety_margin: usize,
    ) {
        self.total_count = total_count;
        self.capacity = capacity;

        if total_count <= capacity || capacity == 0 {
            self.start = 0;
            self.last_focused_id = focused_id;
            return;
        }

        let max_start = total_count.saturating_sub(capacity);
        self.start = self.start.min(max_start);

        let focus_changed = focused_id != self.last_focused_id;
        self.last_focused_id = focused_id;

        if focus_changed {
            if let Some(f_idx) = focused_idx {
                if f_idx < total_count {
                    let safe_margin = safety_margin.min((capacity.saturating_sub(1)) / 2);
                    let upper_threshold = self.start + capacity - 1 - safe_margin;
                    let lower_threshold = self.start + safe_margin;

                    if f_idx > upper_threshold {
                        let shift = f_idx - upper_threshold;
                        self.start = (self.start + shift).min(max_start);
                    } else if f_idx < lower_threshold {
                        let shift = lower_threshold - f_idx;
                        self.start = self.start.saturating_sub(shift);
                    }
                }
            }
        }
    }

    /// Calculate visible window slice `[start, end)`.
    /// When `total_count > capacity`, renders `capacity + 1` icons so the cut-off
    /// half icon is visible peeking into the modules area, preserving CSS padding.
    pub fn visible_range(&self) -> (usize, usize) {
        if self.total_count == 0 || self.capacity == 0 {
            return (0, 0);
        }
        if self.total_count <= self.capacity {
            return (0, self.total_count);
        }
        let end = (self.start + self.capacity + 1).min(self.total_count);
        (self.start, end)
    }
}

/// Calculate the visible window slice `[start, end)` for taskbar icons.
///
/// Ensures the taskbar never renders more than `capacity` items, staying within
/// the available bar space without overlapping or pushing modules.
/// Tracks `focused_idx` and shifts `start` with hysteresis and an optional `safety_margin`.
#[cfg(test)]
pub fn calculate_sliding_window(
    current_start: usize,
    total_count: usize,
    capacity: usize,
    safety_margin: usize,
    focused_idx: Option<usize>,
) -> (usize, usize) {
    let mut vp = TaskbarViewport {
        start: current_start,
        last_focused_id: None,
        total_count,
        capacity,
    };
    vp.update(Some(1), focused_idx, total_count, capacity, safety_margin);
    vp.visible_range()
}

impl TaskbarWidget {
    pub fn new(
        orientation: Orientation,
        spacing: i32,
        bar_position: crate::config::BarPosition,
        bar_size: i32,
        update_tx: async_channel::Sender<()>,
    ) -> Self {
        let container = GtkBox::new(orientation, spacing);
        container.style_context().add_class("taskbar");
        let active_widgets = Rc::new(RefCell::new(HashMap::new()));
        let viewport = Rc::new(RefCell::new(TaskbarViewport::new()));

        container.add_events(gdk::EventMask::SCROLL_MASK);
        let vp_scroll = Rc::clone(&viewport);
        let tx_clone = update_tx;
        let is_vert = orientation == Orientation::Vertical;

        container.connect_scroll_event(move |_, event| {
            let delta = match event.direction() {
                gdk::ScrollDirection::Down | gdk::ScrollDirection::Right => 1,
                gdk::ScrollDirection::Up | gdk::ScrollDirection::Left => -1,
                gdk::ScrollDirection::Smooth => {
                    let (dx, dy) = event.scroll_deltas().unwrap_or((0.0, 0.0));
                    let d = if is_vert {
                        dy
                    } else if dx.abs() > dy.abs() {
                        dx
                    } else {
                        dy
                    };
                    if d > 0.0 {
                        1
                    } else if d < 0.0 {
                        -1
                    } else {
                        0
                    }
                }
                _ => 0,
            };

            if delta != 0 {
                vp_scroll.borrow_mut().scroll_by(delta);
                crate::util::nudge(&tx_clone);
                Propagation::Stop
            } else {
                Propagation::Proceed
            }
        });

        Self {
            container,
            active_widgets,
            orientation,
            bar_position,
            bar_size,
            viewport,
        }
    }

    pub fn reconcile(
        &self,
        current_output: &Option<String>,
        state: &Arc<std::sync::Mutex<AppState>>,
        config: &TaskbarConfig,
        niri_service: &Arc<NiriService>,
        capacity: usize,
    ) {
        if !config.enabled {
            self.container.hide();
            return;
        }
        self.container.show();

        let (windows, workspaces, outputs) = {
            let st = crate::util::lock(state);
            (st.windows.clone(), st.workspaces.clone(), st.outputs.clone())
        };

        let mut filtered_windows =
            filter_windows_for_output(&windows, &workspaces, current_output, config.only_current_workspace);

        log_debug!(
            "taskbar",
            "Bar output {:?}: rendering {} window(s) (total system windows: {})",
            current_output,
            filtered_windows.len(),
            windows.len()
        );

        // Check if there is an incoming cross-monitor drag preview targeting this bar's output
        let mut drag_snapshot = {
            let guard = crate::util::lock(&GLOBAL_DRAG);
            guard.clone()
        };

        let is_vertical = self.orientation == Orientation::Vertical;
        let mut preview_window_id: Option<u64> = None;

        if let Some(ref mut drag) = drag_snapshot {
            if drag.is_dragging {
                if let (Some(ref target_out), Some(ref curr_out)) = (&drag.target_output, current_output) {
                    if target_out == curr_out
                        && drag.source_output.as_ref() != Some(curr_out)
                        && !filtered_windows.iter().any(|w| w.id == drag.window_id)
                    {
                        if let Some(drag_win) = windows.iter().find(|w| w.id == drag.window_id) {
                            let mon_logical = outputs.get(curr_out).and_then(|o| o.logical.clone());

                            let (mon_x, mon_y) = mon_logical.map(|l| (l.x as f64, l.y as f64)).unwrap_or((0.0, 0.0));

                            let rel_mon_x = drag.current_global_x - mon_x;
                            let rel_mon_y = drag.current_global_y - mon_y;

                            let (container_off_x, container_off_y) = if let Some(toplevel) = self.container.toplevel() {
                                self.container.translate_coordinates(&toplevel, 0, 0).unwrap_or((0, 0))
                            } else {
                                (0, 0)
                            };

                            let rel_container_x = rel_mon_x - (container_off_x as f64);
                            let rel_container_y = rel_mon_y - (container_off_y as f64);

                            let existing_children = self.container.children();
                            let non_preview: Vec<_> = existing_children
                                .iter()
                                .filter(|c| !c.style_context().has_class("cross-monitor-preview"))
                                .collect();

                            let mut slot = non_preview.len();
                            for (i, child) in non_preview.iter().enumerate() {
                                let alloc = child.allocation();
                                if is_vertical {
                                    if rel_container_y < (alloc.y() + alloc.height()) as f64 {
                                        slot = i;
                                        break;
                                    }
                                } else {
                                    if rel_container_x < (alloc.x() + alloc.width()) as f64 {
                                        slot = i;
                                        break;
                                    }
                                }
                            }

                            let cur_start = self.viewport.borrow().start;
                            let n = filtered_windows.len();
                            let final_slot = (cur_start + slot).min(n);
                            filtered_windows.insert(final_slot, drag_win.clone());
                            preview_window_id = Some(drag.window_id);
                            log_debug!(
                                "taskbar",
                                "Cross-monitor preview: win={} target={} slot={} on bar output={:?} (source={:?})",
                                drag.window_id,
                                target_out,
                                final_slot,
                                curr_out,
                                drag.source_output
                            );

                            {
                                let mut g = crate::util::lock(&GLOBAL_DRAG);
                                if let Some(ref mut d) = *g {
                                    if d.window_id == drag.window_id {
                                        d.target_slot = final_slot;
                                        d.target_slot_valid = true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let focused_id = filtered_windows.iter().find(|w| w.is_focused).map(|w| w.id);
        let focused_idx = filtered_windows.iter().position(|w| w.is_focused);
        let total_count = filtered_windows.len();
        let (start_idx, end_idx) = {
            let mut vp = self.viewport.borrow_mut();
            vp.update(
                focused_id,
                focused_idx,
                total_count,
                capacity,
                config.scroll_safety_margin,
            );
            vp.visible_range()
        };

        let visible_windows = if start_idx < end_idx && end_idx <= total_count {
            &filtered_windows[start_idx..end_idx]
        } else {
            &[]
        };

        let mut widgets_map = self.active_widgets.borrow_mut();
        let current_ids: HashSet<u64> = visible_windows.iter().map(|w| w.id).collect();

        // 1. Remove widgets for windows that no longer exist or are outside visible window
        widgets_map.retain(|id, w| {
            if !current_ids.contains(id) {
                self.container.remove(&w.button);
                false
            } else {
                true
            }
        });

        // 2. Reconcile remaining / new windows in visible window
        let scale_factor = self.container.scale_factor().max(1);
        for (idx, window) in visible_windows.iter().enumerate() {
            let win_id = window.id;
            let icon_key = window
                .app_id
                .clone()
                .unwrap_or_else(|| "application-x-executable".to_string());
            let title = window.title.clone().unwrap_or_else(|| icon_key.clone());
            let is_focused = window.is_focused;
            let is_ghost_preview = preview_window_id == Some(win_id);

            if let Some(widget) = widgets_map.get_mut(&win_id) {
                // Focus styling
                if widget.is_focused != is_focused {
                    if is_focused {
                        widget.button.style_context().add_class("focused");
                    } else {
                        widget.button.style_context().remove_class("focused");
                    }
                    widget.is_focused = is_focused;
                }

                // Drag state styling
                if is_ghost_preview {
                    widget.button.style_context().add_class("drag-placeholder");
                    widget.button.style_context().add_class("cross-monitor-preview");
                    widget.button.style_context().remove_class("is-dragging");
                } else {
                    widget.button.style_context().remove_class("cross-monitor-preview");
                    let is_locally_dragging = drag_snapshot
                        .as_ref()
                        .map(|d| d.window_id == win_id && d.source_output == *current_output && d.is_dragging)
                        .unwrap_or(false);

                    if is_locally_dragging {
                        widget.button.style_context().add_class("is-dragging");
                        let is_targeting_other = drag_snapshot
                            .as_ref()
                            .map(|d| d.target_output.is_some())
                            .unwrap_or(false);
                        if is_targeting_other {
                            widget.button.style_context().add_class("drag-source-away");
                            widget.button.style_context().remove_class("drag-placeholder");
                            widget.button.set_opacity(0.0);
                        } else {
                            widget.button.style_context().remove_class("drag-source-away");
                            widget.button.style_context().add_class("drag-placeholder");
                            widget.button.set_opacity(1.0);
                        }
                    } else {
                        widget.button.style_context().remove_class("drag-placeholder");
                        widget.button.style_context().remove_class("is-dragging");
                        widget.button.style_context().remove_class("drag-source-away");
                        widget.button.set_opacity(1.0);
                    }
                }

                // Tooltip
                if config.show_tooltips && widget.tooltip != title {
                    widget.button.set_tooltip_text(Some(&title));
                    widget.tooltip = title.clone();
                }

                // Icon image
                if *widget.icon_key.borrow() != icon_key || widget.icon_scale != scale_factor {
                    let img = crate::icon::create_app_image(&icon_key, config.icon_size, scale_factor);
                    widget.button.set_image(Some(&img));
                    *widget.icon_key.borrow_mut() = icon_key.clone();
                    widget.icon_scale = scale_factor;
                }

                let is_locally_dragging = drag_snapshot
                    .as_ref()
                    .map(|d| d.window_id == win_id && d.source_output == *current_output)
                    .unwrap_or(false);

                if !is_locally_dragging {
                    self.container.reorder_child(&widget.button, idx as i32);
                }
            } else {
                // Create new button widget
                let button = Button::new();
                button.style_context().add_class("app-button");
                button.set_halign(gtk::Align::Center);
                button.set_valign(gtk::Align::Center);

                button.add_events(
                    gdk::EventMask::BUTTON_PRESS_MASK
                        | gdk::EventMask::BUTTON_RELEASE_MASK
                        | gdk::EventMask::POINTER_MOTION_MASK
                        | gdk::EventMask::ENTER_NOTIFY_MASK
                        | gdk::EventMask::LEAVE_NOTIFY_MASK,
                );

                button.connect_enter_notify_event(|b, _| {
                    let is_drag_active = crate::util::lock(&GLOBAL_DRAG)
                        .as_ref()
                        .map(|g| g.is_dragging)
                        .unwrap_or(false);
                    if !is_drag_active {
                        if let Some(w) = b.window() {
                            let display = w.display();
                            let pointer_cursor = gdk::Cursor::from_name(&display, "pointer");
                            w.set_cursor(pointer_cursor.as_ref());
                        }
                    }
                    Propagation::Proceed
                });

                button.connect_leave_notify_event(|b, _| {
                    let is_drag_active = crate::util::lock(&GLOBAL_DRAG)
                        .as_ref()
                        .map(|g| g.is_dragging)
                        .unwrap_or(false);
                    if !is_drag_active {
                        if let Some(w) = b.window() {
                            w.set_cursor(None);
                        }
                    }
                    Propagation::Proceed
                });

                let shared_icon_key = Rc::new(RefCell::new(icon_key.clone()));
                let window_id = window.id;

                if is_focused {
                    button.style_context().add_class("focused");
                }

                if is_ghost_preview {
                    button.style_context().add_class("drag-placeholder");
                    button.style_context().add_class("cross-monitor-preview");
                }

                let img = crate::icon::create_app_image(&icon_key, config.icon_size, scale_factor);
                button.set_image(Some(&img));
                button.set_always_show_image(true);

                if config.show_tooltips {
                    button.set_tooltip_text(Some(&title));
                }

                // Drag & Drop tracking
                let current_output_clone = current_output.clone();
                let state_clone_press = Arc::clone(state);
                let state_clone_motion = Arc::clone(state);
                let config_icon_sz = config.icon_size;
                let shared_icon_key_motion = Rc::clone(&shared_icon_key);
                let niri_tx = niri_service.update_tx.clone();
                let bar_pos = self.bar_position;
                let bar_sz = self.bar_size;
                let vp_press = Rc::clone(&self.viewport);

                // Button press
                button.connect_button_press_event(move |b, event| {
                    if event.button() == 2 {
                        gio::spawn_blocking(move || {
                            if let Err(e) = ipc::close_window(window_id) {
                                log_warn!("taskbar", "Close window {window_id} failed: {e}");
                            }
                        });
                        return Propagation::Stop;
                    }

                    if event.button() == 1 {
                        let (root_x, root_y) = event.root();
                        let parent = b.parent().and_then(|p| p.downcast::<GtkBox>().ok());
                        let current_idx = parent
                            .as_ref()
                            .map(|p| {
                                let children = p.children();
                                children.iter().position(|c| c == b).unwrap_or(idx)
                            })
                            .unwrap_or(idx);

                        let cur_start = vp_press.borrow().start;
                        let global_idx = cur_start + current_idx;

                        // Anchor the press in Niri space through the monitor
                        // containing it (see `map_root_to_niri`) — no
                        // widget-offset arithmetic, so GDK↔Niri skew cannot
                        // offset the whole drag.
                        let (source_out, (start_global_x, start_global_y)) = {
                            let st = crate::util::lock(&state_clone_press);
                            let out_name = current_output_clone.clone();
                            let (pt, _) = map_root_to_niri(root_x, root_y, &out_name, &st.outputs, bar_pos, bar_sz);
                            (out_name, pt)
                        };

                        let mut drag = crate::util::lock(&GLOBAL_DRAG);
                        // Single-drag guard: never clobber an active drag
                        // from another button/overlay.
                        if let Some(ref existing) = *drag {
                            if existing.is_dragging && existing.window_id != window_id {
                                return Propagation::Proceed;
                            }
                        }
                        let generation = crate::niri::drag::DRAG_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        *drag = Some(GlobalDragState {
                            window_id,
                            source_output: source_out,
                            source_index: global_idx,
                            start_root_x: root_x,
                            start_root_y: root_y,
                            current_global_x: start_global_x,
                            current_global_y: start_global_y,
                            is_dragging: false,
                            current_target_slot: global_idx,
                            target_output: None,
                            target_slot: global_idx,
                            target_slot_valid: false,
                            generation,
                        });
                    }

                    Propagation::Proceed
                });

                // Motion notify on source button
                let niri_tx_motion = niri_tx.clone();
                let vp_motion = Rc::clone(&self.viewport);

                button.connect_motion_notify_event(move |b, event| {
                    let mut drag_guard = crate::util::lock(&GLOBAL_DRAG);
                    if let Some(ref mut drag) = *drag_guard {
                        if drag.window_id == window_id {
                            let (root_x, root_y) = event.root();
                            let dx = root_x - drag.start_root_x;
                            let dy = root_y - drag.start_root_y;
                            let dist = (dx * dx + dy * dy).sqrt();
                            let threshold = DRAG_THRESHOLD_LOGICAL_PX * f64::from(b.scale_factor().max(1));

                            if dist > threshold {
                                if !drag.is_dragging {
                                    drag.is_dragging = true;
                                    b.style_context().add_class("is-dragging");
                                    b.style_context().add_class("drag-placeholder");

                                    let current_key = shared_icon_key_motion.borrow().clone();
                                    apply_drag_cursor_globally(&current_key, config_icon_sz, Some(b));

                                    // Fullscreen transparent overlays so the
                                    // drag tracks across monitors (see drag.rs).
                                    spawn_monitor_overlays(&state_clone_motion, &niri_tx_motion, window_id, b);
                                }

                                let source_out_snapshot = drag.source_output.clone();
                                let prev_target = drag.target_output.clone();
                                // Drop the guard BEFORE locking state or
                                // touching GDK (fixed lock order).
                                drop(drag_guard);

                                let st_outputs = {
                                    let st = crate::util::lock(&state_clone_motion);
                                    st.outputs.clone()
                                };

                                // Absolute per-event mapping (no accumulated
                                // deltas): skew cannot drift the target.
                                let ((current_global_x, current_global_y), hover) = map_root_to_niri(
                                    root_x,
                                    root_y,
                                    &source_out_snapshot,
                                    &st_outputs,
                                    bar_pos,
                                    bar_sz,
                                );
                                let (is_cross_monitor, new_target) = decide_hover_target(hover, &source_out_snapshot);
                                let target_out_changed = prev_target != new_target;
                                store_drag_position(window_id, current_global_x, current_global_y, new_target);

                                if !is_cross_monitor {
                                    if let Some(parent) = b.parent().and_then(|p| p.downcast::<GtkBox>().ok()) {
                                        let children = parent.children();
                                        let total = children.len();
                                        if total > 1 {
                                            let (evt_x, evt_y) = event.position();
                                            let (rel_x, rel_y) = b
                                                .translate_coordinates(&parent, evt_x as i32, evt_y as i32)
                                                .unwrap_or((0, 0));

                                            let current_pos = children.iter().position(|c| c == b).unwrap_or(0);

                                            let bounds: Vec<ChildBounds> = children
                                                .iter()
                                                .map(|child| {
                                                    let alloc = child.allocation();
                                                    if is_vertical {
                                                        ChildBounds {
                                                            pos: alloc.y(),
                                                            len: alloc.height(),
                                                        }
                                                    } else {
                                                        ChildBounds {
                                                            pos: alloc.x(),
                                                            len: alloc.width(),
                                                        }
                                                    }
                                                })
                                                .collect();
                                            let pointer = if is_vertical { rel_y } else { rel_x };
                                            let target_idx = reorder_target(&bounds, pointer, current_pos);

                                            if target_idx != current_pos {
                                                parent.reorder_child(b, target_idx as i32);
                                            }
                                            {
                                                let cur_start = vp_motion.borrow().start;
                                                let global_target = cur_start + target_idx;
                                                let mut g = crate::util::lock(&GLOBAL_DRAG);
                                                if let Some(ref mut d) = *g {
                                                    if d.window_id == window_id {
                                                        d.current_target_slot = global_target;
                                                        d.target_slot = global_target;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    if target_out_changed {
                                        crate::util::nudge(&niri_tx_motion);
                                    }
                                } else {
                                    crate::util::nudge(&niri_tx_motion);
                                }
                            }
                        }
                    }
                    Propagation::Proceed
                });

                // Button release on source button
                let niri_tx_release = niri_tx.clone();
                let b_release = button.clone();

                button.connect_button_release_event(move |_, event| {
                    if event.button() != 1 {
                        return Propagation::Proceed;
                    }

                    reset_all_cursors_and_overlay(Some(&b_release));
                    // Plain click (no drag threshold crossed) focuses the window.
                    finish_drag(window_id, true);

                    crate::util::nudge(&niri_tx_release);

                    Propagation::Stop
                });

                // Add to container and ensure correct initial index
                self.container.add(&button);
                self.container.reorder_child(&button, idx as i32);
                button.show_all();

                widgets_map.insert(
                    window_id,
                    WindowWidget {
                        button,
                        icon_key: shared_icon_key,
                        icon_scale: scale_factor,
                        is_focused,
                        tooltip: title,
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{calculate_sliding_window, filter_windows_for_output, sort_windows_spatially};
    use crate::niri::model::{WindowInfo, WindowLayout, WorkspaceInfo};

    #[test]
    fn test_taskbar_spatial_sorting() {
        let mut windows = [
            WindowInfo {
                id: 1,
                title: Some("Col 2 Tile 0".to_string()),
                app_id: None,
                workspace_id: Some(1),
                is_focused: false,
                layout: Some(WindowLayout {
                    pos_in_scrolling_layout: Some((2, 0)),
                }),
            },
            WindowInfo {
                id: 2,
                title: Some("Col 0 Tile 1".to_string()),
                app_id: None,
                workspace_id: Some(1),
                is_focused: false,
                layout: Some(WindowLayout {
                    pos_in_scrolling_layout: Some((0, 1)),
                }),
            },
            WindowInfo {
                id: 3,
                title: Some("Col 0 Tile 0".to_string()),
                app_id: None,
                workspace_id: Some(1),
                is_focused: false,
                layout: Some(WindowLayout {
                    pos_in_scrolling_layout: Some((0, 0)),
                }),
            },
            WindowInfo {
                id: 4,
                title: Some("Col 1 Tile 0".to_string()),
                app_id: None,
                workspace_id: Some(1),
                is_focused: false,
                layout: Some(WindowLayout {
                    pos_in_scrolling_layout: Some((1, 0)),
                }),
            },
        ];

        sort_windows_spatially(&mut windows, &[]);

        assert_eq!(windows[0].id, 3); // (0, 0)
        assert_eq!(windows[1].id, 2); // (0, 1)
        assert_eq!(windows[2].id, 4); // (1, 0)
        assert_eq!(windows[3].id, 1); // (2, 0)
    }

    #[test]
    fn test_taskbar_workspace_filtering() {
        let workspaces = [
            WorkspaceInfo {
                id: 1,
                idx: 1,
                output: Some("eDP-1".to_string()),
                is_active: true,
                is_focused: true,
            },
            WorkspaceInfo {
                id: 2,
                idx: 2,
                output: Some("eDP-1".to_string()),
                is_active: false,
                is_focused: false,
            },
            WorkspaceInfo {
                id: 3,
                idx: 1,
                output: Some("DP-1".to_string()),
                is_active: true,
                is_focused: false,
            },
        ];

        let windows = [
            WindowInfo {
                id: 10,
                title: Some("eDP-1 active ws".to_string()),
                app_id: None,
                workspace_id: Some(1),
                is_focused: true,
                layout: None,
            },
            WindowInfo {
                id: 20,
                title: Some("eDP-1 inactive ws".to_string()),
                app_id: None,
                workspace_id: Some(2),
                is_focused: false,
                layout: None,
            },
            WindowInfo {
                id: 30,
                title: Some("DP-1 active ws".to_string()),
                app_id: None,
                workspace_id: Some(3),
                is_focused: false,
                layout: None,
            },
        ];

        let current_output = Some("eDP-1".to_string());

        // Test with only_current_workspace = true (exercises production code)
        let only_current = filter_windows_for_output(&windows, &workspaces, &current_output, true);

        assert_eq!(only_current.len(), 1);
        assert_eq!(only_current[0].id, 10);

        // Test with only_current_workspace = false
        let all_on_output = filter_windows_for_output(&windows, &workspaces, &current_output, false);

        assert_eq!(all_on_output.len(), 2);
        assert_eq!(all_on_output[0].id, 10);
        assert_eq!(all_on_output[1].id, 20);
    }

    #[test]
    fn test_taskbar_groups_by_workspace_then_spatial_order() {
        let workspaces = [
            WorkspaceInfo {
                id: 1,
                idx: 1,
                output: Some("eDP-1".to_string()),
                is_active: true,
                is_focused: true,
            },
            WorkspaceInfo {
                id: 2,
                idx: 2,
                output: Some("eDP-1".to_string()),
                is_active: false,
                is_focused: false,
            },
        ];

        let windows = [
            // App on WS 2, col 0
            WindowInfo {
                id: 10,
                title: Some("WS 2 Col 0".to_string()),
                app_id: None,
                workspace_id: Some(2),
                is_focused: false,
                layout: Some(WindowLayout {
                    pos_in_scrolling_layout: Some((0, 0)),
                }),
            },
            // App on WS 1, col 1
            WindowInfo {
                id: 20,
                title: Some("WS 1 Col 1".to_string()),
                app_id: None,
                workspace_id: Some(1),
                is_focused: false,
                layout: Some(WindowLayout {
                    pos_in_scrolling_layout: Some((1, 0)),
                }),
            },
            // App on WS 1, col 0
            WindowInfo {
                id: 30,
                title: Some("WS 1 Col 0".to_string()),
                app_id: None,
                workspace_id: Some(1),
                is_focused: true,
                layout: Some(WindowLayout {
                    pos_in_scrolling_layout: Some((0, 0)),
                }),
            },
            // App on WS 2, col 1
            WindowInfo {
                id: 40,
                title: Some("WS 2 Col 1".to_string()),
                app_id: None,
                workspace_id: Some(2),
                is_focused: false,
                layout: Some(WindowLayout {
                    pos_in_scrolling_layout: Some((1, 0)),
                }),
            },
        ];

        let current_output = Some("eDP-1".to_string());
        let result = filter_windows_for_output(&windows, &workspaces, &current_output, false);

        let ids: Vec<u64> = result.iter().map(|w| w.id).collect();
        // Must group all WS 1 windows first (ordered col 0 then col 1),
        // followed by all WS 2 windows (ordered col 0 then col 1).
        assert_eq!(ids, vec![30, 20, 10, 40]);
    }

    #[test]
    fn test_taskbar_unresolved_output_shows_nothing() {
        let workspaces = [WorkspaceInfo {
            id: 1,
            idx: 1,
            output: Some("eDP-1".to_string()),
            is_active: true,
            is_focused: true,
        }];
        let windows = [WindowInfo {
            id: 10,
            title: Some("win".to_string()),
            app_id: None,
            workspace_id: Some(1),
            is_focused: true,
            layout: None,
        }];
        // Never leak other monitors' windows when output is unresolved.
        let filtered = filter_windows_for_output(&windows, &workspaces, &None, false);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_sliding_window_empty_or_zero_capacity() {
        assert_eq!(calculate_sliding_window(0, 0, 5, 1, None), (0, 0));
        assert_eq!(calculate_sliding_window(0, 10, 0, 1, None), (0, 0));
    }

    #[test]
    fn test_sliding_window_fits_within_capacity() {
        // 4 windows with capacity 5 -> all visible, start is 0
        assert_eq!(calculate_sliding_window(0, 4, 5, 1, Some(2)), (0, 4));
        assert_eq!(calculate_sliding_window(0, 5, 5, 1, Some(4)), (0, 5));
    }

    #[test]
    fn test_sliding_window_forward_navigation_without_safety() {
        // Capacity 5 full icons, Total 10, Safety 0
        // Window 1 to 5 (indices 0..4) -> no shift, renders 6 items (0..6) with 6th as cut-off icon
        assert_eq!(calculate_sliding_window(0, 10, 5, 0, Some(0)), (0, 6));
        assert_eq!(calculate_sliding_window(0, 10, 5, 0, Some(4)), (0, 6));

        // Window 6 (index 5) is the cut-off icon -> shifts by 1 app (start becomes 1, renders 1..7)
        assert_eq!(calculate_sliding_window(0, 10, 5, 0, Some(5)), (1, 7));

        // Window 7 (index 6) from start 1 -> shifts to start 2 (renders 2..8)
        assert_eq!(calculate_sliding_window(1, 10, 5, 0, Some(6)), (2, 8));
    }

    #[test]
    fn test_sliding_window_forward_navigation_with_safety() {
        // Capacity 5 full icons, Total 10, Safety 1
        // Threshold: start + 5 - 1 - 1 = 3 (4th window, index 3).
        // 5th window (index 4) exceeds threshold 3 -> shifts start to 1! (renders 1..7)
        assert_eq!(calculate_sliding_window(0, 10, 5, 1, Some(3)), (0, 6));
        assert_eq!(calculate_sliding_window(0, 10, 5, 1, Some(4)), (1, 7));
        assert_eq!(calculate_sliding_window(1, 10, 5, 1, Some(5)), (2, 8));
    }

    #[test]
    fn test_sliding_window_backward_hysteresis_and_threshold() {
        // Currently at start 4 (showing indices 4..10)
        // Upper threshold = 4 + 5 - 1 - 1 = 7. Lower threshold = 4 + 1 = 5.
        // Navigating back: index 7, 6, 5 remain in view without shift (hysteresis)
        assert_eq!(calculate_sliding_window(4, 10, 5, 1, Some(7)), (4, 10));
        assert_eq!(calculate_sliding_window(4, 10, 5, 1, Some(6)), (4, 10));
        assert_eq!(calculate_sliding_window(4, 10, 5, 1, Some(5)), (4, 10));

        // When navigating to index 4 (< lower threshold 5) -> shifts back to start 3 (renders 3..9)
        assert_eq!(calculate_sliding_window(4, 10, 5, 1, Some(4)), (3, 9));

        // Moving back to index 0 -> clamped at start 0 (renders 0..6)
        assert_eq!(calculate_sliding_window(3, 10, 5, 1, Some(0)), (0, 6));
    }

    #[test]
    fn test_sliding_window_large_jump() {
        // 100 windows, capacity 10, safety 1. Jump directly from 0 to window 90 (index 89)
        // Upper threshold for start S: S + 10 - 1 - 1 = S + 8.
        // For index 89, S = 89 - 8 = 81.
        // End is (81 + 10 + 1) = 92 (10 full + 1 half icon)
        let (start, end) = calculate_sliding_window(0, 100, 10, 1, Some(89));
        assert_eq!((start, end), (81, 92));
        assert!(89 >= start && 89 < end);

        // Jump to last window 100 (index 99) -> clamped to max_start 90 (renders 90..100)
        let (start, end) = calculate_sliding_window(0, 100, 10, 1, Some(99));
        assert_eq!((start, end), (90, 100));
    }

    #[test]
    fn test_sliding_window_no_focus_keeps_valid_range() {
        // No focused window keeps current start clamped, renders capacity + 1
        assert_eq!(calculate_sliding_window(2, 10, 5, 1, None), (2, 8));
        assert_eq!(calculate_sliding_window(8, 10, 5, 1, None), (5, 10));
    }

    #[test]
    fn test_taskbar_viewport_manual_scroll_retention() {
        use super::TaskbarViewport;
        let mut vp = TaskbarViewport::new();
        // Focus window 1 (id: 101, idx: 0), total 10, capacity 5, safety 1
        vp.update(Some(101), Some(0), 10, 5, 1);
        assert_eq!(vp.start, 0);

        // User manually scrolls down by 2 (towards later windows)
        vp.scroll_by(2);
        assert_eq!(vp.start, 2);

        // Subsequent reconcile with the SAME focused window (id: 101) must NOT snap back!
        vp.update(Some(101), Some(0), 10, 5, 1);
        assert_eq!(
            vp.start, 2,
            "Manual scroll must not snap back when focus has not changed"
        );

        // User scrolls down past max_start (10 - 5 = 5)
        vp.scroll_by(10);
        assert_eq!(vp.start, 5);

        // User scrolls up
        vp.scroll_by(-2);
        assert_eq!(vp.start, 3);
    }

    #[test]
    fn test_taskbar_viewport_focus_change_triggers_auto_scroll() {
        use super::TaskbarViewport;
        let mut vp = TaskbarViewport::new();
        // Initially window 101 at index 0
        vp.update(Some(101), Some(0), 10, 5, 1);
        assert_eq!(vp.start, 0);

        // Focus changes to window 105 (at index 4). Threshold with safety 1 is 0 + 5 - 1 - 1 = 3.
        // Index 4 > 3 -> shifts start to 1.
        vp.update(Some(105), Some(4), 10, 5, 1);
        assert_eq!(vp.start, 1);

        // Focus changes to window 109 (at index 8).
        // Upper threshold for start S: S + 3. Index 8 -> start becomes 8 - 3 = 5.
        vp.update(Some(109), Some(8), 10, 5, 1);
        assert_eq!(vp.start, 5);
    }

    #[test]
    fn test_taskbar_viewport_clamping_on_window_shrink() {
        use super::TaskbarViewport;
        let mut vp = TaskbarViewport::new();
        vp.start = 5;
        // Total shrinks from 10 to 4 windows, capacity 5 -> max_start is 0
        vp.update(Some(101), Some(0), 4, 5, 1);
        assert_eq!(vp.start, 0);
    }

    #[test]
    fn test_taskbar_viewport_visible_range() {
        use super::TaskbarViewport;
        let mut vp = TaskbarViewport::new();
        vp.update(None, None, 0, 5, 1);
        assert_eq!(vp.visible_range(), (0, 0));

        vp.update(None, None, 4, 5, 1);
        assert_eq!(vp.visible_range(), (0, 4));

        vp.update(None, None, 5, 5, 1);
        assert_eq!(vp.visible_range(), (0, 5));

        // 10 windows, capacity 5: renders start..start+capacity+1 (peeking icon)
        vp.update(None, None, 10, 5, 1);
        vp.start = 2;
        assert_eq!(vp.visible_range(), (2, 8));

        // At end: 10 windows, start = 5
        vp.start = 5;
        assert_eq!(vp.visible_range(), (5, 10));
    }
}
