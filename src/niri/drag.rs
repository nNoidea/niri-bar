use crate::niri::model::OutputInfo;
use gdk::prelude::*;
use gtk::prelude::*;
use gtk::{Button, Window};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Snapshot returned when a motion event exceeds the drag threshold.
pub struct DragMotionStep {
    pub just_started: bool,
    pub source_output: Option<String>,
    pub prev_target: Option<String>,
}

/// Owned cross-thread coordinator for taskbar drag state, generation tracking,
/// and background cancellation handoffs (no process-wide statics).
pub struct DragController {
    state: Mutex<Option<GlobalDragState>>,
    gen: AtomicU64,
    cancelled: Mutex<Option<u64>>,
}

impl Default for DragController {
    fn default() -> Self {
        Self::new()
    }
}

impl DragController {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
            gen: AtomicU64::new(1),
            cancelled: Mutex::new(None),
        }
    }

    pub fn snapshot(&self) -> Option<GlobalDragState> {
        crate::util::lock(&self.state).clone()
    }

    pub fn active_window_id(&self) -> Option<u64> {
        crate::util::lock(&self.state).as_ref().map(|d| d.window_id)
    }

    pub fn begin_press(
        &self,
        window_id: u64,
        source_output: Option<String>,
        source_index: usize,
        start_root: (f64, f64),
        start_global: (f64, f64),
    ) {
        let mut drag = crate::util::lock(&self.state);
        // Single-drag guard: never clobber an active drag from another button/overlay.
        if drag.as_ref().is_some_and(|e| e.is_dragging && e.window_id != window_id) {
            return;
        }
        let generation = self.gen.fetch_add(1, Ordering::Relaxed);
        *drag = Some(GlobalDragState {
            window_id,
            source_output,
            source_index,
            start_root_x: start_root.0,
            start_root_y: start_root.1,
            current_global_x: start_global.0,
            current_global_y: start_global.1,
            is_dragging: false,
            current_target_slot: source_index,
            target_output: None,
            target_slot: source_index,
            target_slot_valid: false,
            generation,
        });
    }

    pub fn check_motion_step(&self, window_id: u64, root_x: f64, root_y: f64) -> Option<DragMotionStep> {
        let mut drag_guard = crate::util::lock(&self.state);
        let drag = drag_guard.as_mut()?;
        if drag.window_id != window_id {
            return None;
        }
        let (dx, dy) = (root_x - drag.start_root_x, root_y - drag.start_root_y);
        if (dx * dx + dy * dy).sqrt() <= DRAG_THRESHOLD_LOGICAL_PX {
            return None;
        }
        let just_started = !drag.is_dragging;
        if just_started {
            drag.is_dragging = true;
        }
        Some(DragMotionStep {
            just_started,
            source_output: drag.source_output.clone(),
            prev_target: drag.target_output.clone(),
        })
    }

    pub fn overlay_drag_source(&self, window_id: u64) -> Option<Option<String>> {
        let g = crate::util::lock(&self.state);
        match g.as_ref() {
            Some(d) if d.window_id == window_id => Some(d.source_output.clone()),
            _ => None,
        }
    }

    /// Store a new pointer position + cross-monitor target for `window_id`.
    /// Invalidates the preview slot until the remote `reconcile` recomputes it.
    pub fn store_position(&self, window_id: u64, global_x: f64, global_y: f64, target: Option<String>) {
        let mut g = crate::util::lock(&self.state);
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

    pub fn set_current_slot(&self, window_id: u64, slot: usize) {
        let mut g = crate::util::lock(&self.state);
        if let Some(ref mut d) = *g {
            if d.window_id == window_id {
                d.current_target_slot = slot;
                d.target_slot = slot;
            }
        }
    }

    pub fn set_cross_monitor_preview_slot(&self, window_id: u64, slot: usize) {
        let mut g = crate::util::lock(&self.state);
        if let Some(ref mut d) = *g {
            if d.window_id == window_id {
                d.target_slot = slot;
                d.target_slot_valid = true;
            }
        }
    }

    /// Background-safe cancellation when Niri closes `window_id`.
    pub fn cancel_window(&self, window_id: u64) -> bool {
        let cleared = {
            let mut g = crate::util::lock(&self.state);
            if g.as_ref().is_some_and(|d| d.window_id == window_id) {
                g.take();
                true
            } else {
                false
            }
        };
        if cleared {
            *crate::util::lock(&self.cancelled) = Some(window_id);
        }
        cleared
    }

    /// Main-thread check: consumes any pending background cancel and reports whether
    /// GTK cursor/overlay cleanup should run now (never clobbering a newer drag).
    pub fn should_cleanup_cancel(&self) -> bool {
        let Some(cancelled_id) = crate::util::lock(&self.cancelled).take() else {
            return false;
        };
        let current_id = self.active_window_id();
        current_id.is_none() || current_id == Some(cancelled_id)
    }

    /// Escape-key cancellation: clears active drag only if it belongs to `window_id`.
    pub fn cancel_if_matching(&self, window_id: u64) -> bool {
        let mut g = crate::util::lock(&self.state);
        if g.as_ref().is_some_and(|d| d.window_id == window_id) {
            g.take();
            true
        } else {
            false
        }
    }

    /// Take the active drag (if it belongs to `window_id`), decide the action,
    /// and dispatch the Niri IPC off the GTK thread.
    pub fn finish_drag(&self, window_id: u64, click_to_focus: bool) {
        let drag_opt = {
            let mut g = crate::util::lock(&self.state);
            match g.take() {
                Some(d) if d.window_id == window_id => Some(d),
                other => {
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

        dispatch_drag_action(window_id, action);
    }

    /// Handle `GestureDrag::drag_end` on a taskbar source button.
    pub fn handle_source_gesture_end(
        &self,
        window_id: u64,
        sequence_cancelled: bool,
        source_button: Option<&Button>,
        update_tx: &async_channel::Sender<()>,
    ) -> bool {
        if sequence_cancelled {
            let is_actively_dragging = crate::util::lock(&self.state)
                .as_ref()
                .is_some_and(|d| d.window_id == window_id && d.is_dragging);
            if is_actively_dragging {
                return false;
            }
        }
        reset_all_cursors_and_overlay(source_button);
        self.finish_drag(window_id, !sequence_cancelled);
        crate::util::nudge(update_tx);
        true
    }

    #[cfg(test)]
    pub(crate) fn set_for_test(&self, drag: Option<GlobalDragState>) {
        *crate::util::lock(&self.state) = drag;
    }

    #[cfg(test)]
    pub(crate) fn is_dragging_or_activate_test(&self, window_id: u64, activate: bool) -> bool {
        let mut g = crate::util::lock(&self.state);
        if let Some(ref mut d) = *g {
            if d.window_id == window_id {
                if activate {
                    d.is_dragging = true;
                }
                return d.is_dragging;
            }
        }
        false
    }
}

/// IPC action decided at drag release. Pure decision (no I/O) so it is unit-testable;
/// `DragController::finish_drag` dispatches it asynchronously on the shared Tokio runtime.
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

fn dispatch_drag_action(window_id: u64, action: DragAction) {
    match action {
        DragAction::None => {}
        DragAction::MoveToMonitor { output, slot_1based } => {
            crate::runtime::spawn(async move {
                if let Err(e) = crate::niri::ipc::move_window_to_monitor(window_id, &output, slot_1based).await {
                    log_warn!(
                        "drag",
                        "Move window {window_id} to {output}:{slot_1based:?} failed: {e}"
                    );
                }
            });
        }
        DragAction::MoveToColumn { col_1based } => {
            crate::runtime::spawn(async move {
                if let Err(e) = crate::niri::ipc::move_window_to_column(window_id, col_1based).await {
                    log_warn!("drag", "Move window {window_id} to column {col_1based} failed: {e}");
                }
            });
        }
        DragAction::Focus => {
            crate::runtime::spawn(async move {
                if let Err(e) = crate::niri::ipc::focus_window(window_id).await {
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
    let mut best: Option<(&String, f64)> = None;
    for (name, out) in outputs {
        let Some(ref logical) = out.logical else {
            continue;
        };
        let min_x = f64::from(logical.x);
        let max_x = f64::from(logical.x + logical.width as i32);
        let min_y = f64::from(logical.y);
        let max_y = f64::from(logical.y + logical.height as i32);

        if px >= min_x && px < max_x && py >= min_y && py < max_y {
            return Some(name.clone());
        }

        let dx = px - px.clamp(min_x, max_x);
        let dy = py - py.clamp(min_y, max_y);
        let dist_sq = dx * dx + dy * dy;
        if best.is_none_or(|(_, b)| dist_sq < b) {
            best = Some((name, dist_sq));
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

#[derive(Default)]
struct DragOverlayUi {
    windows: Vec<Window>,
    cursor_grab: bool,
}

thread_local! {
    /// Main-thread-only overlay windows and cursor-grab flag (`gtk::Window` is `!Send`).
    static DRAG_UI: RefCell<DragOverlayUi> = const {
        RefCell::new(DragOverlayUi {
            windows: Vec::new(),
            cursor_grab: false,
        })
    };
}

pub fn close_overlay_windows() {
    let windows: Vec<Window> = DRAG_UI.with(|cell| std::mem::take(&mut cell.borrow_mut().windows));
    for w in windows {
        w.close();
    }
}

fn complete_overlay_drag(drag: &DragController, wid: u64, source_btn: &Button, tx: &async_channel::Sender<()>) {
    reset_all_cursors_and_overlay(Some(source_btn));
    // Overlay exists only mid-drag: never click-to-focus.
    drag.finish_drag(wid, false);
    crate::util::nudge(tx);
}

/// Spawn fullscreen transparent layer-shell overlays on every monitor so a
/// drag is tracked (and can be cancelled/dropped) outside the source button.
/// Must run on the GTK main thread.
pub fn spawn_monitor_overlays(
    drag: &Arc<DragController>,
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
    for mon in crate::monitor::display_monitors(&display) {
        let overlay = Window::new();
        overlay.add_css_class("drag-overlay");
        crate::layer_shell::setup_overlay_window(&overlay, Some(&mon));

        let drag_motion = Arc::clone(drag);
        let state_overlay = Arc::clone(state);
        let tx_overlay = update_tx.clone();

        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(move |_, ox, oy| {
            // Snapshot drag source BEFORE locking state (fixed lock ordering).
            let Some(src_out) = drag_motion.overlay_drag_source(window_id) else {
                return;
            };

            let (cur_gx, cur_gy, hover) = {
                let st = crate::util::lock(&state_overlay);
                let hover = super::detect_monitor_output(&mon, &st.outputs);
                let (gx, gy) = hover
                    .as_ref()
                    .and_then(|name| st.outputs.get(name))
                    .and_then(|o| o.logical.as_ref())
                    .map_or((ox, oy), |l| (f64::from(l.x) + ox, f64::from(l.y) + oy));
                (gx, gy, hover)
            };

            let (_, new_target) = decide_hover_target(hover, &src_out);
            drag_motion.store_position(window_id, cur_gx, cur_gy, new_target);
            crate::util::nudge(&tx_overlay);
        });
        overlay.add_controller(motion);

        let drag_legacy = Arc::clone(drag);
        let b_legacy = source_button.clone();
        let tx_legacy = update_tx.clone();
        let legacy = gtk::EventControllerLegacy::new();
        legacy.set_propagation_phase(gtk::PropagationPhase::Capture);
        legacy.connect_event(move |_, event| {
            if matches!(
                event.event_type(),
                gdk::EventType::ButtonRelease | gdk::EventType::ButtonPress
            ) {
                complete_overlay_drag(&drag_legacy, window_id, &b_legacy, &tx_legacy);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        overlay.add_controller(legacy);

        let drag_rel = Arc::clone(drag);
        let b_rel = source_button.clone();
        let tx_rel = update_tx.clone();
        let drag_unpaired = Arc::clone(drag);
        let b_unpaired = source_button.clone();
        let tx_unpaired = update_tx.clone();

        let click = gtk::GestureClick::builder()
            .button(0)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        click.connect_released(move |_, _, _, _| complete_overlay_drag(&drag_rel, window_id, &b_rel, &tx_rel));
        click.connect_unpaired_release(move |_, _, _, _, _| {
            complete_overlay_drag(&drag_unpaired, window_id, &b_unpaired, &tx_unpaired)
        });
        overlay.add_controller(click);

        let drag_esc = Arc::clone(drag);
        let b_overlay_esc = source_button.clone();
        let tx_overlay_esc = update_tx.clone();

        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gdk::Key::Escape && drag_esc.cancel_if_matching(window_id) {
                reset_all_cursors_and_overlay(Some(&b_overlay_esc));
                crate::util::nudge(&tx_overlay_esc);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        overlay.add_controller(key_ctrl);

        overlay.set_visible(true);
        gtk::prelude::WidgetExt::realize(&overlay);
        DRAG_UI.with(|cell| cell.borrow_mut().windows.push(overlay));
    }
}

#[cfg(test)]
pub fn drag_cursor_grab_active() -> bool {
    DRAG_UI.with(|cell| cell.borrow().cursor_grab)
}

fn set_drag_cursor_grab(active: bool) {
    DRAG_UI.with(|cell| cell.borrow_mut().cursor_grab = active);
}

/// Scale an existing `gdk::Texture` to an exact square `(size, size)` logical
/// cursor texture so HiDPI (`scale_factor >= 2`) button textures are not
/// rendered oversized by `gdk::Cursor::from_texture`.
fn scale_texture_to_square(texture: &gdk::Texture, size: i32) -> Option<gdk::Texture> {
    if size <= 0 {
        return None;
    }
    let w = texture.width();
    let h = texture.height();
    if w <= 0 || h <= 0 {
        return None;
    }
    if w == size && h == size {
        return Some(texture.clone());
    }

    let src_stride = cairo::Format::ARgb32.stride_for_width(w as u32).ok()? as usize;
    let src_len = src_stride.checked_mul(h as usize)?;
    let mut src_bytes = vec![0u8; src_len];
    texture.download(&mut src_bytes, src_stride);

    let src_surface =
        cairo::ImageSurface::create_for_data(src_bytes, cairo::Format::ARgb32, w, h, src_stride as i32).ok()?;
    let dst_surface = cairo::ImageSurface::create(cairo::Format::ARgb32, size, size).ok()?;
    {
        let cr = cairo::Context::new(&dst_surface).ok()?;
        cr.scale(f64::from(size) / f64::from(w), f64::from(size) / f64::from(h));
        cr.set_source_surface(&src_surface, 0.0, 0.0).ok()?;
        cr.source().set_filter(cairo::Filter::Bilinear);
        cr.paint().ok()?;
    }
    dst_surface.flush();
    let dst_stride = dst_surface.stride() as usize;
    let dst_data = dst_surface.take_data().ok()?;
    let bytes = glib::Bytes::from(&*dst_data);
    Some(gdk::MemoryTexture::new(size, size, gdk::MemoryFormat::B8g8r8a8Premultiplied, &bytes, dst_stride).upcast())
}

pub(crate) fn set_widget_and_surface_cursor(widget: &impl WidgetExt, cursor: Option<&gdk::Cursor>) {
    widget.set_cursor(cursor);
    if let Some(native) = widget.native() {
        if let Some(surface) = native.surface() {
            surface.set_cursor(cursor);
        }
    }
}

fn apply_cursor_to_windows(cursor: Option<&gdk::Cursor>, apply_to_source: Option<&Button>) {
    for widget in gtk::Window::list_toplevels() {
        set_widget_and_surface_cursor(&widget, cursor);
    }
    DRAG_UI.with(|cell| {
        for win in &cell.borrow().windows {
            set_widget_and_surface_cursor(win, cursor);
        }
    });
    if let Some(b) = apply_to_source {
        set_widget_and_surface_cursor(b, cursor);
    }
}

pub fn apply_drag_cursor_globally(icon_key: &str, size: i32, source_button: Option<&Button>) {
    let size = if size <= 0 { 20 } else { size };
    let texture_opt = source_button
        .and_then(|b| b.child())
        .and_then(|c| c.downcast::<gtk::Image>().ok())
        .and_then(|img| img.paintable())
        .and_then(|p| p.downcast::<gdk::Texture>().ok())
        .and_then(|tex| scale_texture_to_square(&tex, size))
        .or_else(|| crate::icon::load_pixbuf_at_size(icon_key, size).map(|pb| gdk::Texture::for_pixbuf(&pb)));

    if let Some(texture) = texture_opt {
        if let Some(display) = gdk::Display::default() {
            let cursor = gdk::Cursor::from_texture(&texture, texture.width() / 2, texture.height() / 2, None);
            apply_cursor_to_windows(Some(&cursor), source_button);
            set_drag_cursor_grab(true);
            display.flush();
        }
    }
}

pub fn reset_all_cursors_and_overlay(source_button: Option<&Button>) {
    set_drag_cursor_grab(false);

    if let Some(display) = gdk::Display::default() {
        apply_cursor_to_windows(None, None);
        display.flush();
    }

    close_overlay_windows();

    if let Some(b) = source_button {
        b.remove_css_class("is-dragging");
        b.remove_css_class("drag-placeholder");
        b.remove_css_class("drag-source-away");
        b.set_cursor_from_name(Some("pointer"));
        if let Some(native) = b.native() {
            if let Some(surface) = native.surface() {
                surface.set_cursor(None);
            }
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
    fn test_drag_cursor_applied_to_surfaces_and_reset() {
        crate::util::run_gtk_test(|| {
            struct Teardown;
            impl Drop for Teardown {
                fn drop(&mut self) {
                    reset_all_cursors_and_overlay(None);
                }
            }
            let _guard = Teardown;

            let win = Window::new();
            win.set_visible(true);
            gtk::prelude::WidgetExt::realize(&win);

            let native = win.native().expect("Window must have a native");
            let surface = native.surface().expect("Window must have a native surface");
            assert!(surface.cursor().is_none(), "Initial surface cursor must be None");

            let source_btn = Button::new();
            source_btn.set_cursor_from_name(Some("pointer"));
            let pb = crate::icon::load_pixbuf_at_size("firefox", 24).unwrap();
            let texture = gdk::Texture::for_pixbuf(&pb);
            let img = gtk::Image::from_paintable(Some(&texture));
            source_btn.set_child(Some(&img));
            win.set_child(Some(&source_btn));

            apply_drag_cursor_globally("firefox", 24, Some(&source_btn));

            assert!(drag_cursor_grab_active(), "DRAG_CURSOR_GRAB must be true");
            assert!(win.cursor().is_some(), "Window widget cursor must be set");
            assert!(
                surface.cursor().is_some(),
                "GDK surface cursor must be set to the drag icon so Wayland compositor displays it"
            );
            assert!(source_btn.cursor().is_some(), "Source button widget cursor must be set");

            reset_all_cursors_and_overlay(Some(&source_btn));

            assert!(!drag_cursor_grab_active(), "DRAG_CURSOR_GRAB must be false after reset");
            assert!(
                surface.cursor().is_none(),
                "GDK surface cursor must be None after reset"
            );
            assert!(win.cursor().is_none(), "Window widget cursor must be None after reset");
            assert_eq!(
                source_btn.cursor().map(|c| c.name().map(|n| n.to_string())),
                Some(Some("pointer".to_string())),
                "Source button cursor must be restored to pointer after reset"
            );
        });
    }

    #[test]
    fn test_create_surface_from_pixbuf_with_scale() {
        crate::util::run_gtk_test(|| {
            let pb = gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 64, 64).unwrap();
            let texture = gdk::Texture::for_pixbuf(&pb);
            assert_eq!(texture.width(), 64);
            assert_eq!(texture.height(), 64);

            let cursor = gdk::Cursor::from_texture(&texture, 32, 32, None);
            assert_eq!(cursor.hotspot_x(), 32);
            assert_eq!(cursor.hotspot_y(), 32);

            let img = gtk::Image::from_paintable(Some(&texture));
            assert_eq!(img.storage_type(), gtk::ImageType::Paintable);
        });
    }

    #[test]
    fn test_drag_cursor_grab_released_on_reset() {
        crate::util::run_gtk_test(|| {
            // Pretend a drag grabbed the pointer for its cursor.
            set_drag_cursor_grab(true);
            assert!(drag_cursor_grab_active());

            // Every drag end path goes through reset: it must release the grab,
            // otherwise the bar keeps swallowing pointer events after the drop.
            reset_all_cursors_and_overlay(None);
            assert!(!drag_cursor_grab_active());

            // Idempotent: resetting with no grab is a no-op.
            reset_all_cursors_and_overlay(None);
            assert!(!drag_cursor_grab_active());
        });
    }

    #[test]
    fn test_source_gesture_end_preserves_active_drag_on_overlay_handoff() {
        crate::util::run_gtk_test(|| {
            let btn = gtk::Button::new();
            btn.add_css_class("is-dragging");
            let (tx, _rx) = async_channel::unbounded();
            let drag = DragController::new();

            // 1. Active drag (is_dragging = true) + sequence cancelled (overlay mapped over bar):
            // Must NOT abort active drag or strip .is-dragging; overlay now owns the drag.
            drag.set_for_test(Some(GlobalDragState {
                window_id: 77,
                is_dragging: true,
                ..test_drag()
            }));
            let ended = drag.handle_source_gesture_end(77, true, Some(&btn), &tx);
            assert!(
                !ended,
                "Cancelled GestureDrag on BarWindow while is_dragging=true must hand off to overlay without aborting"
            );
            assert_eq!(drag.active_window_id(), Some(77));
            assert!(btn.has_css_class("is-dragging"));

            // 2. Non-dragging press (is_dragging = false) + sequence cancelled:
            // Must clean up active drag without focusing.
            drag.set_for_test(Some(GlobalDragState {
                window_id: 77,
                is_dragging: false,
                ..test_drag()
            }));
            let ended_pre_drag = drag.handle_source_gesture_end(77, true, Some(&btn), &tx);
            assert!(ended_pre_drag);
            assert_eq!(drag.active_window_id(), None);
            assert!(!btn.has_css_class("is-dragging"));

            // 3. Normal plain click (is_dragging = false, sequence_cancelled = false):
            // Completes drag cleanly and clears active drag.
            drag.set_for_test(Some(GlobalDragState {
                window_id: 77,
                is_dragging: false,
                ..test_drag()
            }));
            let ended_click = drag.handle_source_gesture_end(77, false, Some(&btn), &tx);
            assert!(ended_click, "Plain click release must finish the drag");
            assert_eq!(drag.active_window_id(), None);

            // 4. Local drag release on source button (is_dragging = true, sequence_cancelled = false):
            // Drops locally and clears active drag.
            btn.add_css_class("is-dragging");
            drag.set_for_test(Some(GlobalDragState {
                window_id: 77,
                is_dragging: true,
                current_target_slot: 2,
                source_index: 0,
                ..test_drag()
            }));
            let ended_local = drag.handle_source_gesture_end(77, false, Some(&btn), &tx);
            assert!(ended_local, "Local drop on button must finish the drag");
            assert_eq!(drag.active_window_id(), None);
            assert!(!btn.has_css_class("is-dragging"));
        });
    }

    #[test]
    fn test_overlay_creation_and_escape_teardown() {
        crate::util::run_gtk_test(|| {
            use crate::niri::AppState;

            struct DragGuard;
            impl Drop for DragGuard {
                fn drop(&mut self) {
                    close_overlay_windows();
                }
            }
            let _guard = DragGuard;

            let drag = Arc::new(DragController::new());
            let state = Arc::new(Mutex::new(AppState::default()));
            let (tx, _rx) = async_channel::unbounded();
            let source_btn = Button::new();
            source_btn.add_css_class("is-dragging");

            drag.set_for_test(Some(GlobalDragState {
                window_id: 99,
                is_dragging: true,
                ..test_drag()
            }));

            spawn_monitor_overlays(&drag, &state, &tx, 99, &source_btn);

            // Overlays were spawned
            let overlay_count = DRAG_UI.with(|cell| cell.borrow().windows.len());
            assert!(
                overlay_count > 0,
                "Expected at least one overlay window spawned across monitors"
            );

            // Verify overlay controllers
            DRAG_UI.with(|cell| {
                let ui = cell.borrow();
                let overlay = &ui.windows[0];
                let controllers = overlay.observe_controllers();
                let mut has_motion = false;
                let mut has_legacy = false;
                let mut has_click = false;
                let mut has_key = false;

                for i in 0..controllers.n_items() {
                    let ctrl = controllers.item(i).unwrap();
                    if ctrl.is::<gtk::EventControllerMotion>() {
                        has_motion = true;
                    } else if let Some(legacy) = ctrl.downcast_ref::<gtk::EventControllerLegacy>() {
                        if legacy.propagation_phase() == gtk::PropagationPhase::Capture {
                            has_legacy = true;
                        }
                    } else if let Some(click) = ctrl.downcast_ref::<gtk::GestureClick>() {
                        if click.button() == 0 && click.propagation_phase() == gtk::PropagationPhase::Capture {
                            has_click = true;
                        }
                    } else if ctrl.is::<gtk::EventControllerKey>() {
                        has_key = true;
                    }
                }

                assert!(has_motion, "Overlay must have EventControllerMotion");
                assert!(has_legacy, "Overlay must have Capture-phase EventControllerLegacy");
                assert!(has_click, "Overlay must have Capture-phase GestureClick on button 0");
                assert!(has_key, "Overlay must have EventControllerKey for Escape");
                if crate::layer_shell::is_available() {
                    use crate::layer_shell::LayerShell;
                    assert_eq!(
                        overlay.keyboard_mode(),
                        crate::layer_shell::KeyboardMode::Exclusive,
                        "Drag overlay must request Exclusive keyboard mode so Escape cancels active drags"
                    );
                }
            });

            // Verify that applying drag cursor sets cursor on overlay window and its surface
            apply_drag_cursor_globally("firefox", 24, Some(&source_btn));
            DRAG_UI.with(|cell| {
                let ui = cell.borrow();
                let overlay = &ui.windows[0];
                assert!(overlay.cursor().is_some(), "Overlay window widget cursor must be set");
                if let Some(native) = overlay.native() {
                    if let Some(surface) = native.surface() {
                        assert!(surface.cursor().is_some(), "Overlay GDK surface cursor must be set");
                    }
                }
            });

            // Teardown via close_overlay_windows
            close_overlay_windows();
            let remaining = DRAG_UI.with(|cell| cell.borrow().windows.len());
            assert_eq!(remaining, 0, "Overlay windows must be emptied on close");
        });
    }

    #[test]
    fn test_drag_cursor_downsamples_hidpi_button_texture() {
        crate::util::run_gtk_test(|| {
            struct Teardown;
            impl Drop for Teardown {
                fn drop(&mut self) {
                    reset_all_cursors_and_overlay(None);
                }
            }
            let _guard = Teardown;

            let win = Window::new();
            let source_btn = Button::new();
            // Simulate a 2x HiDPI icon texture (48x48 pixels for a 24px logical icon size)
            // with distinct RGBA channels (R=0xE0, G=0x20, B=0x10, A=0xFF) to verify zero B/R swap.
            let pb = gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 48, 48).unwrap();
            pb.fill(0xE020_10FF);
            let texture = gdk::Texture::for_pixbuf(&pb);
            let img = gtk::Image::from_paintable(Some(&texture));
            source_btn.set_child(Some(&img));
            win.set_child(Some(&source_btn));
            win.set_visible(true);
            gtk::prelude::WidgetExt::realize(&win);

            apply_drag_cursor_globally("org.example.App", 24, Some(&source_btn));

            let cursor = source_btn.cursor().expect("Drag cursor must be set");
            assert_eq!(
                (cursor.hotspot_x(), cursor.hotspot_y()),
                (12, 12),
                "Drag cursor from 2x HiDPI button texture must scale to 24x24 logical px (hotspot 12,12)"
            );
            if let Some(tex) = cursor.texture() {
                assert_eq!((tex.width(), tex.height()), (24, 24));
                let mut bgra = vec![0u8; 24 * 24 * 4];
                tex.download(&mut bgra, 24 * 4);
                // B8G8R8A8_PREMULTIPLIED center pixel: B=0x10, G=0x20, R=0xE0, A=0xFF
                let idx = (12 * 24 + 12) * 4;
                assert_eq!(&bgra[idx..idx + 4], &[0x10, 0x20, 0xE0, 0xFF]);
            }
            win.close();
        });
    }

    #[test]
    fn test_set_widget_and_surface_cursor_updates_cursor() {
        crate::util::run_gtk_test(|| {
            let win = Window::new();
            let pb = gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 8, 8).unwrap();
            let tex = gdk::Texture::for_pixbuf(&pb);
            let cursor = gdk::Cursor::from_texture(&tex, 4, 4, None);
            set_widget_and_surface_cursor(&win, Some(&cursor));
            assert!(win.cursor().is_some());
            set_widget_and_surface_cursor(&win, None);
            assert!(win.cursor().is_none());
            win.close();
        });
    }

    #[test]
    fn test_drag_controller_isolation_and_lifecycle() {
        let a = DragController::new();
        let b = DragController::new();

        a.begin_press(10, Some("eDP-1".into()), 0, (1.0, 2.0), (1.0, 2.0));
        assert_eq!(a.active_window_id(), Some(10));
        assert_eq!(
            b.active_window_id(),
            None,
            "Separate DragController instances must be isolated"
        );

        // Monotonic generation & single-drag guard once dragging starts.
        assert!(a.is_dragging_or_activate_test(10, true));
        a.begin_press(20, Some("eDP-1".into()), 1, (5.0, 5.0), (5.0, 5.0));
        assert_eq!(
            a.active_window_id(),
            Some(10),
            "Active drag must not be clobbered by another window press"
        );

        // Switching target output resets preview slot validity; same output keeps it.
        a.set_cross_monitor_preview_slot(10, 2);
        a.store_position(10, 1500.0, 40.0, Some("DP-1".into()));
        assert!(!a.snapshot().unwrap().target_slot_valid);
        a.set_cross_monitor_preview_slot(10, 3);
        a.store_position(10, 1510.0, 42.0, Some("DP-1".into()));
        assert!(a.snapshot().unwrap().target_slot_valid);

        // Closing another window does not cancel window 10.
        assert!(!a.cancel_window(99));
        assert_eq!(a.active_window_id(), Some(10));

        // Closing window 10 clears it and schedules cleanup; if a newer drag B starts
        // first, should_cleanup_cancel returns false so B survives.
        assert!(a.cancel_window(10));
        assert_eq!(a.active_window_id(), None);
        a.begin_press(20, Some("eDP-1".into()), 1, (5.0, 5.0), (5.0, 5.0));
        assert!(
            !a.should_cleanup_cancel(),
            "Pending cancel for closed win 10 must not tear down newer drag for win 20"
        );

        // Cancelling win 20 when no new drag replaces it triggers cleanup.
        assert!(a.cancel_window(20));
        assert!(a.should_cleanup_cancel());
        assert!(!a.should_cleanup_cancel());
    }
}
