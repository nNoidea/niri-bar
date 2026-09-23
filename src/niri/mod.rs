pub mod drag;
pub mod ipc;
pub mod model;
pub mod taskbar;

use gdk::prelude::MonitorExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::niri::model::{Event, OutputInfo, WindowInfo, WorkspaceInfo};

#[derive(Default, Debug, Clone)]
pub struct AppState {
    pub outputs: HashMap<String, OutputInfo>,
    pub windows: Vec<WindowInfo>,
    pub workspaces: Vec<WorkspaceInfo>,
}

pub struct NiriService {
    pub state: Arc<Mutex<AppState>>,
    pub update_tx: async_channel::Sender<()>,
    pub update_rx: async_channel::Receiver<()>,
    running: Arc<AtomicBool>,
}

impl Drop for NiriService {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl NiriService {
    /// Best-effort outputs refetch off-thread, then nudge the main loop.
    ///
    /// Self-healing for bars stuck at `detected_output == None`: hotplug
    /// races (GDK geometry winning before `OutputsChanged` lands, or an
    /// `OutputsChanged` carrying a not-yet-complete map) otherwise leave a
    /// bar showing an empty taskbar forever. Callers throttle this.
    pub fn refresh_outputs_async(&self) {
        let state = Arc::clone(&self.state);
        let tx = self.update_tx.clone();
        thread::spawn(move || match ipc::fetch_outputs() {
            Ok(outputs) => {
                crate::util::lock(&state).outputs = outputs;
                crate::util::nudge(&tx);
            }
            Err(e) => log_warn!("niri", "On-demand outputs refetch failed: {e}"),
        });
    }

    pub fn new() -> Self {
        let (update_tx, update_rx) = async_channel::unbounded::<()>();
        let state = Arc::new(Mutex::new(AppState::default()));
        let running = Arc::new(AtomicBool::new(true));

        // Initial fetch off-thread: never stall the GTK main thread on
        // Unix-socket round-trips at startup. Bars render empty until the
        // first nudge lands.
        {
            let state_init = Arc::clone(&state);
            let tx_init = update_tx.clone();
            thread::spawn(move || {
                let mut st = crate::util::lock(&state_init);
                if let Ok(outputs) = ipc::fetch_outputs() {
                    st.outputs = outputs;
                } else {
                    log_warn!("niri", "Initial outputs fetch failed; starting empty");
                }
                if let Ok(windows) = ipc::fetch_windows() {
                    st.windows = windows;
                } else {
                    log_warn!("niri", "Initial windows fetch failed; starting empty");
                }
                if let Ok(workspaces) = ipc::fetch_workspaces() {
                    st.workspaces = workspaces;
                } else {
                    log_warn!("niri", "Initial workspaces fetch failed; starting empty");
                }
                drop(st);
                crate::util::nudge(&tx_init);
            });
        }

        // Spawn background event listener thread
        {
            let state_clone = Arc::clone(&state);
            let running_clone = Arc::clone(&running);
            let tx_clone = update_tx.clone();

            thread::spawn(move || {
                while running_clone.load(Ordering::Relaxed) {
                    let state_stream = Arc::clone(&state_clone);
                    let running_stream = Arc::clone(&running_clone);
                    let tx_stream = tx_clone.clone();
                    log_info!("niri", "Starting Niri IPC event listener loop");
                    let running_check = Arc::clone(&running_clone);
                    let res = ipc::listen_event_stream(
                        move |event| {
                            if !running_stream.load(Ordering::Relaxed) {
                                return;
                            }
                            log_debug!("niri", "Received IPC event: {}", event.privacy_summary());
                            // OutputsChanged needs blocking IPC refetches: do them
                            // WITHOUT holding the state lock so reconcile/taskbar
                            // updates are not stalled.
                            if let Event::OutputsChanged { outputs } = &event {
                                let need_fetch_outputs = outputs.as_ref().map(|o| o.is_empty()).unwrap_or(true);
                                let fetched_outputs = if need_fetch_outputs {
                                    match ipc::fetch_outputs() {
                                        Ok(o) => Some(o),
                                        Err(e) => {
                                            log_warn!("niri", "Outputs refetch failed after OutputsChanged: {e}");
                                            None
                                        }
                                    }
                                } else {
                                    None
                                };
                                let fetched_workspaces = match ipc::fetch_workspaces() {
                                    Ok(w) => Some(w),
                                    Err(e) => {
                                        log_warn!("niri", "Workspaces refetch failed after OutputsChanged: {e}");
                                        None
                                    }
                                };
                                let mut st = crate::util::lock(&state_stream);
                                if let Some(outs) = outputs.clone() {
                                    if !outs.is_empty() {
                                        st.outputs = outs;
                                    } else if let Some(fetched) = fetched_outputs {
                                        st.outputs = fetched;
                                    }
                                } else if let Some(fetched) = fetched_outputs {
                                    st.outputs = fetched;
                                }
                                if let Some(wspaces) = fetched_workspaces {
                                    st.workspaces = wspaces;
                                }
                                crate::util::nudge(&tx_stream);
                                return;
                            }
                            let mut st = crate::util::lock(&state_stream);
                            match event {
                                Event::WorkspacesChanged { workspaces } => {
                                    st.workspaces = workspaces;
                                }
                                Event::WorkspaceActivated { id, focused } => {
                                    let mut ws_output = None;
                                    for ws in &mut st.workspaces {
                                        if ws.id == id {
                                            ws_output.clone_from(&ws.output);
                                            ws.is_active = true;
                                            if focused {
                                                ws.is_focused = true;
                                            }
                                        }
                                    }
                                    if let Some(out) = ws_output {
                                        for ws in &mut st.workspaces {
                                            if ws.output.as_ref() == Some(&out) && ws.id != id {
                                                ws.is_active = false;
                                                if focused {
                                                    ws.is_focused = false;
                                                }
                                            } else if focused && ws.id != id {
                                                ws.is_focused = false;
                                            }
                                        }
                                    }
                                }
                                Event::OutputsChanged { .. } => {
                                    // Handled above without holding the lock.
                                }
                                Event::WindowsChanged { windows } => {
                                    st.windows = windows;
                                }
                                Event::WindowOpenedOrChanged { window } => {
                                    if window.is_focused {
                                        for w in &mut st.windows {
                                            w.is_focused = w.id == window.id;
                                        }
                                    }
                                    if let Some(idx) = st.windows.iter().position(|w| w.id == window.id) {
                                        st.windows[idx] = window;
                                    } else {
                                        st.windows.push(window);
                                    }
                                }
                                Event::WindowClosed { id } => {
                                    st.windows.retain(|w| w.id != id);
                                    let mut should_reset_drag = false;
                                    {
                                        let mut g = crate::util::lock(&drag::GLOBAL_DRAG);
                                        if let Some(ref d) = *g {
                                            if d.window_id == id {
                                                g.take();
                                                should_reset_drag = true;
                                            }
                                        }
                                    }
                                    // NOTE: background thread — do NOT touch GTK here
                                    // (`reset_all_cursors_and_overlay` creates/destroys
                                    // widgets and must run on the main thread).
                                    // We clear the drag state (Sync-safe) and the
                                    // nudge below wakes the main-thread updater,
                                    // which performs the GTK cleanup only if the
                                    // cancelled id still matches (see take_drag_cancel).
                                    if should_reset_drag {
                                        drag::request_drag_cancel(id);
                                    }
                                }
                                Event::WindowFocusChanged { id } => {
                                    for w in &mut st.windows {
                                        w.is_focused = id == Some(w.id);
                                    }
                                    if let Some(win_id) = id {
                                        if let Some(ws_id) =
                                            st.windows.iter().find(|w| w.id == win_id).and_then(|w| w.workspace_id)
                                        {
                                            for ws in &mut st.workspaces {
                                                if ws.id == ws_id {
                                                    ws.is_focused = true;
                                                    ws.is_active = true;
                                                } else {
                                                    ws.is_focused = false;
                                                }
                                            }
                                        }
                                    }
                                }
                                Event::WorkspaceActiveWindowChanged { .. } => {}
                                Event::WindowLayoutsChanged { changes } => {
                                    for (win_id, layout) in changes {
                                        if let Some(w) = st.windows.iter_mut().find(|w| w.id == win_id) {
                                            w.layout = Some(layout);
                                        }
                                    }
                                }
                                Event::Other => return,
                            }

                            crate::util::nudge(&tx_stream);
                        },
                        move || running_check.load(Ordering::Relaxed),
                    );

                    if !running_clone.load(Ordering::Relaxed) {
                        break;
                    }

                    if let Err(e) = res {
                        log_warn!("niri", "Niri event stream disconnected: {e}. Retrying in 1s...");
                    }
                    thread::sleep(Duration::from_secs(1));
                }
            });
        }

        Self {
            state,
            update_tx,
            update_rx,
            running,
        }
    }
}

/// Tolerances for monitor↔output matching. GDK and Niri disagree by a few
/// px/mm on fractional scale / overscan setups, so exact equality is wrong.
pub const GEOMETRY_TOLERANCE_PX: i32 = 30;
pub const PHYSICAL_TOLERANCE_MM: i32 = 10;

/// GTK-free fingerprint so output matching is unit-testable without a display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorFingerprint {
    pub x: i32,
    pub y: i32,
    pub make: Option<String>,
    pub model: Option<String>,
    pub w_mm: i32,
    pub h_mm: i32,
}

/// Pure output matching (steps 1-6). `single_monitor_system` gates the
/// single-output fallback so tests don't need `gdk::Display`.
pub fn match_output_for_monitor(
    fp: &MonitorFingerprint,
    outputs: &HashMap<String, OutputInfo>,
    single_monitor_system: bool,
) -> Option<String> {
    // 1. Logical geometry match. Candidates, not first-hit: overlapping
    // outputs (mirrored `position x=0 y=0` pairs, or a hotplug transient)
    // match the same point, and `HashMap` order is random — returning the
    // first hit would flap between outputs across restarts. A unique hit
    // returns immediately; multiple hits fall through so physical / make /
    // model disambiguate below.
    let mut geo_hits: Vec<String> = Vec::new();
    for (name, out) in outputs {
        if let Some(ref logical) = out.logical {
            let scale = logical.scale.unwrap_or(1.0);
            let matches_logical =
                (fp.x - logical.x).abs() <= GEOMETRY_TOLERANCE_PX && (fp.y - logical.y).abs() <= GEOMETRY_TOLERANCE_PX;
            let phys_x = (logical.x as f64 * scale).round() as i32;
            let phys_y = (logical.y as f64 * scale).round() as i32;
            let matches_scaled =
                (fp.x - phys_x).abs() <= GEOMETRY_TOLERANCE_PX && (fp.y - phys_y).abs() <= GEOMETRY_TOLERANCE_PX;
            if matches_logical || matches_scaled {
                geo_hits.push(name.clone());
            }
        }
    }
    if geo_hits.len() == 1 {
        return geo_hits.into_iter().next();
    }

    // Deterministic candidate pool for the steps below: ambiguous geometry
    // restricts matching to the tied outputs; otherwise all outputs apply.
    // Sorted so multi-match steps can't flap on `HashMap` order.
    let mut pool: Vec<String> = if geo_hits.is_empty() {
        outputs.keys().cloned().collect()
    } else {
        geo_hits.clone()
    };
    pool.sort();

    // 2. Physical size match (fallback for unpositioned/offset setups)
    if fp.w_mm > 0 && fp.h_mm > 0 {
        for name in &pool {
            if let Some(out) = outputs.get(name) {
                if let Some(phys) = out.physical_size {
                    if (fp.w_mm - phys[0]).abs() <= PHYSICAL_TOLERANCE_MM
                        && (fp.h_mm - phys[1]).abs() <= PHYSICAL_TOLERANCE_MM
                    {
                        return Some(name.clone());
                    }
                }
            }
        }
    }

    // 3. Make & Model match (case-insensitive fuzzy/substring)
    if let (Some(g_make), Some(g_model)) = (&fp.make, &fp.model) {
        let g_make_low = g_make.to_lowercase();
        let g_model_low = g_model.to_lowercase();

        for name in &pool {
            let out = &outputs[name];
            let n_make_low = out.make.as_deref().unwrap_or_default().to_lowercase();
            let n_model_low = out.model.as_deref().unwrap_or_default().to_lowercase();

            let make_matches =
                !n_make_low.is_empty() && (g_make_low.contains(&n_make_low) || n_make_low.contains(&g_make_low));
            let model_matches =
                !n_model_low.is_empty() && (g_model_low.contains(&n_model_low) || n_model_low.contains(&g_model_low));

            if make_matches && model_matches {
                return Some(name.clone());
            }
        }
    }

    // 4. Model-only match
    if let Some(g_model) = &fp.model {
        let g_model_low = g_model.to_lowercase();
        for name in &pool {
            let out = &outputs[name];
            let n_model_low = out.model.as_deref().unwrap_or_default().to_lowercase();
            if !n_model_low.is_empty() && (g_model_low.contains(&n_model_low) || n_model_low.contains(&g_model_low)) {
                return Some(name.clone());
            }
        }
    }

    // 5. Single-output fallback ONLY when exactly 1 monitor is attached to the system
    if single_monitor_system && outputs.len() == 1 {
        if let Some(name) = outputs.keys().next() {
            return Some(name.clone());
        }
    }

    // 6. Ambiguous geometry with nothing stricter matching (overlapping
    // outputs no physical/make/model step could tell apart, e.g. identical
    // mirrored twins): deterministic sorted-first instead of `HashMap`-order
    // flap. Debug-logged; this should be unreachable on sane layouts.
    if !geo_hits.is_empty() {
        geo_hits.sort();
        log_debug!(
            "niri",
            "Ambiguous monitor match for ({},{}), candidates {geo_hits:?}",
            fp.x,
            fp.y
        );
        return geo_hits.into_iter().next();
    }

    None
}

pub fn detect_monitor_output(monitor: &gdk::Monitor, outputs: &HashMap<String, OutputInfo>) -> Option<String> {
    let geom = monitor.geometry();
    let fp = MonitorFingerprint {
        x: geom.x(),
        y: geom.y(),
        make: monitor.manufacturer().map(|s| s.to_string()),
        model: monitor.model().map(|s| s.to_string()),
        w_mm: monitor.width_mm(),
        h_mm: monitor.height_mm(),
    };
    let single = if outputs.len() == 1 {
        gdk::Display::default().map(|d| d.n_monitors() == 1).unwrap_or(false)
    } else {
        false
    };
    match_output_for_monitor(&fp, outputs, single)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::niri::model::{LogicalOutput, WindowLayout};

    #[test]
    fn test_app_state_window_lifecycle_events() {
        let mut state = AppState {
            workspaces: vec![
                WorkspaceInfo {
                    id: 1,
                    output: Some("eDP-1".to_string()),
                    is_active: true,
                    is_focused: true,
                },
                WorkspaceInfo {
                    id: 2,
                    output: Some("DP-1".to_string()),
                    is_active: true,
                    is_focused: false,
                },
            ],
            ..Default::default()
        };

        // 2. Window opened
        let win1 = WindowInfo {
            id: 100,
            title: Some("Firefox".to_string()),
            app_id: Some("firefox".to_string()),
            workspace_id: Some(1),
            is_focused: true,
            layout: Some(WindowLayout {
                pos_in_scrolling_layout: Some((0, 0)),
            }),
        };
        state.windows.push(win1);
        assert_eq!(state.windows.len(), 1);
        assert!(state.windows[0].is_focused);

        // 3. Second window opened and focused
        let win2 = WindowInfo {
            id: 200,
            title: Some("Alacritty".to_string()),
            app_id: Some("Alacritty".to_string()),
            workspace_id: Some(1),
            is_focused: true,
            layout: Some(WindowLayout {
                pos_in_scrolling_layout: Some((1, 0)),
            }),
        };
        for w in &mut state.windows {
            w.is_focused = false;
        }
        state.windows.push(win2);
        assert_eq!(state.windows.len(), 2);
        assert!(!state.windows[0].is_focused);
        assert!(state.windows[1].is_focused);

        // 4. Window focus changed back to win1
        for w in &mut state.windows {
            w.is_focused = w.id == 100;
        }
        assert!(state.windows[0].is_focused);
        assert!(!state.windows[1].is_focused);

        // 5. Window closed
        state.windows.retain(|w| w.id != 100);
        assert_eq!(state.windows.len(), 1);
        assert_eq!(state.windows[0].id, 200);
    }

    #[test]
    fn test_app_state_workspace_activation() {
        let mut state = AppState {
            workspaces: vec![
                WorkspaceInfo {
                    id: 1,
                    output: Some("eDP-1".to_string()),
                    is_active: true,
                    is_focused: true,
                },
                WorkspaceInfo {
                    id: 2,
                    output: Some("eDP-1".to_string()),
                    is_active: false,
                    is_focused: false,
                },
            ],
            ..Default::default()
        };

        // Activate workspace 2
        for ws in &mut state.workspaces {
            if ws.id == 2 {
                ws.is_active = true;
                ws.is_focused = true;
            } else if ws.output == Some("eDP-1".to_string()) {
                ws.is_active = false;
                ws.is_focused = false;
            }
        }

        assert!(!state.workspaces[0].is_active);
        assert!(!state.workspaces[0].is_focused);
        assert!(state.workspaces[1].is_active);
        assert!(state.workspaces[1].is_focused);
    }

    fn test_output(logical: Option<LogicalOutput>, make: &str, model: &str) -> OutputInfo {
        OutputInfo {
            name: None,
            make: if make.is_empty() { None } else { Some(make.into()) },
            model: if model.is_empty() { None } else { Some(model.into()) },
            physical_size: None,
            logical,
        }
    }

    fn logical_output(x: i32, y: i32, scale: f64) -> LogicalOutput {
        LogicalOutput {
            x,
            y,
            width: 1920,
            height: 1080,
            scale: Some(scale),
        }
    }

    #[test]
    fn test_match_output_logical_geometry() {
        let mut outputs = std::collections::HashMap::new();
        outputs.insert(
            "DP-1".into(),
            test_output(Some(logical_output(1920, 0, 1.0)), "Dell", "U2720Q"),
        );
        let fp = MonitorFingerprint {
            x: 1925,
            y: 5,
            make: None,
            model: None,
            w_mm: 0,
            h_mm: 0,
        };
        assert_eq!(match_output_for_monitor(&fp, &outputs, false), Some("DP-1".to_string()));
    }

    #[test]
    fn test_match_output_scaled_geometry() {
        // Niri logical (0,0) at scale 2.0 also matches physical (0,0) origin.
        let mut outputs = std::collections::HashMap::new();
        outputs.insert("eDP-1".into(), test_output(Some(logical_output(0, 0, 2.0)), "", ""));
        let fp = MonitorFingerprint {
            x: 2,
            y: -3,
            make: None,
            model: None,
            w_mm: 0,
            h_mm: 0,
        };
        assert_eq!(
            match_output_for_monitor(&fp, &outputs, false),
            Some("eDP-1".to_string())
        );
    }

    #[test]
    fn test_match_output_physical_and_model_fallbacks() {
        let mut outputs = std::collections::HashMap::new();
        let mut out = test_output(None, "Dell", "U2720Q");
        out.physical_size = Some([600, 340]);
        outputs.insert("DP-1".into(), out);

        // Far from any logical origin: falls through to physical-size match.
        let fp_phys = MonitorFingerprint {
            x: 9999,
            y: 9999,
            make: Some("Other".into()),
            model: Some("Other".into()),
            w_mm: 605,
            h_mm: 335,
        };
        assert_eq!(
            match_output_for_monitor(&fp_phys, &outputs, false),
            Some("DP-1".to_string())
        );

        // Wrong physical size: falls through to make+model fuzzy match.
        let fp_mm = MonitorFingerprint {
            x: 9999,
            y: 9999,
            make: Some("DELL".into()),
            model: Some("dell u2720q".into()),
            w_mm: 100,
            h_mm: 100,
        };
        assert_eq!(
            match_output_for_monitor(&fp_mm, &outputs, false),
            Some("DP-1".to_string())
        );
    }

    #[test]
    fn test_match_output_single_fallback_gated() {
        let mut outputs = std::collections::HashMap::new();
        outputs.insert("DP-1".into(), test_output(None, "", ""));
        let fp = MonitorFingerprint {
            x: 9999,
            y: 9999,
            make: None,
            model: None,
            w_mm: 0,
            h_mm: 0,
        };
        assert_eq!(match_output_for_monitor(&fp, &outputs, true), Some("DP-1".to_string()));
        assert_eq!(match_output_for_monitor(&fp, &outputs, false), None);
    }

    #[test]
    fn test_match_output_overlapping_disambiguated() {
        // Mirrored layout: both outputs at (0,0). Geometry alone ties, so
        // physical size must decide — deterministically, every call.
        let mut outputs = std::collections::HashMap::new();
        let mut laptop = test_output(Some(logical_output(0, 0, 1.35)), "LG Display", "0x05FE");
        laptop.physical_size = Some([340, 190]);
        outputs.insert("eDP-1".into(), laptop);
        let mut external = test_output(Some(logical_output(0, 0, 1.0)), "Microstep", "MSI MAG271C");
        external.physical_size = Some([600, 340]);
        outputs.insert("DP-1".into(), external);

        let fp_external = MonitorFingerprint {
            x: 5,
            y: 5,
            make: Some("Microstep".into()),
            model: Some("MSI MAG271C".into()),
            w_mm: 600,
            h_mm: 340,
        };
        for _ in 0..10 {
            assert_eq!(
                match_output_for_monitor(&fp_external, &outputs, false),
                Some("DP-1".to_string())
            );
        }

        let fp_laptop = MonitorFingerprint {
            x: 5,
            y: 5,
            make: Some("LG Display".into()),
            model: Some("0x05FE".into()),
            w_mm: 340,
            h_mm: 190,
        };
        for _ in 0..10 {
            assert_eq!(
                match_output_for_monitor(&fp_laptop, &outputs, false),
                Some("eDP-1".to_string())
            );
        }
    }

    #[test]
    fn test_match_output_identical_twins_deterministic() {
        // Truly indistinguishable outputs (same geometry, size, make, model):
        // sorted-first, stable across calls — never HashMap-order flap.
        let mut outputs = std::collections::HashMap::new();
        for name in ["DP-2", "DP-1"] {
            let mut out = test_output(Some(logical_output(0, 0, 1.0)), "Same", "Same");
            out.physical_size = Some([600, 340]);
            outputs.insert(name.into(), out);
        }
        let fp = MonitorFingerprint {
            x: 5,
            y: 5,
            make: Some("Same".into()),
            model: Some("Same".into()),
            w_mm: 600,
            h_mm: 340,
        };
        for _ in 0..10 {
            assert_eq!(match_output_for_monitor(&fp, &outputs, false), Some("DP-1".to_string()));
        }
    }
}
