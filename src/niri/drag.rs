use crate::niri::model::OutputInfo;
use gtk::prelude::*;
use gtk::{Button, Window, WindowType};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::AppState;

#[derive(Clone, Debug)]
pub struct GlobalDragState {
    pub window_id: u64,
    pub source_output: Option<String>,
    pub source_index: usize,
    pub start_root_x: f64,
    pub start_root_y: f64,
    pub current_global_x: f64,
    pub current_global_y: f64,
    pub is_dragging: bool,
    /// Local (same-output) reorder slot, updated live while hovering the
    /// source bar. `target_slot` is the cross-monitor preview slot applied
    /// to the *remote* bar. Two fields are intentional: local motion updates
    /// `current_target_slot` every pointer event, while cross-monitor hover
    /// only updates `target_slot` when the preview slot is computed in
    /// `reconcile`. `decide_drag_action` prefers the cross-monitor target.
    pub current_target_slot: usize,
    pub target_output: Option<String>,
    pub target_slot: usize,
    /// True once the remote `reconcile` has computed `target_slot` for the
    /// current `target_output`. Guards fast-drop: releasing before the next
    /// updater wakeup must not use a stale slot (falls back to append).
    pub target_slot_valid: bool,
    /// Monotonic id to distinguish overlapping drags (A closed, B started).
    /// Read in debug logs to correlate press/motion/release across buttons.
    pub generation: u64,
}

/// Drag-start threshold in logical pixels, scaled by monitor scale factor
/// by callers (see taskbar motion handler).
pub const DRAG_THRESHOLD_LOGICAL_PX: f64 = 6.0;

pub static GLOBAL_DRAG: Mutex<Option<GlobalDragState>> = Mutex::new(None);

/// Next drag generation. Bumps on every new press so overlapping drags
/// (A closed, B started) can be told apart by background threads.
pub static DRAG_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Pending drag-cancel from a background (Niri IPC) thread, as the closed
/// `window_id`. The GTK main thread takes it and only cleans up when it
/// matches (or when no drag is active). Never a bare bool: a global flag
/// would kill an unrelated drag B started after A closed.
pub static DRAG_CANCELLED: Mutex<Option<u64>> = Mutex::new(None);

/// Request main-thread drag cleanup for `window_id` (background-safe).
pub fn request_drag_cancel(window_id: u64) {
    *crate::util::lock(&DRAG_CANCELLED) = Some(window_id);
}

/// Take a pending cancel, returning the closed `window_id` if any.
pub fn take_drag_cancel() -> Option<u64> {
    crate::util::lock(&DRAG_CANCELLED).take()
}

/// IPC action decided at drag release. Pure decision (no I/O) so it is unit-testable;
/// `finish_drag` below maps it to `gio::spawn_blocking` calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DragAction {
    None,
    MoveToMonitor {
        output: String,
        /// 1-based slot, or `None` to append when the preview slot was never
        /// computed (fast flick-and-release before the next reconcile).
        slot_1based: Option<usize>,
    },
    MoveToColumn {
        col_1based: usize,
    },
    Focus,
}

/// Decide the release action for a taken drag state.
///
/// Niri indices are 1-based; internal slots are 0-based.
pub fn decide_drag_action(drag: &GlobalDragState, click_to_focus: bool) -> DragAction {
    if !drag.is_dragging {
        return if click_to_focus {
            DragAction::Focus
        } else {
            DragAction::None
        };
    }
    if let Some(ref target_out) = drag.target_output {
        let slot = if drag.target_slot_valid {
            Some(drag.target_slot + 1)
        } else {
            None
        };
        return DragAction::MoveToMonitor {
            output: target_out.clone(),
            slot_1based: slot,
        };
    }
    if drag.current_target_slot != drag.source_index {
        return DragAction::MoveToColumn {
            col_1based: drag.current_target_slot + 1,
        };
    }
    DragAction::None
}

/// Take the global drag (if it belongs to `window_id`), decide the action,
/// and dispatch the Niri IPC off the GTK thread. Replaces two copies of the
/// take + match + `spawn_blocking` sequence in the taskbar release handlers.
/// Callers still own cursor/overlay reset + update-channel nudge.
pub fn finish_drag(window_id: u64, click_to_focus: bool) {
    let drag_opt = {
        let mut g = crate::util::lock(&GLOBAL_DRAG);
        match g.take() {
            Some(d) if d.window_id == window_id => Some(d),
            other => {
                // Not ours: put it back so the owning button/overlay can finish it.
                *g = other;
                None
            }
        }
    };

    let action = match drag_opt {
        Some(ref d) => {
            log_debug!("drag", "Finishing drag win={} gen={}", d.window_id, d.generation);
            let action = decide_drag_action(d, click_to_focus);
            // Silent drops are the hardest drag bugs to diagnose (empty
            // taskbar on the target monitor looks identical to a dead drop):
            // log them at debug so `NIRI_BAR_DEBUG=1` shows what happened.
            if d.is_dragging && action == DragAction::None {
                log_debug!(
                    "drag",
                    "Dropping active drag win={} with no action (target_output={:?}, slot {}/{})",
                    d.window_id,
                    d.target_output,
                    d.current_target_slot,
                    d.source_index
                );
            }
            action
        }
        None => return,
    };

    match action {
        DragAction::None => {}
        DragAction::MoveToMonitor { output, slot_1based } => {
            gio::spawn_blocking(move || {
                if let Err(e) = crate::niri::ipc::move_window_to_monitor(window_id, &output, slot_1based) {
                    log_warn!(
                        "drag",
                        "Move window {window_id} to {output}:{slot_1based:?} failed: {e}"
                    );
                }
            });
        }
        DragAction::MoveToColumn { col_1based } => {
            gio::spawn_blocking(move || {
                if let Err(e) = crate::niri::ipc::move_window_to_column(window_id, col_1based) {
                    log_warn!("drag", "Move window {window_id} to column {col_1based} failed: {e}");
                }
            });
        }
        DragAction::Focus => {
            gio::spawn_blocking(move || {
                if let Err(e) = crate::niri::ipc::focus_window(window_id) {
                    log_warn!("drag", "Focus window {window_id} failed: {e}");
                }
            });
        }
    }
}

/// Hover decision from an already-resolved hover output. Pure.
///
/// `hover_output` must come from hit-testing a single space (see
/// [`gdk_monitor_at_point`]); bezel gaps yield `None` and never target.
/// Returns `(is_cross_monitor, new_target)`.
pub fn decide_hover_target(hover_output: Option<String>, source_output: &Option<String>) -> (bool, Option<String>) {
    let is_cross = hover_output.is_some() && hover_output != *source_output;
    let target = if is_cross { hover_output } else { None };
    (is_cross, target)
}

/// Compute the top-left logical coordinates of a bar on its output in Niri space.
/// This accounts for bar placement (`Left`, `Top`, `Right`, `Bottom`) and thickness.
pub fn bar_logical_origin(
    output_name: &str,
    outputs: &HashMap<String, OutputInfo>,
    position: crate::config::BarPosition,
    bar_size: i32,
) -> (f64, f64) {
    if let Some(out) = outputs.get(output_name) {
        if let Some(ref l) = out.logical {
            let (off_x, off_y) = match position {
                crate::config::BarPosition::Left | crate::config::BarPosition::Top => (0, 0),
                crate::config::BarPosition::Right => (l.width.saturating_sub(bar_size.max(0) as u32) as i32, 0),
                crate::config::BarPosition::Bottom => (0, l.height.saturating_sub(bar_size.max(0) as u32) as i32),
            };
            return (f64::from(l.x + off_x), f64::from(l.y + off_y));
        }
    }
    (0.0, 0.0)
}

/// Find which Niri output contains the given point in Niri logical space.
///
/// 1. Exact bounding box hit in Niri logical coordinates.
/// 2. Deterministic fallback to the nearest monitor by clamped Euclidean distance,
///    so cursor positions in 1-pixel rounding gaps or bezel borders never drop the target.
pub fn find_output_at_point(px: f64, py: f64, outputs: &HashMap<String, OutputInfo>) -> Option<String> {
    if outputs.is_empty() {
        return None;
    }

    // 1. Exact bounding box match in Niri logical coordinates
    for (name, out) in outputs {
        if let Some(ref logical) = out.logical {
            let min_x = f64::from(logical.x);
            let max_x = f64::from(logical.x + logical.width as i32);
            let min_y = f64::from(logical.y);
            let max_y = f64::from(logical.y + logical.height as i32);

            if px >= min_x && px < max_x && py >= min_y && py < max_y {
                return Some(name.clone());
            }
        }
    }

    // 2. Fallback: closest monitor by clamped Euclidean distance (for bezel gaps / slight overshoots)
    let mut best: Option<(&String, f64)> = None;
    for (name, out) in outputs {
        if let Some(ref logical) = out.logical {
            let min_x = f64::from(logical.x);
            let max_x = f64::from(logical.x + logical.width as i32);
            let min_y = f64::from(logical.y);
            let max_y = f64::from(logical.y + logical.height as i32);

            let clamped_x = px.clamp(min_x, max_x);
            let clamped_y = py.clamp(min_y, max_y);
            let dx = px - clamped_x;
            let dy = py - clamped_y;
            let dist_sq = dx * dx + dy * dy;

            match best {
                Some((_, best_dist)) if dist_sq < best_dist => {
                    best = Some((name, dist_sq));
                }
                None => {
                    best = Some((name, dist_sq));
                }
                _ => {}
            }
        }
    }

    best.map(|(name, _)| name.clone())
}

/// Map a GDK root point (surface-local to the bar window on `source_output`) into
/// Niri logical space and resolve the hovered output.
///
/// In Wayland, every toplevel surface's internal origin is `(0, 0)`, so event
/// coordinates are surface-local. Adding the bar's known logical origin in Niri
/// transforms the point into the global Niri coordinate system, where hit-testing
/// against Niri logical outputs determines the true target monitor.
pub fn map_root_to_niri(
    root_x: f64,
    root_y: f64,
    source_output: &Option<String>,
    outputs: &HashMap<String, OutputInfo>,
    position: crate::config::BarPosition,
    bar_size: i32,
) -> ((f64, f64), Option<String>) {
    let (bar_x, bar_y) = source_output
        .as_deref()
        .map(|o| bar_logical_origin(o, outputs, position, bar_size))
        .unwrap_or((0.0, 0.0));
    let global_x = bar_x + root_x;
    let global_y = bar_y + root_y;
    let hover = find_output_at_point(global_x, global_y, outputs);
    ((global_x, global_y), hover)
}

/// Store a new pointer position + cross-monitor target for `window_id`.
/// Main-thread only (touches the GTK-handler global drag). Invalidates the
/// preview slot until the remote `reconcile` recomputes it.
pub fn store_drag_position(window_id: u64, global_x: f64, global_y: f64, target: Option<String>) {
    let mut g = crate::util::lock(&GLOBAL_DRAG);
    if let Some(ref mut d) = *g {
        if d.window_id == window_id {
            if d.target_output != target {
                d.target_slot_valid = false;
            }
            d.current_global_x = global_x;
            d.current_global_y = global_y;
            d.target_output = target;
        }
    }
}

thread_local! {
    pub static OVERLAY_WINDOWS: std::cell::RefCell<Vec<Window>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub fn close_overlay_windows() {
    OVERLAY_WINDOWS.with(|cell| {
        for w in cell.borrow_mut().drain(..) {
            // Safe teardown: `Window::close()` goes through the normal
            // delete-event path on the main thread. Background threads never
            // call this directly (see DRAG_CANCELLED handoff in NiriService).
            w.close();
        }
    });
}

/// Spawn fullscreen transparent layer-shell overlays on every monitor so a
/// drag is tracked (and can be cancelled/dropped) outside the source button.
/// Must run on the GTK main thread. Extracted from the taskbar motion handler
/// with no behavior change; overlays are torn down by `close_overlay_windows`
/// via every release/Escape path and the `DRAG_CANCELLED` handoff.
pub fn spawn_monitor_overlays(
    state: &Arc<Mutex<AppState>>,
    update_tx: &async_channel::Sender<()>,
    window_id: u64,
    source_button: &Button,
) {
    // Never stack overlays from overlapping drags; one set per drag.
    close_overlay_windows();
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let n_monitors = display.n_monitors();
    for m_idx in 0..n_monitors {
        let Some(mon) = display.monitor(m_idx) else {
            continue;
        };
        let overlay = Window::new(WindowType::Toplevel);
        overlay.set_app_paintable(true);
        if let Some(screen) = gtk::prelude::WidgetExt::screen(&overlay) {
            if let Some(visual) = screen.rgba_visual() {
                overlay.set_visual(Some(&visual));
            }
        }
        crate::layer_shell::setup_overlay_window(&overlay, Some(&mon));
        overlay.add_events(
            gdk::EventMask::POINTER_MOTION_MASK | gdk::EventMask::BUTTON_RELEASE_MASK | gdk::EventMask::KEY_PRESS_MASK,
        );

        let state_overlay = Arc::clone(state);
        let tx_overlay = update_tx.clone();
        let wid_overlay = window_id;
        let mon_overlay = mon.clone();

        overlay.connect_motion_notify_event(move |_, o_ev| {
            // Fixed lock order: snapshot drag identity, drop guard, THEN
            // lock state. Never hold GLOBAL_DRAG across a state lock.
            let (ours, src_out) = {
                let g = crate::util::lock(&GLOBAL_DRAG);
                match g.as_ref() {
                    Some(d) if d.window_id == wid_overlay => (true, d.source_output.clone()),
                    _ => (false, None),
                }
            };
            if !ours {
                return glib::Propagation::Proceed;
            }
            let (ox, oy) = o_ev.position();

            let st_outputs = {
                let st = crate::util::lock(&state_overlay);
                st.outputs.clone()
            };

            let hover = super::detect_monitor_output(&mon_overlay, &st_outputs);
            let (cur_gx, cur_gy) = if let Some(ref out_name) = hover {
                if let Some(out) = st_outputs.get(out_name) {
                    if let Some(ref l) = out.logical {
                        (f64::from(l.x) + ox, f64::from(l.y) + oy)
                    } else {
                        (ox, oy)
                    }
                } else {
                    (ox, oy)
                }
            } else {
                (ox, oy)
            };

            let (_, new_target) = decide_hover_target(hover, &src_out);
            store_drag_position(wid_overlay, cur_gx, cur_gy, new_target);
            crate::util::nudge(&tx_overlay);
            glib::Propagation::Proceed
        });

        let b_overlay_rel = source_button.clone();
        let tx_overlay_rel = update_tx.clone();

        overlay.connect_button_release_event(move |_, o_ev| {
            if o_ev.button() != 1 {
                return glib::Propagation::Proceed;
            }

            reset_all_cursors_and_overlay(Some(&b_overlay_rel));
            // Overlay exists only mid-drag: never click-to-focus.
            finish_drag(wid_overlay, false);

            crate::util::nudge(&tx_overlay_rel);

            glib::Propagation::Stop
        });

        let b_overlay_esc = source_button.clone();
        let tx_overlay_esc = update_tx.clone();

        overlay.connect_key_press_event(move |_, k_ev| {
            if k_ev.keyval() == gdk::keys::constants::Escape {
                // Only cancel our own drag; a stale overlay must not kill a
                // newer drag started after this overlay was created.
                let ours = crate::util::lock(&GLOBAL_DRAG)
                    .as_ref()
                    .map(|d| d.window_id == wid_overlay)
                    .unwrap_or(false);
                if ours {
                    reset_all_cursors_and_overlay(Some(&b_overlay_esc));
                    crate::util::lock(&GLOBAL_DRAG).take();
                    crate::util::nudge(&tx_overlay_esc);
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });

        overlay.show_all();
        OVERLAY_WINDOWS.with(|cell| cell.borrow_mut().push(overlay));
    }
}

pub fn apply_drag_cursor_globally(icon_key: &str, size: i32, source_button: Option<&Button>) {
    let size = if size <= 0 { 20 } else { size };
    if let Some(pixbuf) = crate::icon::load_pixbuf_at_size(icon_key, size) {
        if let Some(display) = gdk::Display::default() {
            let cursor = gdk::Cursor::from_pixbuf(&display, &pixbuf, size / 2, size / 2);
            if let Some(screen) = gdk::Screen::default() {
                for win in screen.toplevel_windows() {
                    win.set_cursor(Some(&cursor));
                }
            }
            if let Some(b) = source_button {
                if let Some(w) = b.window() {
                    w.set_cursor(Some(&cursor));
                }
            }
            display.flush();
        }
    }
}

pub fn reset_all_cursors_and_overlay(source_button: Option<&Button>) {
    if let Some(display) = gdk::Display::default() {
        if let Some(screen) = gdk::Screen::default() {
            for win in screen.toplevel_windows() {
                win.set_cursor(None);
            }
        }
        // NOTE: no `display.sync()` here. It blocks the UI thread on a
        // round-trip for every drag release / Escape; `flush()` is enough.
        display.flush();
    }

    close_overlay_windows();

    if let Some(b) = source_button {
        if let Some(w) = b.window() {
            let display = w.display();
            let pointer_cursor = gdk::Cursor::from_name(&display, "pointer");
            w.set_cursor(pointer_cursor.as_ref());
        }
    }

    if let Some(display) = gdk::Display::default() {
        display.flush();
    }
}

/// One child's extent along the drag axis (x/width or y/height).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildBounds {
    pub pos: i32,
    pub len: i32,
}

/// Decide the same-monitor reorder slot from the pointer position along the
/// drag axis. Pure so the hover math is unit-testable; the GTK caller builds
/// `bounds` from child allocations and applies the result via `reorder_child`.
pub fn reorder_target(bounds: &[ChildBounds], pointer: i32, current_pos: usize) -> usize {
    let total = bounds.len();
    if total <= 1 {
        return current_pos.min(total.saturating_sub(1));
    }
    for (idx, b) in bounds.iter().enumerate() {
        if pointer >= b.pos && pointer < b.pos + b.len {
            return idx;
        }
    }
    let first = &bounds[0];
    let last = &bounds[total - 1];
    if pointer < first.pos {
        0
    } else if pointer >= last.pos + last.len {
        total - 1
    } else {
        current_pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_drag() -> GlobalDragState {
        GlobalDragState {
            window_id: 7,
            source_output: Some("eDP-1".into()),
            source_index: 1,
            start_root_x: 0.0,
            start_root_y: 0.0,
            current_global_x: 0.0,
            current_global_y: 0.0,
            is_dragging: true,
            current_target_slot: 1,
            target_output: None,
            target_slot: 1,
            target_slot_valid: false,
            generation: 1,
        }
    }

    #[test]
    fn test_decide_drag_action() {
        // Plain click focuses; overlay release without drag does nothing.
        let click = GlobalDragState {
            is_dragging: false,
            ..test_drag()
        };
        assert_eq!(decide_drag_action(&click, true), DragAction::Focus);
        assert_eq!(decide_drag_action(&click, false), DragAction::None);

        // Cross-monitor move wins over local slot, with 1-based slot.
        let mut cross = test_drag();
        cross.target_output = Some("DP-1".into());
        cross.target_slot = 2;
        cross.target_slot_valid = true;
        assert_eq!(
            decide_drag_action(&cross, true),
            DragAction::MoveToMonitor {
                output: "DP-1".into(),
                slot_1based: Some(3),
            }
        );

        // Fast drop before reconcile: append (None) instead of stale slot.
        let mut fast = test_drag();
        fast.target_output = Some("DP-1".into());
        fast.target_slot = 0;
        fast.target_slot_valid = false;
        assert_eq!(
            decide_drag_action(&fast, true),
            DragAction::MoveToMonitor {
                output: "DP-1".into(),
                slot_1based: None,
            }
        );

        // Local reorder.
        let mut local = test_drag();
        local.current_target_slot = 0;
        assert_eq!(
            decide_drag_action(&local, true),
            DragAction::MoveToColumn { col_1based: 1 }
        );

        // Dragged but dropped home: nothing.
        assert_eq!(decide_drag_action(&test_drag(), true), DragAction::None);
    }

    #[test]
    fn test_reorder_target() {
        let bounds = [
            ChildBounds { pos: 0, len: 40 },
            ChildBounds { pos: 40, len: 40 },
            ChildBounds { pos: 80, len: 40 },
        ];
        // Inside each child.
        assert_eq!(reorder_target(&bounds, 10, 0), 0);
        assert_eq!(reorder_target(&bounds, 50, 0), 1);
        assert_eq!(reorder_target(&bounds, 100, 0), 2);
        // Before first / after last.
        assert_eq!(reorder_target(&bounds, -5, 1), 0);
        assert_eq!(reorder_target(&bounds, 200, 0), 2);
        // Boundary belongs to the next child (matches `< pos + len`).
        assert_eq!(reorder_target(&bounds, 40, 0), 1);
        // Empty and single-child containers stay put.
        assert_eq!(reorder_target(&[], 10, 0), 0);
        assert_eq!(reorder_target(&bounds[..1], 500, 0), 0);
    }

    #[test]
    fn test_decide_hover_target() {
        let src: Option<String> = Some("eDP-1".to_string());

        // Hovering home monitor: not cross, no target.
        assert_eq!(decide_hover_target(Some("eDP-1".to_string()), &src), (false, None));

        // Hovering the other monitor: cross with target.
        assert_eq!(
            decide_hover_target(Some("DP-1".to_string()), &src),
            (true, Some("DP-1".to_string()))
        );

        // Bezel gap (no hover): never a target, cross or not.
        assert_eq!(decide_hover_target(None, &src), (false, None));

        // Unresolved source output: any hover counts as cross.
        assert_eq!(
            decide_hover_target(Some("DP-1".to_string()), &None),
            (true, Some("DP-1".to_string()))
        );
    }

    #[test]
    fn test_bar_logical_origin() {
        use crate::config::BarPosition;
        use crate::niri::model::LogicalOutput;

        let mut outputs = HashMap::new();
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

        assert_eq!(
            bar_logical_origin("DP-1", &outputs, BarPosition::Left, 50),
            (1423.0, 0.0)
        );
        assert_eq!(
            bar_logical_origin("DP-1", &outputs, BarPosition::Top, 50),
            (1423.0, 0.0)
        );
        assert_eq!(
            bar_logical_origin("DP-1", &outputs, BarPosition::Right, 50),
            (3293.0, 0.0)
        );
        assert_eq!(
            bar_logical_origin("DP-1", &outputs, BarPosition::Bottom, 50),
            (1423.0, 1030.0)
        );
        assert_eq!(
            bar_logical_origin("nonexistent", &outputs, BarPosition::Left, 50),
            (0.0, 0.0)
        );
    }

    #[test]
    fn test_find_output_at_point() {
        use crate::niri::model::LogicalOutput;

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
                    scale: Some(1.35),
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

        // Exact hits
        assert_eq!(find_output_at_point(500.0, 300.0, &outputs), Some("eDP-1".to_string()));
        assert_eq!(find_output_at_point(1500.0, 300.0, &outputs), Some("DP-1".to_string()));

        // 1px gap between monitors (1422.5 is between 1422 and 1423)
        let gap_hit = find_output_at_point(1422.5, 300.0, &outputs);
        assert!(gap_hit.is_some(), "Points in bezel gap must resolve to nearest output");

        // Overshoot outside all monitors
        assert_eq!(find_output_at_point(-100.0, 300.0, &outputs), Some("eDP-1".to_string()));
        assert_eq!(find_output_at_point(4000.0, 300.0, &outputs), Some("DP-1".to_string()));

        // Empty outputs
        assert_eq!(find_output_at_point(100.0, 100.0, &HashMap::new()), None);
    }

    #[test]
    fn test_right_to_left_drag_hover_behavior() {
        use crate::config::BarPosition;
        use crate::niri::model::LogicalOutput;

        let mut outputs = HashMap::new();
        outputs.insert(
            "eDP-1".to_string(),
            OutputInfo {
                name: Some("eDP-1".to_string()),
                make: Some("LG Display".to_string()),
                model: Some("0x05FE".to_string()),
                physical_size: Some([340, 190]),
                logical: Some(LogicalOutput {
                    x: 0,
                    y: 0,
                    width: 1422,
                    height: 800,
                    scale: Some(1.35),
                }),
            },
        );
        outputs.insert(
            "DP-1".to_string(),
            OutputInfo {
                name: Some("DP-1".to_string()),
                make: Some("Microstep".to_string()),
                model: Some("MSI MAG271C".to_string()),
                physical_size: Some([600, 340]),
                logical: Some(LogicalOutput {
                    x: 1423,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    scale: Some(1.0),
                }),
            },
        );

        let right_src = Some("DP-1".to_string());

        // 1. Dragging on DP-1 (right monitor) and hovering over DP-1's bar:
        // Must stay on DP-1 (NOT jump to eDP-1).
        let ((gx_home, gy_home), hover_home) =
            map_root_to_niri(25.0, 50.0, &right_src, &outputs, BarPosition::Left, 50);
        assert_eq!(gx_home, 1448.0);
        assert_eq!(gy_home, 50.0);
        assert_eq!(hover_home, Some("DP-1".to_string()));
        let (is_cross, target) = decide_hover_target(hover_home, &right_src);
        assert!(!is_cross);
        assert_eq!(target, None);

        // 2. Dragging left from DP-1 into eDP-1:
        // Pointer moves left across monitor boundary (root_x = -423.0 on DP-1 bar = 1000.0 in Niri space).
        let ((gx_left, gy_left), hover_left) =
            map_root_to_niri(-423.0, 50.0, &right_src, &outputs, BarPosition::Left, 50);
        assert_eq!(gx_left, 1000.0);
        assert_eq!(gy_left, 50.0);
        assert_eq!(hover_left, Some("eDP-1".to_string()));
        let (is_cross_left, target_left) = decide_hover_target(hover_left, &right_src);
        assert!(is_cross_left);
        assert_eq!(target_left, Some("eDP-1".to_string()));

        // 3. Dragging from eDP-1 (left monitor) to DP-1 (right monitor):
        let left_src = Some("eDP-1".to_string());
        let ((gx_left_home, _), hover_left_home) =
            map_root_to_niri(25.0, 50.0, &left_src, &outputs, BarPosition::Left, 50);
        assert_eq!(gx_left_home, 25.0);
        assert_eq!(hover_left_home, Some("eDP-1".to_string()));
        assert_eq!(decide_hover_target(hover_left_home, &left_src), (false, None));

        let ((gx_right, _), hover_right) = map_root_to_niri(1500.0, 50.0, &left_src, &outputs, BarPosition::Left, 50);
        assert_eq!(gx_right, 1500.0);
        assert_eq!(hover_right, Some("DP-1".to_string()));
        assert_eq!(
            decide_hover_target(hover_right, &left_src),
            (true, Some("DP-1".to_string()))
        );
    }

    #[test]
    fn test_scale_to_exact_square() {
        let pb = gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 48, 48).unwrap();
        let scaled = crate::icon::normalize_pixbuf_to_size(pb, 32);
        assert_eq!(scaled.width(), 32);
        assert_eq!(scaled.height(), 32);
    }

    #[test]
    fn test_create_image_for_theme_icon_preserves_icon_name() {
        if gtk::is_initialized() {
            if !gtk::is_initialized_main_thread() {
                eprintln!("Skipping GTK test: running on non-main GTK thread");
                return;
            }
        } else if gtk::init().is_err() {
            eprintln!("Skipping GTK test: display not available");
            return;
        }

        let img = crate::icon::create_app_image("application-x-executable", 32, 2);
        assert_eq!(img.storage_type(), gtk::ImageType::IconName);
        assert_eq!(img.pixel_size(), 32);
    }

    #[test]
    fn test_create_surface_from_pixbuf_with_scale() {
        if gtk::is_initialized() {
            if !gtk::is_initialized_main_thread() {
                eprintln!("Skipping GTK test: running on non-main GTK thread");
                return;
            }
        } else if gtk::init().is_err() {
            eprintln!("Skipping GTK test: display not available");
            return;
        }

        let pb = gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 64, 64).unwrap();
        let surface = pb.create_surface(2, Option::<&gdk::Window>::None);
        assert!(surface.is_some());
        let surf = surface.unwrap();
        assert_eq!(surf.device_scale(), (2.0, 2.0));

        let img = gtk::Image::from_surface(Some(&surf));
        assert_eq!(img.storage_type(), gtk::ImageType::Surface);
    }
}
