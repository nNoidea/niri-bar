mod pulse;

use gtk::Orientation;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::{resolve_level, VolumeConfig};
use crate::modules::{poll_backoff, BarModule, ModuleCore, ModuleState, MouseButton, ScrollDirection};
pub use pulse::PulseAction;
use pulse::PulseSession;

const WRITE_ECHO_DEBOUNCE: Duration = Duration::from_millis(180);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkType {
    Speaker,
    Headphones,
    Headset,
    Bluetooth,
    Hdmi,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkInfo {
    pub name: String,
    pub description: String,
    pub sink_type: SinkType,
}

pub struct VolumeModule {
    config: VolumeConfig,
    current_vol: Arc<AtomicU32>,
    is_muted: Arc<AtomicBool>,
    current_sink: Arc<Mutex<SinkInfo>>,
    cmd_tx: async_channel::Sender<PulseAction>,
    #[cfg(test)]
    cmd_rx: async_channel::Receiver<PulseAction>,
    core: ModuleCore,
}

impl VolumeModule {
    pub fn new(config: VolumeConfig) -> Self {
        Self::new_with_worker(config, !cfg!(test))
    }

    pub fn new_with_worker(config: VolumeConfig, start_worker: bool) -> Self {
        let core = ModuleCore::new();
        let current_vol = Arc::new(AtomicU32::new(50));
        let is_muted = Arc::new(AtomicBool::new(false));
        let current_sink = Arc::new(Mutex::new(SinkInfo {
            name: "@DEFAULT_SINK@".to_string(),
            description: "Default Output".to_string(),
            sink_type: SinkType::Speaker,
        }));
        let (cmd_tx, cmd_rx) = async_channel::bounded(32);

        if start_worker {
            let cur_vol = Arc::clone(&current_vol);
            let cur_muted = Arc::clone(&is_muted);
            let cur_sink = Arc::clone(&current_sink);
            let core_worker = core.clone();
            let cfg = config.clone();
            #[cfg(test)]
            let worker_rx = cmd_rx.clone();
            #[cfg(not(test))]
            let worker_rx = cmd_rx;

            crate::runtime::spawn(async move {
                let mut fail_streak: u32 = 0;
                while !core_worker.is_stopped() {
                    let stream_res = match Self::resolve_pulse_socket() {
                        Some(path) => tokio::net::UnixStream::connect(path).await.map_err(|e| e.to_string()),
                        None => Err("PulseAudio socket not found".to_string()),
                    };

                    match stream_res {
                        Ok(stream) => {
                            fail_streak = 0;
                            Self::run_pulse_session(
                                stream,
                                &worker_rx,
                                &core_worker,
                                &cfg,
                                &cur_vol,
                                &cur_muted,
                                &cur_sink,
                            )
                            .await;
                        }
                        Err(e) => {
                            if fail_streak == 0 {
                                log_debug!("volume", "PulseAudio unavailable ({e}); waiting for server");
                            }
                            fail_streak = fail_streak.saturating_add(1);
                        }
                    }

                    if core_worker.is_stopped() {
                        break;
                    }
                    let backoff = poll_backoff(Duration::from_millis(250), fail_streak, Duration::from_secs(5));
                    if core_worker.wait_timeout_async(backoff).await {
                        break;
                    }
                }
            });
        }

        Self {
            config,
            current_vol,
            is_muted,
            current_sink,
            cmd_tx,
            #[cfg(test)]
            cmd_rx,
            core,
        }
    }

    async fn run_pulse_session(
        stream: tokio::net::UnixStream,
        cmd_rx: &async_channel::Receiver<PulseAction>,
        core: &ModuleCore,
        cfg: &VolumeConfig,
        cur_vol: &Arc<AtomicU32>,
        cur_muted: &Arc<AtomicBool>,
        cur_sink: &Arc<Mutex<SinkInfo>>,
    ) {
        let mut session = match PulseSession::connect(stream).await {
            Ok(s) => s,
            Err(e) => {
                log_debug!("volume", "PulseAudio handshake failed: {e}");
                return;
            }
        };

        if let Ok((vol, muted, sink)) = session.fetch_state().await {
            cur_vol.store(vol, Ordering::Relaxed);
            cur_muted.store(muted, Ordering::Relaxed);
            *crate::util::lock(cur_sink) = sink.clone();
            core.broadcast(|orient| Self::format_state_with_config(cfg, vol, muted, &sink, orient));
        }

        let mut suppress_until: Option<tokio::time::Instant> = None;
        let mut pending_refresh = false;

        'session_loop: loop {
            if core.is_stopped() {
                break;
            }

            if session.pending_event {
                session.pending_event = false;
                if suppress_until.is_some_and(|t| tokio::time::Instant::now() < t) {
                    pending_refresh = true;
                } else if !Self::refresh_and_apply(&mut session, core, cfg, cur_vol, cur_muted, cur_sink, true).await {
                    break;
                }
                continue;
            }

            let debounce_sleep = async {
                if let Some(deadline) = suppress_until {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };

            tokio::select! {
                biased;

                () = core.wait_stopped() => break,

                action_res = cmd_rx.recv() => {
                    let Ok(first_action) = action_res else { break };
                    let mut cycled_sink = matches!(first_action, PulseAction::CycleSink);
                    if let Err(e) = session.handle_action(first_action).await {
                        log_debug!("volume", "PulseAudio command failed: {e}");
                        break;
                    }
                    while let Ok(more) = cmd_rx.try_recv() {
                        cycled_sink |= matches!(more, PulseAction::CycleSink);
                        if let Err(e) = session.handle_action(more).await {
                            log_debug!("volume", "PulseAudio command failed: {e}");
                            break 'session_loop;
                        }
                    }
                    if cycled_sink {
                        suppress_until = None;
                        pending_refresh = false;
                        if !Self::refresh_and_apply(&mut session, core, cfg, cur_vol, cur_muted, cur_sink, false).await {
                            break;
                        }
                    } else {
                        // In pipewire-pulse, SetSinkMute/SetSinkVolume ACK before PipeWire
                        // finishes updating the node in the graph. Suppress immediate readback
                        // so stale pre-mutation state never clobbers rapid click/scroll updates.
                        suppress_until = Some(tokio::time::Instant::now() + WRITE_ECHO_DEBOUNCE);
                    }
                }

                () = debounce_sleep, if suppress_until.is_some() => {
                    suppress_until = None;
                    if pending_refresh {
                        pending_refresh = false;
                        if !Self::refresh_and_apply(&mut session, core, cfg, cur_vol, cur_muted, cur_sink, false).await {
                            break;
                        }
                    }
                }

                frame_res = session.read_next_frame() => {
                    match frame_res {
                        Ok(()) => {
                            if suppress_until.is_some_and(|t| tokio::time::Instant::now() < t) {
                                pending_refresh = true;
                            } else if !Self::refresh_and_apply(&mut session, core, cfg, cur_vol, cur_muted, cur_sink, true).await {
                                break;
                            }
                        }
                        Err(e) => {
                            if !core.is_stopped() {
                                log_debug!("volume", "PulseAudio stream closed ({e})");
                            }
                            break;
                        }
                    }
                }
            }
        }
    }

    async fn refresh_and_apply(
        session: &mut PulseSession,
        core: &ModuleCore,
        cfg: &VolumeConfig,
        cur_vol: &Arc<AtomicU32>,
        cur_muted: &Arc<AtomicBool>,
        cur_sink: &Arc<Mutex<SinkInfo>>,
        auto_unmute_on_vol_change: bool,
    ) -> bool {
        let (vol, mut muted, new_sink) = match session.fetch_state().await {
            Ok(state) => state,
            Err(e) => {
                log_debug!("volume", "Failed to read PulseAudio state: {e}");
                return false;
            }
        };

        let old_vol = cur_vol.load(Ordering::Relaxed);
        let old_muted = cur_muted.load(Ordering::Relaxed);
        let sink_changed = {
            let guard = crate::util::lock(cur_sink);
            *guard != new_sink
        };

        if auto_unmute_on_vol_change && old_muted && muted && vol != old_vol && !sink_changed {
            let _ = session.set_default_mute(false).await;
            muted = false;
        }

        if vol != old_vol || muted != old_muted || sink_changed {
            cur_vol.store(vol, Ordering::Relaxed);
            cur_muted.store(muted, Ordering::Relaxed);
            *crate::util::lock(cur_sink) = new_sink.clone();
            core.broadcast(|orient| Self::format_state_with_config(cfg, vol, muted, &new_sink, orient));
        }
        true
    }

    pub fn compute_scroll_volume(current_vol: u32, step: u32, max_volume: u32, direction: ScrollDirection) -> u32 {
        let step = if step == 0 { 10 } else { step };
        let max_vol = if max_volume == 0 { 100 } else { max_volume };
        match direction {
            ScrollDirection::Up => current_vol.saturating_add(step).min(max_vol),
            ScrollDirection::Down => current_vol.saturating_sub(step),
        }
    }

    pub fn classify_sink(
        name: &str,
        desc: &str,
        form_factor: Option<&str>,
        icon_name: Option<&str>,
        bus: Option<&str>,
        active_port: Option<&str>,
    ) -> SinkType {
        let name_lower = name.to_lowercase();
        let desc_lower = desc.to_lowercase();
        let ff_lower = form_factor.unwrap_or("").to_lowercase();
        let icon_lower = icon_name.unwrap_or("").to_lowercase();
        let bus_lower = bus.unwrap_or("").to_lowercase();
        let port_lower = active_port.unwrap_or("").to_lowercase();

        let is_bluetooth =
            bus_lower == "bluetooth" || name_lower.starts_with("bluez_") || icon_lower.contains("bluetooth");

        let is_headset = ff_lower == "headset"
            || icon_lower.contains("headset")
            || port_lower.contains("headset")
            || desc_lower.contains("headset");

        let is_headphones = ff_lower == "headphone"
            || icon_lower.contains("headphone")
            || port_lower.contains("headphone")
            || desc_lower.contains("headphone")
            || desc_lower.contains("earphone")
            || desc_lower.contains("buds")
            || desc_lower.contains("airpod")
            || desc_lower.contains("wh-1000");

        let is_hdmi = name_lower.contains("hdmi")
            || desc_lower.contains("hdmi")
            || desc_lower.contains("displayport")
            || port_lower.contains("hdmi");

        if is_bluetooth {
            SinkType::Bluetooth
        } else if is_headphones {
            SinkType::Headphones
        } else if is_headset {
            SinkType::Headset
        } else if is_hdmi {
            SinkType::Hdmi
        } else {
            SinkType::Speaker
        }
    }

    pub fn adapt_icon_for_sink(formatted: &str, _volume: u32, _is_muted: bool, sink_type: SinkType) -> String {
        let speaker_icons = ["", "", ""];
        let target_icon = match sink_type {
            SinkType::Bluetooth | SinkType::Headphones | SinkType::Headset => "󰋋",
            SinkType::Hdmi => "󰍹",
            SinkType::Speaker => return formatted.to_string(),
        };

        for ic in speaker_icons {
            if formatted.starts_with(ic) {
                return formatted.replacen(ic, target_icon, 1);
            }
        }

        formatted.to_string()
    }

    pub fn format_state_with_config(
        config: &VolumeConfig,
        volume: u32,
        is_muted: bool,
        sink: &SinkInfo,
        _orientation: Orientation,
    ) -> ModuleState {
        let mut css_classes = Vec::new();
        let special = if is_muted { Some("muted") } else { None };

        let is_bt = sink.sink_type == SinkType::Bluetooth;
        let is_hp = matches!(sink.sink_type, SinkType::Headphones | SinkType::Headset);
        let is_hdmi = sink.sink_type == SinkType::Hdmi;

        let custom_levels = config.get_levels_for_sink(is_bt, is_hp, is_hdmi);
        let levels_map = custom_levels.unwrap_or(&config.levels);

        let (mut formatted, threshold_opt) = resolve_level(levels_map, volume, special);

        if custom_levels.is_none() {
            formatted = Self::adapt_icon_for_sink(&formatted, volume, is_muted, sink.sink_type);
        }

        if is_muted || volume == 0 {
            css_classes.push("muted".to_string());
        }
        if let Some(t) = threshold_opt {
            css_classes.push(format!("level-{t}"));
        }

        match sink.sink_type {
            SinkType::Speaker => css_classes.push("sink-speaker".to_string()),
            SinkType::Headphones => {
                css_classes.push("sink-headphones".to_string());
            }
            SinkType::Headset => {
                css_classes.push("sink-headset".to_string());
                css_classes.push("sink-headphones".to_string());
            }
            SinkType::Bluetooth => {
                css_classes.push("sink-bluetooth".to_string());
                css_classes.push("sink-headphones".to_string());
            }
            SinkType::Hdmi => css_classes.push("sink-hdmi".to_string()),
        }

        let type_label = match sink.sink_type {
            SinkType::Speaker => "Speaker",
            SinkType::Headphones => "Headphones",
            SinkType::Headset => "Headset",
            SinkType::Bluetooth => "Bluetooth",
            SinkType::Hdmi => "HDMI",
        };

        let tooltip = format!("[{type_label}] {desc}", desc = sink.description);

        ModuleState {
            icon: None,
            text: Some(formatted),
            tooltip: Some(tooltip),
            css_classes,
        }
    }

    fn dispatch_immediate_update(&self) {
        let vol = self.current_vol.load(Ordering::Relaxed);
        let muted = self.is_muted.load(Ordering::Relaxed);
        let sink = crate::util::lock(&self.current_sink).clone();
        self.core
            .broadcast(|orient| Self::format_state_with_config(&self.config, vol, muted, &sink, orient));
    }
}

impl BarModule for VolumeModule {
    fn name(&self) -> &'static str {
        "volume"
    }

    fn current_state(&self, orientation: Orientation) -> ModuleState {
        let vol = self.current_vol.load(Ordering::Relaxed);
        let muted = self.is_muted.load(Ordering::Relaxed);
        let sink = crate::util::lock(&self.current_sink).clone();
        Self::format_state_with_config(&self.config, vol, muted, &sink, orientation)
    }

    fn core(&self) -> &ModuleCore {
        &self.core
    }

    fn handle_click(&self, button: MouseButton) {
        if self.config.click.try_run(button) {
            return;
        }
        match button {
            MouseButton::Left => {
                let cur_vol = self.current_vol.load(Ordering::Relaxed);
                let is_muted = self.is_muted.load(Ordering::Relaxed);

                if cur_vol == 0 {
                    let step = if self.config.step == 0 { 10 } else { self.config.step };
                    let max_vol = if self.config.max_volume == 0 {
                        100
                    } else {
                        self.config.max_volume
                    };
                    let target_vol = step.min(max_vol);
                    self.current_vol.store(target_vol, Ordering::Relaxed);
                    self.is_muted.store(false, Ordering::Relaxed);
                    self.dispatch_immediate_update();
                    let _ = self.cmd_tx.try_send(PulseAction::SetVolumeAndUnmute(target_vol));
                } else {
                    let new_muted = !is_muted;
                    self.is_muted.store(new_muted, Ordering::Relaxed);
                    self.dispatch_immediate_update();
                    let _ = self.cmd_tx.try_send(PulseAction::SetMute(new_muted));
                }
            }
            MouseButton::Right => {
                let _ = self.cmd_tx.try_send(PulseAction::CycleSink);
            }
            MouseButton::Middle => {}
        }
    }

    fn handle_scroll(&self, direction: ScrollDirection) {
        let cur_vol = self.current_vol.load(Ordering::Relaxed);
        let target_vol = Self::compute_scroll_volume(cur_vol, self.config.step, self.config.max_volume, direction);
        self.current_vol.store(target_vol, Ordering::Relaxed);
        self.is_muted.store(false, Ordering::Relaxed);
        self.dispatch_immediate_update();
        let _ = self.cmd_tx.try_send(PulseAction::SetVolumeAndUnmute(target_vol));
    }

    fn is_clickable(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulseaudio::protocol;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    impl SinkInfo {
        pub fn speaker() -> Self {
            Self {
                name: "default_speaker".to_string(),
                description: "Speaker".to_string(),
                sink_type: SinkType::Speaker,
            }
        }

        pub fn bluetooth_headset(name: &str, desc: &str) -> Self {
            Self {
                name: name.to_string(),
                description: desc.to_string(),
                sink_type: SinkType::Bluetooth,
            }
        }

        pub fn headphones(name: &str, desc: &str) -> Self {
            Self {
                name: name.to_string(),
                description: desc.to_string(),
                sink_type: SinkType::Headphones,
            }
        }

        pub fn hdmi(name: &str, desc: &str) -> Self {
            Self {
                name: name.to_string(),
                description: desc.to_string(),
                sink_type: SinkType::Hdmi,
            }
        }
    }

    #[test]
    fn test_volume_format_and_levels() {
        let config = VolumeConfig::default();
        let speaker = SinkInfo::speaker();

        // 0%
        let s0 = VolumeModule::format_state_with_config(&config, 0, false, &speaker, Orientation::Vertical);
        assert_eq!(s0.text.as_deref(), Some("\n0%"));
        assert!(s0.css_classes.contains(&"level-0".to_string()));
        assert!(s0.css_classes.contains(&"muted".to_string()));
        assert!(s0.css_classes.contains(&"sink-speaker".to_string()));

        // 20% -> level-1 (1 wave), NOT muted
        let s20 = VolumeModule::format_state_with_config(&config, 20, false, &speaker, Orientation::Vertical);
        assert_eq!(s20.text.as_deref(), Some("\n20%"));
        assert!(s20.css_classes.contains(&"level-1".to_string()));
        assert!(!s20.css_classes.contains(&"muted".to_string()));

        // 50% -> level-1 (1 wave)
        let s50 = VolumeModule::format_state_with_config(&config, 50, false, &speaker, Orientation::Vertical);
        assert_eq!(s50.text.as_deref(), Some("\n50%"));
        assert!(s50.css_classes.contains(&"level-1".to_string()));

        // 80% -> level-1 (1 wave)
        let s80 = VolumeModule::format_state_with_config(&config, 80, false, &speaker, Orientation::Vertical);
        assert_eq!(s80.text.as_deref(), Some("\n80%"));
        assert!(s80.css_classes.contains(&"level-1".to_string()));

        // 100% -> level-100 (max volume icon only at exactly 100%)
        let s100 = VolumeModule::format_state_with_config(&config, 100, false, &speaker, Orientation::Vertical);
        assert_eq!(s100.text.as_deref(), Some("\n100%"));
        assert!(s100.css_classes.contains(&"level-100".to_string()));

        // Muted
        let s_mut = VolumeModule::format_state_with_config(&config, 80, true, &speaker, Orientation::Vertical);
        assert_eq!(s_mut.text.as_deref(), Some("\nM"));
        assert!(s_mut.css_classes.contains(&"muted".to_string()));
        assert_eq!(s_mut.tooltip.as_deref(), Some("[Speaker] Speaker"));
    }

    #[test]
    fn test_volume_output_icons_and_classes() {
        let config = VolumeConfig::default();

        // Bluetooth headset
        let bt = SinkInfo::bluetooth_headset("bluez_sink", "Pixel Buds Pro 2");
        let s_bt = VolumeModule::format_state_with_config(&config, 80, false, &bt, Orientation::Vertical);
        assert_eq!(s_bt.text.as_deref(), Some("󰋋\n80%"));
        assert!(s_bt.css_classes.contains(&"sink-bluetooth".to_string()));
        assert!(s_bt.css_classes.contains(&"sink-headphones".to_string()));
        assert_eq!(s_bt.tooltip.as_deref(), Some("[Bluetooth] Pixel Buds Pro 2"));

        // Bluetooth muted
        let s_bt_mut = VolumeModule::format_state_with_config(&config, 80, true, &bt, Orientation::Vertical);
        assert_eq!(s_bt_mut.text.as_deref(), Some("󰋋\nM"));

        // Wired Headphones
        let hp = SinkInfo::headphones("alsa_hp", "Audio Technica M50x");
        let s_hp = VolumeModule::format_state_with_config(&config, 65, false, &hp, Orientation::Vertical);
        assert_eq!(s_hp.text.as_deref(), Some("󰋋\n65%"));
        assert!(s_hp.css_classes.contains(&"sink-headphones".to_string()));

        // HDMI
        let hdmi = SinkInfo::hdmi("alsa_hdmi", "LG TV HDMI");
        let s_hdmi = VolumeModule::format_state_with_config(&config, 50, false, &hdmi, Orientation::Vertical);
        assert_eq!(s_hdmi.text.as_deref(), Some("󰍹\n50%"));
        assert!(s_hdmi.css_classes.contains(&"sink-hdmi".to_string()));
    }

    #[test]
    fn test_volume_custom_output_levels() {
        let mut custom_bt = std::collections::HashMap::new();
        custom_bt.insert("0".to_string(), "󰂰\n%v%".to_string());
        custom_bt.insert("1".to_string(), "󰂯\n%v%".to_string());
        custom_bt.insert("muted".to_string(), "󰂲\nM".to_string());

        let config = VolumeConfig {
            levels_bluetooth: Some(custom_bt),
            ..Default::default()
        };

        let bt = SinkInfo::bluetooth_headset("bluez_sink", "Pixel Buds Pro 2");
        let s_bt = VolumeModule::format_state_with_config(&config, 75, false, &bt, Orientation::Vertical);
        assert_eq!(s_bt.text.as_deref(), Some("󰂯\n75%"));

        let s_bt_mut = VolumeModule::format_state_with_config(&config, 75, true, &bt, Orientation::Vertical);
        assert_eq!(s_bt_mut.text.as_deref(), Some("󰂲\nM"));
    }

    #[test]
    fn test_volume_click_and_scroll_behavior() {
        let config = VolumeConfig::default();
        let module = VolumeModule::new(config);

        // When volume is 0, clicking left should set volume to step (10) and un-mute
        module.current_vol.store(0, Ordering::Relaxed);
        module.is_muted.store(true, Ordering::Relaxed);
        module.handle_click(MouseButton::Left);
        assert_eq!(module.current_vol.load(Ordering::Relaxed), 10);
        assert!(!module.is_muted.load(Ordering::Relaxed));
        assert_eq!(module.cmd_rx.try_recv().ok(), Some(PulseAction::SetVolumeAndUnmute(10)));

        // When volume is 50, clicking left should toggle mute while preserving volume 50
        module.current_vol.store(50, Ordering::Relaxed);
        module.is_muted.store(false, Ordering::Relaxed);
        module.handle_click(MouseButton::Left);
        assert_eq!(module.current_vol.load(Ordering::Relaxed), 50);
        assert!(module.is_muted.load(Ordering::Relaxed));
        assert_eq!(module.cmd_rx.try_recv().ok(), Some(PulseAction::SetMute(true)));

        // Clicking left again un-mutes and retains volume 50
        module.handle_click(MouseButton::Left);
        assert_eq!(module.current_vol.load(Ordering::Relaxed), 50);
        assert!(!module.is_muted.load(Ordering::Relaxed));
        assert_eq!(module.cmd_rx.try_recv().ok(), Some(PulseAction::SetMute(false)));

        // Right click cycles sink
        module.handle_click(MouseButton::Right);
        assert_eq!(module.cmd_rx.try_recv().ok(), Some(PulseAction::CycleSink));

        // Scroll up increases volume and unmutes
        module.is_muted.store(true, Ordering::Relaxed);
        module.handle_scroll(ScrollDirection::Up);
        assert_eq!(module.current_vol.load(Ordering::Relaxed), 60);
        assert!(!module.is_muted.load(Ordering::Relaxed));
        assert_eq!(module.cmd_rx.try_recv().ok(), Some(PulseAction::SetVolumeAndUnmute(60)));

        // Middle click with default config (None) is a no-op
        module.handle_click(MouseButton::Middle);
        assert!(module.cmd_rx.try_recv().is_err());
    }

    #[test]
    fn test_compute_scroll_volume() {
        // Default step (10) and default max (100)
        assert_eq!(VolumeModule::compute_scroll_volume(50, 0, 0, ScrollDirection::Up), 60);
        assert_eq!(VolumeModule::compute_scroll_volume(50, 0, 0, ScrollDirection::Down), 40);
        assert_eq!(VolumeModule::compute_scroll_volume(95, 0, 0, ScrollDirection::Up), 100);
        assert_eq!(VolumeModule::compute_scroll_volume(5, 0, 0, ScrollDirection::Down), 0);

        // Custom step (5) and custom max (150)
        assert_eq!(
            VolumeModule::compute_scroll_volume(100, 5, 150, ScrollDirection::Up),
            105
        );
        assert_eq!(
            VolumeModule::compute_scroll_volume(148, 5, 150, ScrollDirection::Up),
            150
        );
        assert_eq!(
            VolumeModule::compute_scroll_volume(100, 5, 150, ScrollDirection::Down),
            95
        );
    }

    #[test]
    fn test_run_pulse_session_spam_mute_does_not_clobber_optimistic_state() {
        crate::runtime::handle().block_on(async move {
            let (client_sock, mut server_sock) =
                tokio::net::UnixStream::pair().expect("UnixStream::pair must succeed in tests");

            // Mock a pipewire-pulse server where GetSinkInfoList lags behind SetSinkMute ACKs
            let server_task = crate::runtime::spawn(async move {
                let ver = protocol::MAX_VERSION;
                let mut hdr = [0u8; protocol::DESCRIPTOR_SIZE];
                let mut buf = Vec::new();
                let mut out = Vec::new();

                while server_sock.read_exact(&mut hdr).await.is_ok() {
                    let desc = protocol::read_descriptor(&mut &hdr[..]).expect("parse desc");
                    buf.resize(protocol::DESCRIPTOR_SIZE + desc.length as usize, 0);
                    buf[..protocol::DESCRIPTOR_SIZE].copy_from_slice(&hdr);
                    if server_sock
                        .read_exact(&mut buf[protocol::DESCRIPTOR_SIZE..])
                        .await
                        .is_err()
                    {
                        break;
                    }
                    let (seq, cmd) = protocol::read_command_message(&mut &buf[..], ver).expect("parse cmd");
                    out.clear();
                    match cmd {
                        protocol::Command::Auth(_) => {
                            protocol::write_reply_message(
                                &mut out,
                                seq,
                                &protocol::AuthReply {
                                    version: ver,
                                    use_shm: false,
                                    use_memfd: false,
                                },
                                ver,
                            )
                            .expect("auth reply");
                        }
                        protocol::Command::SetClientName(_) => {
                            protocol::write_reply_message(
                                &mut out,
                                seq,
                                &protocol::SetClientNameReply { client_id: 1 },
                                ver,
                            )
                            .expect("name reply");
                        }
                        protocol::Command::Subscribe(_)
                        | protocol::Command::SetSinkMute(_)
                        | protocol::Command::SetSinkVolume(_)
                        | protocol::Command::SetDefaultSink(_) => {
                            protocol::write_ack_message(&mut out, seq).expect("ack");
                        }
                        protocol::Command::GetServerInfo => {
                            let srv = protocol::ServerInfo {
                                default_sink_name: Some(c"alsa_output.speaker".to_owned()),
                                ..Default::default()
                            };
                            protocol::write_reply_message(&mut out, seq, &srv, ver).expect("srv reply");
                        }
                        protocol::Command::GetSinkInfoList => {
                            // Simulate pipewire-pulse returning stale muted=false right after SetSinkMute ACK
                            let mut sink = protocol::SinkInfo::new_dummy(1);
                            sink.name = c"alsa_output.speaker".to_owned();
                            sink.cvolume = VolumeModule::pct_to_channel_volume(50, 2);
                            sink.muted = false;
                            let sinks: protocol::SinkInfoList = vec![sink];
                            protocol::write_reply_message(&mut out, seq, &sinks, ver).expect("sinks reply");
                        }
                        _ => break,
                    }
                    if server_sock.write_all(&out).await.is_err() {
                        break;
                    }
                }
            });

            let module = VolumeModule::new_with_worker(VolumeConfig::default(), false);
            let cmd_rx = module.cmd_rx.clone();
            let core = module.core.clone();
            let cfg = module.config.clone();
            let cur_vol = Arc::clone(&module.current_vol);
            let cur_muted = Arc::clone(&module.is_muted);
            let cur_sink = Arc::clone(&module.current_sink);

            let session_task = crate::runtime::spawn(async move {
                VolumeModule::run_pulse_session(client_sock, &cmd_rx, &core, &cfg, &cur_vol, &cur_muted, &cur_sink)
                    .await;
            });

            // Wait briefly for initial handshake + state fetch
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(!module.is_muted.load(Ordering::Relaxed));

            // Click 1: should mute -> true, and NOT be reverted to false by a premature post-ACK GetSinkInfoList!
            module.handle_click(MouseButton::Left);
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(
                module.is_muted.load(Ordering::Relaxed),
                "Optimistic mute state was clobbered by stale post-ACK GetSinkInfoList!"
            );

            // Click 2: should unmute -> false
            module.handle_click(MouseButton::Left);
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(!module.is_muted.load(Ordering::Relaxed));

            // Click 3: should mute -> true
            module.handle_click(MouseButton::Left);
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(module.is_muted.load(Ordering::Relaxed));

            module.shutdown();
            let _ = session_task.await;
            let _ = server_task.await;
        });
    }

    #[test]
    fn test_volume_overamplification_format() {
        let config = VolumeConfig {
            max_volume: 150,
            ..Default::default()
        };
        let speaker = SinkInfo::speaker();
        let s = VolumeModule::format_state_with_config(&config, 150, false, &speaker, Orientation::Vertical);
        assert_eq!(s.text.as_deref(), Some("\n150%"));
        assert!(s.css_classes.contains(&"level-100".to_string()));
        assert_eq!(s.tooltip.as_deref(), Some("[Speaker] Speaker"));
    }

    #[test]
    fn test_adapt_icon_for_all_sink_types() {
        // Speaker keeps original icons
        assert_eq!(
            VolumeModule::adapt_icon_for_sink("\n50%", 50, false, SinkType::Speaker),
            "\n50%"
        );
        assert_eq!(
            VolumeModule::adapt_icon_for_sink("\nM", 50, true, SinkType::Speaker),
            "\nM"
        );

        // Headset / Bluetooth / Headphones adapt to 󰋋
        assert_eq!(
            VolumeModule::adapt_icon_for_sink("\n50%", 50, false, SinkType::Headset),
            "󰋋\n50%"
        );
        assert_eq!(
            VolumeModule::adapt_icon_for_sink("\n100%", 100, false, SinkType::Bluetooth),
            "󰋋\n100%"
        );
        assert_eq!(
            VolumeModule::adapt_icon_for_sink("\nM", 50, true, SinkType::Headphones),
            "󰋋\nM"
        );

        // HDMI adapts to 󰍹
        assert_eq!(
            VolumeModule::adapt_icon_for_sink("\n50%", 50, false, SinkType::Hdmi),
            "󰍹\n50%"
        );
    }

    #[test]
    fn test_classify_sink() {
        let n = VolumeModule::classify_sink;
        // Plain speaker is the default.
        assert_eq!(
            n("alsa_output", "Ryzen HD Audio Speaker", None, None, None, None),
            SinkType::Speaker
        );
        // Bluetooth via bus, bluez_ prefix, or icon.
        assert_eq!(
            n("bluez_sink.01", "Speaker", None, None, None, None),
            SinkType::Bluetooth
        );
        assert_eq!(
            n(
                "alsa_bt",
                "Speaker",
                None,
                Some("audio-bluetooth"),
                Some("bluetooth"),
                None
            ),
            SinkType::Bluetooth
        );
        // Bluetooth headphones keep the Bluetooth type (not plain Headphones).
        assert_eq!(
            n("bluez_sink.01", "WH-1000XM4", None, None, Some("bluetooth"), None),
            SinkType::Bluetooth
        );
        // Wired headphones via form factor, desc keywords, or port.
        assert_eq!(
            n("alsa_hp", "Headphones", Some("headphone"), None, None, None),
            SinkType::Headphones
        );
        assert_eq!(
            n("alsa_hp", "Pixel Buds Pro", None, None, None, None),
            SinkType::Headphones
        );
        assert_eq!(n("alsa_hp", "AirPods", None, None, None, None), SinkType::Headphones);
        assert_eq!(
            n("alsa_hp", "Output", None, None, None, Some("headphone")),
            SinkType::Headphones
        );
        // Headset without headphone markers.
        assert_eq!(
            n("usb_dongle", "Jabra Headset", None, None, None, None),
            SinkType::Headset
        );
        // HDMI via name, desc, or DisplayPort alias.
        assert_eq!(n("alsa_hdmi", "Output", None, None, None, None), SinkType::Hdmi);
        assert_eq!(
            n("alsa_dp", "DisplayPort Monitor", None, None, None, None),
            SinkType::Hdmi
        );
    }

    #[test]
    fn test_volume_shutdown_signals_stop() {
        let module = VolumeModule::new_with_worker(VolumeConfig::default(), false);
        assert!(!module.is_stopped());
        module.shutdown();
        assert!(module.is_stopped());
    }
}
