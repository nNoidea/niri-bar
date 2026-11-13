use glib::Propagation;
use gtk::prelude::*;
use gtk::{Box as GtkBox, Button, Orientation};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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
    effective_scale: Rc<Cell<i32>>,
    #[cfg(test)]
    update_tx: async_channel::Sender<()>,
}

fn apply_effective_scale(effective_scale: &Cell<i32>, update_tx: &async_channel::Sender<()>, scale: i32) {
    let clamped = scale.max(1);
    if effective_scale.replace(clamped) != clamped {
        crate::util::nudge(update_tx);
    }
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
    let Some(bar_out) = current_output.as_ref() else {
        // Do not leak or mirror other monitors' windows if output is unresolved
        return Vec::new();
    };
    let active_ws_for_bar = if only_current_workspace {
        active_workspace_per_output(workspaces).get(bar_out).copied()
    } else {
        None
    };

    let mut filtered: Vec<WindowInfo> = windows
        .iter()
        .filter(|w| {
            let Some(ws_id) = w.workspace_id else {
                return false;
            };
            let Some(ws) = workspaces.iter().find(|ws| ws.id == ws_id) else {
                return false;
            };
            if ws.output.as_deref() != Some(bar_out.as_str()) {
                return false;
            }
            if let Some(active_ws_id) = active_ws_for_bar {
                if ws.id != active_ws_id {
                    return false;
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

fn relative_drag_coords(
    global_x: f64,
    global_y: f64,
    logical: Option<&crate::niri::model::LogicalOutput>,
    container_off: (f64, f64),
) -> (f64, f64) {
    let (mon_x, mon_y) = logical.map_or((0.0, 0.0), |l| (f64::from(l.x), f64::from(l.y)));
    (global_x - mon_x - container_off.0, global_y - mon_y - container_off.1)
}

fn container_root_offset(container: &GtkBox) -> (f64, f64) {
    container
        .root()
        .and_then(|r| crate::util::widget_translate_coords(container, &r, 0.0, 0.0))
        .unwrap_or((0.0, 0.0))
}

struct WindowRenderView<'a> {
    window: &'a WindowInfo,
    icon_key: &'a str,
    title: &'a str,
    is_ghost_preview: bool,
    scale_factor: i32,
}

struct DragButtonCtx<'a> {
    state: &'a Arc<std::sync::Mutex<AppState>>,
    drag: &'a Arc<DragController>,
    niri_tx: &'a async_channel::Sender<()>,
    current_output: &'a Option<String>,
    viewport: &'a Rc<RefCell<TaskbarViewport>>,
    bar_position: crate::config::BarPosition,
    bar_size: i32,
    config: &'a TaskbarConfig,
    is_vertical: bool,
    initial_idx: usize,
}

fn apply_existing_widget_state(
    widget: &mut WindowWidget,
    view: &WindowRenderView<'_>,
    drag_snapshot: Option<&GlobalDragState>,
    current_output: &Option<String>,
    config: &TaskbarConfig,
) {
    let win_id = view.window.id;
    let is_focused = view.window.is_focused;
    if widget.is_focused != is_focused {
        if is_focused {
            widget.button.add_css_class("focused");
        } else {
            widget.button.remove_css_class("focused");
        }
        widget.is_focused = is_focused;
    }

    if view.is_ghost_preview {
        widget.button.add_css_class("drag-placeholder");
        widget.button.add_css_class("cross-monitor-preview");
        widget.button.remove_css_class("is-dragging");
    } else {
        widget.button.remove_css_class("cross-monitor-preview");
        let locally_dragging =
            drag_snapshot.is_some_and(|d| d.window_id == win_id && d.source_output == *current_output && d.is_dragging);

        if locally_dragging {
            widget.button.add_css_class("is-dragging");
            let targeting_other = drag_snapshot.is_some_and(|d| d.target_output.is_some());
            if targeting_other {
                widget.button.add_css_class("drag-source-away");
                widget.button.remove_css_class("drag-placeholder");
                widget.button.set_opacity(0.0);
            } else {
                widget.button.remove_css_class("drag-source-away");
                widget.button.add_css_class("drag-placeholder");
                widget.button.set_opacity(1.0);
            }
        } else {
            widget.button.remove_css_class("drag-placeholder");
            widget.button.remove_css_class("is-dragging");
            widget.button.remove_css_class("drag-source-away");
            widget.button.set_opacity(1.0);
            widget.button.set_cursor_from_name(Some("pointer"));
        }
    }

    if config.show_tooltips && widget.tooltip != view.title {
        widget.button.set_tooltip_text(Some(view.title));
        widget.tooltip = view.title.to_string();
    }

    if *widget.icon_key.borrow() != view.icon_key || widget.icon_scale != view.scale_factor {
        let img =
            crate::icon::create_app_image_with_pid(view.icon_key, view.window.pid, config.icon_size, view.scale_factor);
        widget.button.set_child(Some(&img));
        *widget.icon_key.borrow_mut() = view.icon_key.to_string();
        widget.icon_scale = view.scale_factor;
    }
}

fn attach_middle_click_close(button: &Button, window_id: u64) {
    let middle_click = gtk::GestureClick::builder()
        .button(2)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    middle_click.connect_pressed(move |gesture, n_press, _, _| {
        if n_press >= 1 {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            crate::runtime::spawn(async move {
                if let Err(e) = ipc::close_window(window_id).await {
                    log_warn!("taskbar", "Close window {window_id} failed: {e}");
                }
            });
        }
    });
    button.add_controller(middle_click);
}

fn attach_drag_and_click_gesture(
    button: &Button,
    window_id: u64,
    shared_icon_key: &Rc<RefCell<String>>,
    ctx: DragButtonCtx<'_>,
) {
    // Capture-phase GestureDrag grabs the pointer sequence in `connect_drag_begin`
    // so `connect_drag_update` keeps receiving motion after leaving the button.
    let current_output_clone = ctx.current_output.clone();
    let drag_begin_ctrl = Arc::clone(ctx.drag);
    let drag_update_ctrl = Arc::clone(ctx.drag);
    let drag_end_ctrl = Arc::clone(ctx.drag);
    let state_press = Arc::clone(ctx.state);
    let state_motion = Arc::clone(ctx.state);
    let shared_icon_key_motion = Rc::clone(shared_icon_key);
    let vp_press = Rc::clone(ctx.viewport);
    let vp_motion = Rc::clone(ctx.viewport);
    let bar_pos = ctx.bar_position;
    let bar_sz = ctx.bar_size;
    let icon_sz = ctx.config.icon_size;
    let is_vertical = ctx.is_vertical;
    let idx = ctx.initial_idx;

    let drag_gesture = gtk::GestureDrag::builder()
        .button(1)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    let seq_cancelled = Rc::new(Cell::new(false));

    let seq_cancelled_begin = Rc::clone(&seq_cancelled);
    let btn_press_weak = button.downgrade();
    drag_gesture.connect_drag_begin(move |gesture, x, y| {
        seq_cancelled_begin.set(false);
        let Some(b) = btn_press_weak.upgrade() else {
            return;
        };
        gesture.set_state(gtk::EventSequenceState::Claimed);
        let (root_x, root_y) = crate::util::widget_to_toplevel_coords(&b, x, y);
        let parent = b.parent().and_then(|p| p.downcast::<GtkBox>().ok());
        let current_idx = parent
            .as_ref()
            .and_then(|p| {
                let b_widget: &gtk::Widget = b.upcast_ref();
                crate::util::widget_children(p).iter().position(|c| c == b_widget)
            })
            .unwrap_or(idx);

        let global_idx = vp_press.borrow().start + current_idx;
        let out_name = current_output_clone.clone();
        let (start_global_x, start_global_y) = {
            let st = crate::util::lock(&state_press);
            let (pt, _) = map_root_to_niri(root_x, root_y, &out_name, &st.outputs, bar_pos, bar_sz);
            pt
        };

        drag_begin_ctrl.begin_press(
            window_id,
            out_name,
            global_idx,
            (root_x, root_y),
            (start_global_x, start_global_y),
        );
    });

    let seq_cancelled_cancel = Rc::clone(&seq_cancelled);
    drag_gesture.connect_cancel(move |_, _| seq_cancelled_cancel.set(true));

    let niri_tx_motion = ctx.niri_tx.clone();
    let btn_motion_weak = button.downgrade();
    drag_gesture.connect_drag_update(move |gesture, offset_x, offset_y| {
        let Some(b) = btn_motion_weak.upgrade() else {
            return;
        };
        let (start_x, start_y) = gesture.start_point().unwrap_or((0.0, 0.0));
        let (evt_x, evt_y) = (start_x + offset_x, start_y + offset_y);
        let (root_x, root_y) = crate::util::widget_to_toplevel_coords(&b, evt_x, evt_y);

        let Some(step) = drag_update_ctrl.check_motion_step(window_id, root_x, root_y) else {
            return;
        };
        if step.just_started {
            b.add_css_class("is-dragging");
            b.add_css_class("drag-placeholder");
            // Spawn overlays BEFORE cursor so new overlay windows inherit drag cursor.
            spawn_monitor_overlays(&drag_update_ctrl, &state_motion, &niri_tx_motion, window_id, &b);
            let current_key = shared_icon_key_motion.borrow().clone();
            apply_drag_cursor_globally(&current_key, icon_sz, Some(&b));
        }

        let ((current_global_x, current_global_y), hover) = {
            let st = crate::util::lock(&state_motion);
            map_root_to_niri(root_x, root_y, &step.source_output, &st.outputs, bar_pos, bar_sz)
        };
        let (is_cross_monitor, new_target) = decide_hover_target(hover, &step.source_output);
        let target_out_changed = step.prev_target != new_target;
        drag_update_ctrl.store_position(window_id, current_global_x, current_global_y, new_target);

        if !is_cross_monitor {
            if let Some(parent) = b.parent().and_then(|p| p.downcast::<GtkBox>().ok()) {
                let rel_coords = crate::util::widget_translate_coords(&b, &parent, evt_x, evt_y).unwrap_or((0.0, 0.0));
                update_local_drag_reorder(
                    &parent,
                    &b,
                    rel_coords,
                    is_vertical,
                    vp_motion.borrow().start,
                    window_id,
                    &drag_update_ctrl,
                );
            }
            if target_out_changed {
                crate::util::nudge(&niri_tx_motion);
            }
        } else {
            crate::util::nudge(&niri_tx_motion);
        }
    });

    let seq_cancelled_end = Rc::clone(&seq_cancelled);
    let niri_tx_release = ctx.niri_tx.clone();
    let btn_end_weak = button.downgrade();
    drag_gesture.connect_drag_end(move |_, _, _| {
        let b_release = btn_end_weak.upgrade();
        drag_end_ctrl.handle_source_gesture_end(
            window_id,
            seq_cancelled_end.get(),
            b_release.as_ref(),
            &niri_tx_release,
        );
    });
    button.add_controller(drag_gesture);
}

fn create_window_button(view: &WindowRenderView<'_>, ctx: DragButtonCtx<'_>) -> (Button, Rc<RefCell<String>>) {
    let window_id = view.window.id;
    let button = Button::new();
    button.add_css_class("app-button");
    button.set_has_frame(false);
    button.set_halign(gtk::Align::Center);
    button.set_valign(gtk::Align::Center);
    button.set_focusable(false);
    button.set_cursor_from_name(Some("pointer"));

    let shared_icon_key = Rc::new(RefCell::new(view.icon_key.to_string()));
    if view.window.is_focused {
        button.add_css_class("focused");
    }
    if view.is_ghost_preview {
        button.add_css_class("drag-placeholder");
        button.add_css_class("cross-monitor-preview");
    }

    let img =
        crate::icon::create_app_image_with_pid(view.icon_key, view.window.pid, ctx.config.icon_size, view.scale_factor);
    button.set_child(Some(&img));

    if ctx.config.show_tooltips {
        button.set_tooltip_text(Some(view.title));
    }

    attach_middle_click_close(&button, window_id);
    attach_drag_and_click_gesture(&button, window_id, &shared_icon_key, ctx);

    (button, shared_icon_key)
}

fn snapshot_windows_for_reconcile(
    state: &Arc<std::sync::Mutex<AppState>>,
    current_output: &Option<String>,
    only_current_workspace: bool,
    drag_want_id: Option<u64>,
) -> (
    Vec<WindowInfo>,
    usize,
    Option<crate::niri::model::LogicalOutput>,
    Option<WindowInfo>,
) {
    let st = crate::util::lock(state);
    let filtered = filter_windows_for_output(&st.windows, &st.workspaces, current_output, only_current_workspace);
    let logical = current_output
        .as_ref()
        .and_then(|o| st.outputs.get(o))
        .and_then(|i| i.logical.clone());
    let drag_win = drag_want_id
        .filter(|id| !filtered.iter().any(|w| w.id == *id))
        .and_then(|id| st.windows.iter().find(|w| w.id == id).cloned());
    (filtered, st.windows.len(), logical, drag_win)
}

struct ReconcileCtx<'a> {
    current_output: &'a Option<String>,
    state: &'a Arc<std::sync::Mutex<AppState>>,
    config: &'a TaskbarConfig,
    niri_service: &'a Arc<NiriService>,
    drag_snapshot: Option<&'a GlobalDragState>,
    curr_logical: Option<&'a crate::niri::model::LogicalOutput>,
}

impl TaskbarWidget {
    pub fn new(
        orientation: Orientation,
        spacing: i32,
        bar_position: crate::config::BarPosition,
        bar_size: i32,
        update_tx: async_channel::Sender<()>,
        initial_scale: i32,
    ) -> Self {
        let container = GtkBox::new(orientation, spacing);
        container.add_css_class("taskbar");
        let active_widgets = Rc::new(RefCell::new(HashMap::new()));
        let viewport = Rc::new(RefCell::new(TaskbarViewport::new()));

        let vp_scroll = Rc::clone(&viewport);
        let tx_clone = update_tx.clone();
        let is_vert = orientation == Orientation::Vertical;

        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::BOTH_AXES | gtk::EventControllerScrollFlags::DISCRETE,
        );
        scroll.connect_scroll(move |_, dx, dy| {
            let d = if is_vert {
                dy
            } else if dx.abs() > dy.abs() {
                dx
            } else {
                dy
            };
            let delta = if d > 0.0 {
                1
            } else if d < 0.0 {
                -1
            } else {
                0
            };

            if delta != 0 {
                vp_scroll.borrow_mut().scroll_by(delta);
                crate::util::nudge(&tx_clone);
                Propagation::Stop
            } else {
                Propagation::Proceed
            }
        });
        container.add_controller(scroll);

        let effective_scale = Rc::new(Cell::new(initial_scale.max(1)));
        let scale_cell = Rc::clone(&effective_scale);
        #[cfg(test)]
        let tx_scale = update_tx.clone();
        #[cfg(not(test))]
        let tx_scale = update_tx;
        container.connect_scale_factor_notify(move |c| {
            apply_effective_scale(&scale_cell, &tx_scale, c.scale_factor());
        });

        Self {
            container,
            active_widgets,
            orientation,
            bar_position,
            bar_size,
            viewport,
            effective_scale,
            #[cfg(test)]
            update_tx,
        }
    }

    /// Update the active HiDPI scale factor and nudge the reconcile loop when changed.
    #[cfg(test)]
    pub fn set_scale_factor(&self, scale: i32) {
        apply_effective_scale(&self.effective_scale, &self.update_tx, scale);
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
            self.container.set_visible(false);
            return;
        }
        self.container.set_visible(true);

        // Fixed lock order: snapshot DragController first, then inspect AppState.
        let drag_snapshot = niri_service.drag.snapshot();
        let drag_want_id = drag_snapshot.as_ref().and_then(|d| {
            let matches_output = d
                .target_output
                .as_ref()
                .zip(current_output.as_ref())
                .is_some_and(|(t, c)| t == c);
            (d.is_dragging && matches_output && d.source_output.as_ref() != current_output.as_ref())
                .then_some(d.window_id)
        });

        let (mut filtered_windows, total_sys_windows, curr_logical, preview_drag_win) =
            snapshot_windows_for_reconcile(state, current_output, config.only_current_workspace, drag_want_id);

        log_debug!(
            "taskbar",
            "Bar output {:?}: rendering {} window(s) (total system windows: {})",
            current_output,
            filtered_windows.len(),
            total_sys_windows
        );

        let ctx = ReconcileCtx {
            current_output,
            state,
            config,
            niri_service,
            drag_snapshot: drag_snapshot.as_ref(),
            curr_logical: curr_logical.as_ref(),
        };
        let preview_window_id = match (ctx.drag_snapshot, preview_drag_win) {
            (Some(drag), Some(drag_win)) => {
                self.apply_cross_monitor_preview(&ctx, drag, drag_win, &mut filtered_windows)
            }
            _ => None,
        };

        let (start_idx, visible_len) = {
            let focused = filtered_windows.iter().enumerate().find(|(_, w)| w.is_focused);
            let mut vp = self.viewport.borrow_mut();
            vp.update(
                focused.map(|(_, w)| w.id),
                focused.map(|(i, _)| i),
                filtered_windows.len(),
                capacity,
                config.scroll_safety_margin,
            );
            vp.visible_range()
        };
        let visible_windows = filtered_windows.get(start_idx..visible_len).unwrap_or(&[]);

        self.sync_visible_widgets(&ctx, visible_windows, preview_window_id, start_idx);
    }

    fn apply_cross_monitor_preview(
        &self,
        ctx: &ReconcileCtx<'_>,
        drag: &GlobalDragState,
        drag_win: WindowInfo,
        filtered_windows: &mut Vec<WindowInfo>,
    ) -> Option<u64> {
        let is_vertical = self.orientation == Orientation::Vertical;
        let (rel_x, rel_y) = relative_drag_coords(
            drag.current_global_x,
            drag.current_global_y,
            ctx.curr_logical,
            container_root_offset(&self.container),
        );
        let mut slot = None;
        let mut non_preview_idx = 0usize;
        for child in crate::util::widget_children(&self.container) {
            if child.has_css_class("cross-monitor-preview") {
                continue;
            }
            let (ax, ay, aw, ah) = crate::util::widget_bounds_in(&child, &self.container).unwrap_or((0, 0, 0, 0));
            let before = if is_vertical {
                rel_y < (ay + ah) as f64
            } else {
                rel_x < (ax + aw) as f64
            };
            if before {
                slot = Some(non_preview_idx);
                break;
            }
            non_preview_idx += 1;
        }
        let cur_start = self.viewport.borrow().start;
        let final_slot = (cur_start + slot.unwrap_or(non_preview_idx)).min(filtered_windows.len());
        filtered_windows.insert(final_slot, drag_win);
        log_debug!(
            "taskbar",
            "Cross-monitor preview: win={} target={:?} slot={} on bar output={:?} (source={:?})",
            drag.window_id,
            drag.target_output,
            final_slot,
            ctx.current_output,
            drag.source_output
        );
        ctx.niri_service
            .drag
            .set_cross_monitor_preview_slot(drag.window_id, final_slot);
        Some(drag.window_id)
    }

    fn sync_visible_widgets(
        &self,
        ctx: &ReconcileCtx<'_>,
        visible_windows: &[WindowInfo],
        preview_window_id: Option<u64>,
        start_idx: usize,
    ) {
        let mut widgets_map = self.active_widgets.borrow_mut();
        widgets_map.retain(|id, w| {
            let keep = visible_windows.iter().any(|vw| vw.id == *id);
            if !keep {
                self.container.remove(&w.button);
            }
            keep
        });

        let is_vertical = self.orientation == Orientation::Vertical;
        let any_locally_dragging = ctx
            .drag_snapshot
            .is_some_and(|d| d.is_dragging && d.source_output == *ctx.current_output && d.target_output.is_none());
        let scale_factor = self.effective_scale.get().max(1);

        for (idx, window) in visible_windows.iter().enumerate() {
            let win_id = window.id;
            let icon_key = window.app_id.as_deref().unwrap_or("application-x-executable");
            let title = window.title.as_deref().unwrap_or(icon_key);
            let view = WindowRenderView {
                window,
                icon_key,
                title,
                is_ghost_preview: preview_window_id == Some(win_id),
                scale_factor,
            };

            if let Some(widget) = widgets_map.get_mut(&win_id) {
                apply_existing_widget_state(widget, &view, ctx.drag_snapshot, ctx.current_output, ctx.config);
                if !any_locally_dragging {
                    crate::util::reorder_box_child(&self.container, &widget.button, idx);
                }
            } else {
                let (button, shared_icon_key) = create_window_button(
                    &view,
                    DragButtonCtx {
                        state: ctx.state,
                        drag: &ctx.niri_service.drag,
                        niri_tx: &ctx.niri_service.update_tx,
                        current_output: ctx.current_output,
                        viewport: &self.viewport,
                        bar_position: self.bar_position,
                        bar_size: self.bar_size,
                        config: ctx.config,
                        is_vertical,
                        initial_idx: idx,
                    },
                );
                self.container.append(&button);
                crate::util::reorder_box_child(&self.container, &button, idx);
                widgets_map.insert(
                    win_id,
                    WindowWidget {
                        button,
                        icon_key: shared_icon_key,
                        icon_scale: scale_factor,
                        is_focused: window.is_focused,
                        tooltip: title.to_string(),
                    },
                );
            }
        }

        if any_locally_dragging {
            if let Some(drag) = ctx.drag_snapshot {
                if let Some(drag_widget) = widgets_map.get(&drag.window_id) {
                    let rel_coords = relative_drag_coords(
                        drag.current_global_x,
                        drag.current_global_y,
                        ctx.curr_logical,
                        container_root_offset(&self.container),
                    );
                    update_local_drag_reorder(
                        &self.container,
                        &drag_widget.button,
                        rel_coords,
                        is_vertical,
                        start_idx,
                        drag.window_id,
                        &ctx.niri_service.drag,
                    );
                }
            }
        }
    }
}

fn update_local_drag_reorder(
    parent: &GtkBox,
    b: &Button,
    rel_coords: (f64, f64),
    is_vertical: bool,
    viewport_start: usize,
    window_id: u64,
    drag: &DragController,
) {
    let (rel_x, rel_y) = rel_coords;
    let children = crate::util::widget_children(parent);
    if children.len() <= 1 {
        return;
    }

    let b_widget: &gtk::Widget = b.upcast_ref();
    let current_pos = children.iter().position(|c| c == b_widget).unwrap_or(0);

    let bounds: Vec<ChildBounds> = children
        .iter()
        .map(|child| {
            let (ax, ay, aw, ah) = crate::util::widget_bounds_in(child, parent).unwrap_or((0, 0, 0, 0));
            if is_vertical {
                ChildBounds { pos: ay, len: ah }
            } else {
                ChildBounds { pos: ax, len: aw }
            }
        })
        .collect();

    if !bounds.iter().any(|bd| bd.len > 0) {
        return;
    }

    let pointer = if is_vertical { rel_y as i32 } else { rel_x as i32 };
    let target_idx = reorder_target(&bounds, pointer, current_pos);

    if target_idx != current_pos {
        crate::util::reorder_box_child(parent, b, target_idx);
    }

    drag.set_current_slot(window_id, viewport_start + target_idx);
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
                pid: None,
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
                pid: None,
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
                pid: None,
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
                pid: None,
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
                pid: None,
                workspace_id: Some(1),
                is_focused: true,
                layout: None,
            },
            WindowInfo {
                id: 20,
                title: Some("eDP-1 inactive ws".to_string()),
                app_id: None,
                pid: None,
                workspace_id: Some(2),
                is_focused: false,
                layout: None,
            },
            WindowInfo {
                id: 30,
                title: Some("DP-1 active ws".to_string()),
                app_id: None,
                pid: None,
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
                pid: None,
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
                pid: None,
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
                pid: None,
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
                pid: None,
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
            pid: None,
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

    #[test]
    fn test_taskbar_button_click_gestures_use_capture_phase() {
        crate::util::run_gtk_test(|| {
            use crate::config::TaskbarConfig;
            use crate::niri::{AppState, NiriService, WindowInfo, WorkspaceInfo};
            use gtk::prelude::*;

            let niri = std::sync::Arc::new(NiriService::new_mock());
            let tb = super::TaskbarWidget::new(
                gtk::Orientation::Vertical,
                4,
                crate::config::BarPosition::Left,
                48,
                niri.update_tx.clone(),
                1,
            );
            {
                let mut st = AppState::default();
                st.workspaces.push(WorkspaceInfo {
                    id: 1,
                    idx: 1,
                    output: Some("eDP-1".into()),
                    is_active: true,
                    is_focused: true,
                });
                st.windows.push(WindowInfo {
                    id: 99,
                    title: Some("Firefox".into()),
                    app_id: Some("firefox".into()),
                    workspace_id: Some(1),
                    is_focused: true,
                    layout: Default::default(),
                    pid: None,
                });
                *crate::util::lock(&niri.state) = st;
            }
            tb.reconcile(&Some("eDP-1".into()), &niri.state, &TaskbarConfig::default(), &niri, 10);

            let children = crate::util::widget_children(&tb.container);
            assert_eq!(children.len(), 1);
            let btn = children[0].clone().downcast::<gtk::Button>().unwrap();
            assert!(
                !btn.is_focusable(),
                "Taskbar app button should not be focusable to avoid GTK4 focus rings"
            );

            let controllers = btn.observe_controllers();
            let mut has_capture_drag = false;
            let mut has_capture_middle = false;
            for i in 0..controllers.n_items() {
                if let Some(ctrl) = controllers.item(i).and_then(|o| o.downcast::<gtk::GestureDrag>().ok()) {
                    if ctrl.button() == 1 {
                        assert_eq!(
                            ctrl.propagation_phase(),
                            gtk::PropagationPhase::Capture,
                            "GestureDrag on gtk::Button must use Capture phase"
                        );
                        has_capture_drag = true;
                    }
                }
                if let Some(ctrl) = controllers.item(i).and_then(|o| o.downcast::<gtk::GestureClick>().ok()) {
                    if ctrl.button() == 2 {
                        assert_eq!(
                            ctrl.propagation_phase(),
                            gtk::PropagationPhase::Capture,
                            "Middle-click GestureClick on gtk::Button must use Capture phase"
                        );
                        has_capture_middle = true;
                    }
                }
                if controllers
                    .item(i)
                    .and_then(|o| o.downcast::<gtk::EventControllerMotion>().ok())
                    .is_some()
                {
                    panic!("Taskbar app button must NOT have EventControllerMotion attached (intercepts hover and breaks drag tracking past widget allocation in GTK4)");
                }
            }
            assert!(has_capture_drag, "Expected Capture-phase GestureDrag for button 1");
            assert!(
                has_capture_middle,
                "Expected Capture-phase GestureClick for middle button"
            );
        });
    }

    #[test]
    fn test_reconcile_during_local_drag_preserves_reordered_box_position() {
        crate::util::run_gtk_test(|| {
            use crate::config::TaskbarConfig;
            use crate::niri::drag::GlobalDragState;
            use crate::niri::{AppState, NiriService, WindowInfo, WorkspaceInfo};
            use gtk::prelude::*;

            let niri = std::sync::Arc::new(NiriService::new_mock());
            let tb = super::TaskbarWidget::new(
                gtk::Orientation::Vertical,
                4,
                crate::config::BarPosition::Left,
                48,
                niri.update_tx.clone(),
                1,
            );
            {
                let mut st = AppState::default();
                st.workspaces.push(WorkspaceInfo {
                    id: 1,
                    idx: 1,
                    output: Some("eDP-1".into()),
                    is_active: true,
                    is_focused: true,
                });
                for (id, col) in [(10u64, 1usize), (20u64, 2usize), (30u64, 3usize)] {
                    st.windows.push(WindowInfo {
                        id,
                        title: Some(format!("Win {id}")),
                        app_id: Some("firefox".into()),
                        workspace_id: Some(1),
                        is_focused: id == 10,
                        layout: Some(crate::niri::model::WindowLayout {
                            pos_in_scrolling_layout: Some((col, 1)),
                        }),
                        pid: None,
                    });
                }
                *crate::util::lock(&niri.state) = st;
            }
            tb.reconcile(&Some("eDP-1".into()), &niri.state, &TaskbarConfig::default(), &niri, 10);

            let initial_children = crate::util::widget_children(&tb.container);
            assert_eq!(initial_children.len(), 3);
            let btn_10 = initial_children[0].clone().downcast::<gtk::Button>().unwrap();

            // Simulate dragging window 10 to slot 2 (index 2)
            crate::util::reorder_box_child(&tb.container, &btn_10, 2);
            niri.drag.set_for_test(Some(GlobalDragState {
                window_id: 10,
                source_output: Some("eDP-1".into()),
                source_index: 0,
                start_root_x: 10.0,
                start_root_y: 10.0,
                current_global_x: 10.0,
                current_global_y: 120.0,
                is_dragging: true,
                current_target_slot: 2,
                target_output: None,
                target_slot: 0,
                target_slot_valid: false,
                generation: 1,
            }));

            // Reconcile while dragging locally must NOT snap btn_10 back to index 0
            tb.reconcile(&Some("eDP-1".into()), &niri.state, &TaskbarConfig::default(), &niri, 10);

            let after_children = crate::util::widget_children(&tb.container);
            assert_eq!(
                &after_children[2],
                btn_10.upcast_ref::<gtk::Widget>(),
                "Reconcile during active local drag must not snap dragged button back to index 0"
            );

            niri.drag.set_for_test(None);
        });
    }

    #[test]
    fn test_cross_monitor_drag_preview_reconcile_and_cleanup() {
        crate::util::run_gtk_test(|| {
            use crate::config::TaskbarConfig;
            use crate::niri::drag::GlobalDragState;
            use crate::niri::model::{LogicalOutput, OutputInfo};
            use crate::niri::{AppState, NiriService, WindowInfo, WorkspaceInfo};
            use gtk::prelude::*;
            use std::collections::HashMap;

            let niri = std::sync::Arc::new(NiriService::new_mock());
            let tb_source = super::TaskbarWidget::new(
                gtk::Orientation::Vertical,
                4,
                crate::config::BarPosition::Left,
                48,
                niri.update_tx.clone(),
                1,
            );
            let tb_target = super::TaskbarWidget::new(
                gtk::Orientation::Vertical,
                4,
                crate::config::BarPosition::Left,
                48,
                niri.update_tx.clone(),
                1,
            );

            let mut outputs = HashMap::new();
            outputs.insert(
                "eDP-1".to_string(),
                OutputInfo {
                    name: Some("eDP-1".to_string()),
                    make: None,
                    model: None,
                    physical_size: None,
                    logical: Some(LogicalOutput {
                        x: 0,
                        y: 0,
                        width: 1422,
                        height: 800,
                        scale: Some(1.0),
                    }),
                },
            );
            outputs.insert(
                "DP-1".to_string(),
                OutputInfo {
                    name: Some("DP-1".to_string()),
                    make: None,
                    model: None,
                    physical_size: None,
                    logical: Some(LogicalOutput {
                        x: 1423,
                        y: 0,
                        width: 1920,
                        height: 1080,
                        scale: Some(1.0),
                    }),
                },
            );

            {
                let mut st = AppState {
                    outputs,
                    ..Default::default()
                };
                st.workspaces.push(WorkspaceInfo {
                    id: 1,
                    idx: 1,
                    output: Some("eDP-1".into()),
                    is_active: true,
                    is_focused: true,
                });
                st.workspaces.push(WorkspaceInfo {
                    id: 2,
                    idx: 2,
                    output: Some("DP-1".into()),
                    is_active: true,
                    is_focused: false,
                });
                st.windows.push(WindowInfo {
                    id: 10,
                    title: Some("Firefox".into()),
                    app_id: Some("firefox".into()),
                    workspace_id: Some(1),
                    is_focused: true,
                    layout: Some(crate::niri::model::WindowLayout {
                        pos_in_scrolling_layout: Some((1, 1)),
                    }),
                    pid: None,
                });
                st.windows.push(WindowInfo {
                    id: 20,
                    title: Some("Terminal".into()),
                    app_id: Some("firefox".into()),
                    workspace_id: Some(2),
                    is_focused: false,
                    layout: Some(crate::niri::model::WindowLayout {
                        pos_in_scrolling_layout: Some((1, 1)),
                    }),
                    pid: None,
                });
                *crate::util::lock(&niri.state) = st;
            }

            let cfg = TaskbarConfig::default();

            // Initial reconcile: source bar has win 10, target bar has win 20
            tb_source.reconcile(&Some("eDP-1".into()), &niri.state, &cfg, &niri, 10);
            tb_target.reconcile(&Some("DP-1".into()), &niri.state, &cfg, &niri, 10);

            assert_eq!(crate::util::widget_children(&tb_source.container).len(), 1);
            assert_eq!(crate::util::widget_children(&tb_target.container).len(), 1);

            // 1. Simulate cross-monitor drag of win 10 targeting DP-1
            niri.drag.set_for_test(Some(GlobalDragState {
                window_id: 10,
                source_output: Some("eDP-1".into()),
                source_index: 0,
                start_root_x: 20.0,
                start_root_y: 20.0,
                current_global_x: 1500.0,
                current_global_y: 50.0,
                is_dragging: true,
                current_target_slot: 0,
                target_output: Some("DP-1".into()),
                target_slot: 0,
                target_slot_valid: false,
                generation: 1,
            }));

            // Target bar reconciles: must insert ghost preview button
            tb_target.reconcile(&Some("DP-1".into()), &niri.state, &cfg, &niri, 10);
            let target_children = crate::util::widget_children(&tb_target.container);
            assert_eq!(
                target_children.len(),
                2,
                "Target bar must render original window + ghost preview button"
            );

            let preview_btn = target_children
                .iter()
                .find(|c| c.has_css_class("cross-monitor-preview"))
                .expect("Ghost preview button must have .cross-monitor-preview class");
            assert!(
                preview_btn.has_css_class("drag-placeholder"),
                "Ghost preview button must have .drag-placeholder class"
            );

            // DragController target_slot_valid must now be true
            {
                let snap = niri.drag.snapshot();
                let d = snap.as_ref().unwrap();
                assert!(d.target_slot_valid, "Drag target slot must be marked valid");
            }

            // Source bar reconciles: win 10 button must become drag-source-away with opacity 0.0
            tb_source.reconcile(&Some("eDP-1".into()), &niri.state, &cfg, &niri, 10);
            let source_children = crate::util::widget_children(&tb_source.container);
            assert_eq!(source_children.len(), 1);
            assert!(
                source_children[0].has_css_class("drag-source-away"),
                "Source button must have .drag-source-away class while targeting another monitor"
            );
            assert_eq!(
                source_children[0].opacity(),
                0.0,
                "Source button must have 0.0 opacity while targeting another monitor"
            );

            // 2. Drag ends (dropped or cancelled): clean up
            niri.drag.set_for_test(None);

            // Target bar reconciles again: ghost preview button must be removed
            tb_target.reconcile(&Some("DP-1".into()), &niri.state, &cfg, &niri, 10);
            let target_clean_children = crate::util::widget_children(&tb_target.container);
            assert_eq!(
                target_clean_children.len(),
                1,
                "Target bar must remove ghost preview button after drag ends"
            );
            assert!(
                !target_clean_children[0].has_css_class("cross-monitor-preview"),
                "Remaining button on target bar must not have .cross-monitor-preview"
            );

            // Source bar reconciles again: source button restores opacity and removes drag-source-away
            tb_source.reconcile(&Some("eDP-1".into()), &niri.state, &cfg, &niri, 10);
            let source_clean_children = crate::util::widget_children(&tb_source.container);
            assert_eq!(source_clean_children.len(), 1);
            assert!(
                !source_clean_children[0].has_css_class("drag-source-away"),
                "Source button must remove .drag-source-away after drag ends"
            );
            assert_eq!(
                source_clean_children[0].opacity(),
                1.0,
                "Source button opacity must be restored to 1.0"
            );
        });
    }

    #[test]
    fn test_removed_taskbar_button_destroys_without_cycle() {
        crate::util::run_gtk_test(|| {
            use crate::config::TaskbarConfig;
            use crate::niri::{AppState, NiriService, WindowInfo, WorkspaceInfo};
            use gtk::prelude::*;
            use std::cell::Cell;
            use std::rc::Rc;

            let niri = std::sync::Arc::new(NiriService::new_mock());
            let tb = super::TaskbarWidget::new(
                gtk::Orientation::Vertical,
                4,
                crate::config::BarPosition::Left,
                48,
                niri.update_tx.clone(),
                1,
            );
            let win = gtk::Window::new();
            win.set_child(Some(&tb.container));
            win.set_visible(true);

            {
                let mut st = AppState::default();
                st.workspaces.push(WorkspaceInfo {
                    id: 1,
                    idx: 1,
                    output: Some("OUT-1".into()),
                    is_active: true,
                    is_focused: true,
                });
                st.windows.push(WindowInfo {
                    id: 501,
                    title: Some("Example App".into()),
                    app_id: Some("org.example.App".into()),
                    workspace_id: Some(1),
                    is_focused: true,
                    layout: Default::default(),
                    pid: None,
                });
                *crate::util::lock(&niri.state) = st;
            }
            tb.reconcile(&Some("OUT-1".into()), &niri.state, &TaskbarConfig::default(), &niri, 10);

            let children = crate::util::widget_children(&tb.container);
            assert_eq!(children.len(), 1);
            let destroyed = Rc::new(Cell::new(false));
            let d_flag = Rc::clone(&destroyed);
            children[0].connect_destroy(move |_| d_flag.set(true));
            drop(children);

            // Window closes: reconcile removes the button
            crate::util::lock(&niri.state).windows.clear();
            tb.reconcile(&Some("OUT-1".into()), &niri.state, &TaskbarConfig::default(), &niri, 10);
            assert!(crate::util::widget_children(&tb.container).is_empty());

            let ctx = glib::MainContext::default();
            for _ in 0..10 {
                while ctx.pending() {
                    ctx.iteration(false);
                }
            }

            assert!(
                destroyed.get(),
                "Removed taskbar button must emit destroy instead of leaking via GestureDrag cycle"
            );
            win.close();
        });
    }

    #[test]
    fn test_taskbar_uses_initial_scale_and_updates_on_scale_change() {
        crate::util::run_gtk_test(|| {
            use crate::config::TaskbarConfig;
            use crate::niri::{AppState, NiriService, WindowInfo, WorkspaceInfo};
            use gtk::prelude::*;

            let niri = std::sync::Arc::new(NiriService::new_mock());
            {
                let mut st = AppState::default();
                st.workspaces.push(WorkspaceInfo {
                    id: 1,
                    idx: 1,
                    output: Some("OUT-1".into()),
                    is_active: true,
                    is_focused: true,
                });
                st.windows.push(WindowInfo {
                    id: 701,
                    title: Some("Scale App".into()),
                    app_id: Some("org.example.ScaleApp".into()),
                    workspace_id: Some(1),
                    is_focused: true,
                    layout: Default::default(),
                    pid: None,
                });
                *crate::util::lock(&niri.state) = st;
            }
            let cfg = TaskbarConfig {
                icon_size: 20,
                ..TaskbarConfig::default()
            };

            let (tx_1x, rx_1x) = async_channel::unbounded();
            let tb_1x = super::TaskbarWidget::new(
                gtk::Orientation::Vertical,
                4,
                crate::config::BarPosition::Left,
                48,
                tx_1x,
                1,
            );
            tb_1x.reconcile(&Some("OUT-1".into()), &niri.state, &cfg, &niri, 10);

            assert_eq!(tb_1x.active_widgets.borrow().get(&701).unwrap().icon_scale, 1);
            let btn_1x = crate::util::widget_children(&tb_1x.container)
                .into_iter()
                .next()
                .unwrap()
                .downcast::<gtk::Button>()
                .unwrap();
            let img_1x = btn_1x.child().unwrap().downcast::<gtk::Image>().unwrap();
            assert_eq!(img_1x.pixel_size(), 20);
            assert_eq!(
                img_1x.paintable().unwrap().intrinsic_width(),
                20,
                "TaskbarWidget with initial scale 1 must render 20px icon buffer"
            );

            let (tx_2x, _rx_2x) = async_channel::unbounded();
            let tb_2x = super::TaskbarWidget::new(
                gtk::Orientation::Vertical,
                4,
                crate::config::BarPosition::Left,
                48,
                tx_2x,
                2,
            );
            tb_2x.reconcile(&Some("OUT-1".into()), &niri.state, &cfg, &niri, 10);

            assert_eq!(tb_2x.active_widgets.borrow().get(&701).unwrap().icon_scale, 2);
            let btn_2x = crate::util::widget_children(&tb_2x.container)
                .into_iter()
                .next()
                .unwrap()
                .downcast::<gtk::Button>()
                .unwrap();
            let img_2x = btn_2x.child().unwrap().downcast::<gtk::Image>().unwrap();
            assert_eq!(img_2x.pixel_size(), 20);
            assert_eq!(
                img_2x.paintable().unwrap().intrinsic_width(),
                40,
                "TaskbarWidget with initial scale 2 must render 40px icon buffer"
            );

            // Dynamic scale transition 1 -> 2 must wake reconcile loop and rescale existing button icon
            while rx_1x.try_recv().is_ok() {}
            tb_1x.set_scale_factor(2);
            assert!(
                rx_1x.try_recv().is_ok(),
                "Changing taskbar scale factor must wake bar reconcile loop"
            );

            tb_1x.reconcile(&Some("OUT-1".into()), &niri.state, &cfg, &niri, 10);
            assert_eq!(tb_1x.active_widgets.borrow().get(&701).unwrap().icon_scale, 2);
            let img_after = btn_1x.child().unwrap().downcast::<gtk::Image>().unwrap();
            assert_eq!(img_after.pixel_size(), 20);
            assert_eq!(
                img_after.paintable().unwrap().intrinsic_width(),
                40,
                "Reconcile after scale change 1 -> 2 must replace existing button icon with 40px buffer"
            );
        });
    }

    #[test]
    fn test_relative_drag_coords_subtracts_monitor_and_container_offsets() {
        let logical = crate::niri::model::LogicalOutput {
            x: 1920,
            y: 100,
            width: 2560,
            height: 1440,
            scale: Some(1.0),
        };
        let (rx, ry) = super::relative_drag_coords(2050.0, 220.0, Some(&logical), (30.0, 10.0));
        assert_eq!((rx, ry), (100.0, 110.0));
        assert_eq!(super::relative_drag_coords(50.0, 60.0, None, (5.0, 10.0)), (45.0, 50.0));
    }
}
