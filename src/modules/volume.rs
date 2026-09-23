use gtk::Orientation;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::config::{resolve_level, VolumeConfig};
use crate::modules::{
    resolve_helper, spawn_command, truncate_log, BarModule, ModuleState, ModuleSubscribers, MouseButton,
    ScrollDirection,
};

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
    pub is_bluetooth: bool,
}

pub struct VolumeModule {
    config: VolumeConfig,
    subscribers: ModuleSubscribers,
    current_vol: Arc<AtomicU32>,
    is_muted: Arc<AtomicBool>,
    current_sink: Arc<Mutex<SinkInfo>>,
}

impl VolumeModule {
    pub fn new(config: VolumeConfig) -> Self {
        // Never fork+exec on the caller (GTK main) thread. Start with a
        // clearly-stale placeholder; the worker below re-reads immediately
        // and broadcasts the real state.
        let subscribers: ModuleSubscribers = Arc::new(Mutex::new(Vec::new()));
        let current_vol = Arc::new(AtomicU32::new(50));
        let is_muted = Arc::new(AtomicBool::new(false));
        let current_sink = Arc::new(Mutex::new(SinkInfo {
            name: "@DEFAULT_AUDIO_SINK@".to_string(),
            description: "Default Output".to_string(),
            sink_type: SinkType::Speaker,
            is_bluetooth: false,
        }));

        let cur_vol = Arc::clone(&current_vol);
        let cur_muted = Arc::clone(&is_muted);
        let cur_sink = Arc::clone(&current_sink);
        let subs = Arc::clone(&subscribers);
        let cfg = config.clone();

        // Event-driven listener using native PipeWire `pw-mon` (0% CPU at idle, instant reaction).
        // Supervised: pw-mon session → backoff polling → re-exec, forever.
        thread::spawn(move || {
            // Initial truth (off GTK thread): replaces the stale placeholder.
            {
                let (vol, muted) = VolumeModule::read_system_volume();
                let new_sink = VolumeModule::read_current_sink();
                cur_vol.store(vol, Ordering::Relaxed);
                cur_muted.store(muted, Ordering::Relaxed);
                *crate::util::lock(&cur_sink) = new_sink.clone();
                crate::modules::broadcast(&subs, |orient| {
                    VolumeModule::format_state_with_config(&cfg, vol, muted, &new_sink, orient)
                });
            }
            let apply = |vol: u32, muted: bool, new_sink: &SinkInfo| {
                let old_vol = cur_vol.load(Ordering::Relaxed);
                let old_muted = cur_muted.load(Ordering::Relaxed);
                let sink_changed = {
                    let guard = crate::util::lock(&cur_sink);
                    *guard != *new_sink
                };
                let mut muted = muted;

                // If volume changed while muted (e.g. media keys), auto-unmute sink
                if old_muted && vol != old_vol {
                    spawn_command("wpctl set-mute @DEFAULT_AUDIO_SINK@ 0");
                    muted = false;
                }

                if vol != old_vol || muted != old_muted || sink_changed {
                    cur_vol.store(vol, Ordering::Relaxed);
                    cur_muted.store(muted, Ordering::Relaxed);
                    {
                        let mut guard = crate::util::lock(&cur_sink);
                        *guard = new_sink.clone();
                    }

                    crate::modules::broadcast(&subs, |orient| {
                        VolumeModule::format_state_with_config(&cfg, vol, muted, new_sink, orient)
                    });
                }
            };

            // Supervise pw-mon forever: event session → backoff polling → re-exec.
            loop {
                let spawned_child = VolumeModule::spawn_pw_mon();

                if let Some(mut child) = spawned_child {
                    if let Some(stdout) = child.stdout.take() {
                        let reader = BufReader::new(stdout);
                        for line in reader.lines() {
                            let l = match line {
                                Ok(l) => l,
                                Err(e) => {
                                    // Transient read error: don't kill the event
                                    // loop permanently; log and keep listening.
                                    log_warn!("volume", "pw-mon read error: {e}");
                                    continue;
                                }
                            };
                            if VolumeModule::is_volume_event_line(&l) {
                                let (vol, muted) = VolumeModule::read_system_volume();
                                let new_sink = VolumeModule::read_current_sink();
                                apply(vol, muted, &new_sink);
                            }
                        }
                    }
                    // `pw-mon` exited (or stdout closed): reap, then polling.
                    if let Err(e) = child.kill() {
                        log_debug!("volume", "pw-mon kill during teardown: {e}");
                    }
                    match child.wait() {
                        Ok(status) => log_debug!("volume", "pw-mon exited with {status}; using polling fallback"),
                        Err(e) => log_warn!("volume", "Failed waiting on pw-mon: {e}"),
                    }
                } else {
                    log_debug!("volume", "pw-mon unavailable; using polling fallback");
                }

                // Fallback polling with backoff while PipeWire is down
                // (250ms → 5s). Returns after ~120 polls so the outer
                // supervisor re-execs pw-mon to resume event mode.
                let mut fail_streak: u32 = 0;
                for _ in 0..120 {
                    let ok_before = VolumeModule::probe_audio_ok();
                    let (vol, muted) = VolumeModule::read_system_volume();
                    let new_sink = VolumeModule::read_current_sink();
                    apply(vol, muted, &new_sink);
                    let healthy = ok_before && new_sink.name != "@DEFAULT_AUDIO_SINK@";
                    if healthy {
                        fail_streak = 0;
                        thread::sleep(Duration::from_millis(250));
                    } else {
                        fail_streak = fail_streak.saturating_add(1);
                        let backoff = (250u64.saturating_mul(2u64.saturating_pow(fail_streak.min(5)))).min(5000);
                        log_debug!("volume", "Audio backend unhealthy, backing off {backoff}ms");
                        thread::sleep(Duration::from_millis(backoff));
                    }
                }
            } // end supervise loop
        });

        Self {
            config,
            subscribers,
            current_vol,
            is_muted,
            current_sink,
        }
    }

    fn spawn_pw_mon() -> Option<std::process::Child> {
        Command::new(resolve_helper("pw-mon"))
            .arg("-N")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }

    /// Cheap health probe: true when `wpctl get-volume` spawns, exits 0,
    /// and yields a parsable level. Used for backoff decisions.
    fn probe_audio_ok() -> bool {
        let Ok(out) = Command::new(resolve_helper("wpctl"))
            .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
            .output()
        else {
            return false;
        };
        if !out.status.success() {
            return false;
        }
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .any(|p| p.parse::<f64>().is_ok())
    }

    pub fn read_system_volume() -> (u32, bool) {
        static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let output = match Command::new(resolve_helper("wpctl"))
            .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                if !LOGGED.swap(true, Ordering::Relaxed) {
                    log_warn!("volume", "wpctl not available ({e}); showing last-known volume");
                }
                return (50, false);
            }
        };
        {
            let s = String::from_utf8_lossy(&output.stdout);
            let is_muted = s.contains("[MUTED]");
            for part in s.split_whitespace() {
                if let Ok(val) = part.parse::<f64>() {
                    LOGGED.store(false, Ordering::Relaxed);
                    let pct = (val * 100.0).round() as u32;
                    return (pct, is_muted);
                }
            }
            log_debug!("volume", "Unparseable wpctl output: {}", truncate_log(&s));
        }
        (50, false)
    }

    pub fn is_volume_event_line(line: &str) -> bool {
        line.contains("channelVolumes")
            || line.contains("Props:volume")
            || line.contains("Props:mute")
            || line.contains("ParamId:Route")
            || line.contains("Audio/Sink")
    }

    pub fn parse_wpctl_sinks(output: &str) -> (Vec<SinkInfo>, Option<usize>) {
        let mut sinks = Vec::new();
        let mut default_idx = None;
        let mut in_audio = false;
        let mut in_sinks = false;

        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed == "Audio" {
                in_audio = true;
                continue;
            } else if trimmed == "Video" || trimmed == "Settings" {
                in_audio = false;
                in_sinks = false;
            }

            if in_audio {
                if line.contains("Sinks:") {
                    in_sinks = true;
                    continue;
                } else if in_sinks
                    && (line.contains("Sources:") || line.contains("Filters:") || line.contains("Streams:"))
                {
                    in_sinks = false;
                    continue;
                }

                if in_sinks {
                    let cleaned = line.replace(['│', '├', '└', '─'], "").trim().to_string();
                    if cleaned.is_empty() {
                        continue;
                    }
                    let is_default = cleaned.starts_with('*');
                    let content = cleaned.trim_start_matches('*').trim();
                    if let Some((id_str, rest)) = content.split_once('.') {
                        let id_trimmed = id_str.trim();
                        if id_trimmed.chars().all(|c| c.is_ascii_digit()) && !id_trimmed.is_empty() {
                            let desc = rest.split("[vol:").next().unwrap_or("").trim().to_string();
                            let (stype, is_bt) = Self::classify_sink(id_trimmed, &desc, None, None, None, None);
                            if is_default {
                                default_idx = Some(sinks.len());
                            }
                            sinks.push(SinkInfo {
                                name: id_trimmed.to_string(),
                                description: desc,
                                sink_type: stype,
                                is_bluetooth: is_bt,
                            });
                        }
                    }
                }
            }
        }

        (sinks, default_idx)
    }

    pub fn classify_sink(
        name: &str,
        desc: &str,
        form_factor: Option<&str>,
        icon_name: Option<&str>,
        bus: Option<&str>,
        active_port: Option<&str>,
    ) -> (SinkType, bool) {
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
        // NOTE: `is_headset` is deliberately NOT folded in here. The branch
        // order below gives Bluetooth priority, then pure headphone markers,
        // then headset — folding would make `SinkType::Headset` unreachable.

        let is_hdmi = name_lower.contains("hdmi")
            || desc_lower.contains("hdmi")
            || desc_lower.contains("displayport")
            || port_lower.contains("hdmi");

        let sink_type = if is_bluetooth && (is_headphones || is_headset) {
            SinkType::Bluetooth
        } else if is_headphones {
            SinkType::Headphones
        } else if is_headset {
            SinkType::Headset
        } else if is_bluetooth {
            SinkType::Bluetooth
        } else if is_hdmi {
            SinkType::Hdmi
        } else {
            SinkType::Speaker
        };

        (sink_type, is_bluetooth)
    }

    pub fn read_current_sink() -> SinkInfo {
        match Command::new(resolve_helper("wpctl")).arg("status").output() {
            Ok(out) if out.status.success() => {
                let s = String::from_utf8_lossy(&out.stdout).to_string();
                let (sinks, default_idx) = Self::parse_wpctl_sinks(&s);
                if let Some(idx) = default_idx {
                    if let Some(sink) = sinks.get(idx) {
                        return sink.clone();
                    }
                }
                if let Some(first) = sinks.into_iter().next() {
                    return first;
                }
                log_debug!("volume", "wpctl status parsed no sinks");
            }
            Ok(out) => log_debug!("volume", "wpctl status failed: {}", out.status),
            Err(e) => log_debug!("volume", "wpctl not available for sink read: {e}"),
        }

        SinkInfo {
            name: "@DEFAULT_AUDIO_SINK@".to_string(),
            description: "Default Output".to_string(),
            sink_type: SinkType::Speaker,
            is_bluetooth: false,
        }
    }

    pub fn cycle_audio_output() {
        let out = match Command::new(resolve_helper("wpctl")).arg("status").output() {
            Ok(out) => out,
            Err(e) => {
                log_warn!("volume", "Sink cycle failed: wpctl not available: {e}");
                return;
            }
        };
        if !out.status.success() {
            log_warn!("volume", "Sink cycle failed: wpctl status exited with {}", out.status);
            return;
        }
        let s = String::from_utf8_lossy(&out.stdout).to_string();
        let (sinks, default_idx) = Self::parse_wpctl_sinks(&s);
        if sinks.is_empty() {
            log_warn!("volume", "Sink cycle found no audio sinks");
            return;
        }
        let cur = default_idx.unwrap_or(0);
        let next = (cur + 1) % sinks.len();
        // Sink names are numeric ids validated in `parse_wpctl_sinks`
        // (`is_ascii_digit`), passed as argv — never through a shell.
        if let Err(e) = Command::new(resolve_helper("wpctl"))
            .args(["set-default", &sinks[next].name])
            .status()
        {
            log_warn!("volume", "Failed to set default sink to {}: {e}", sinks[next].name);
        }
    }

    pub fn adapt_icon_for_sink(formatted: &str, _volume: u32, _is_muted: bool, sink_type: &SinkType) -> String {
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

        // If no custom level map was provided for this output type, adapt standard speaker icons
        if custom_levels.is_none() {
            formatted = Self::adapt_icon_for_sink(&formatted, volume, is_muted, &sink.sink_type);
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

        let tooltip = format!(
            "[{type_label}] {desc}",
            type_label = type_label,
            desc = sink.description
        );

        ModuleState {
            icon: None,
            text: Some(formatted),
            tooltip: Some(tooltip),
            css_classes,
        }
    }

    pub fn format_scroll_command(step: u32, max_volume: u32, direction: ScrollDirection) -> String {
        let step = if step == 0 { 10 } else { step };
        let max_vol = if max_volume == 0 { 100 } else { max_volume };
        let max_fraction = (max_vol as f64) / 100.0;
        let arg = match direction {
            ScrollDirection::Up => format!("{step}%+"),
            ScrollDirection::Down => format!("{step}%-"),
        };
        format!(
            "wpctl set-mute @DEFAULT_AUDIO_SINK@ 0 && wpctl set-volume -l {max_fraction:.2} @DEFAULT_AUDIO_SINK@ {arg}"
        )
    }

    fn dispatch_immediate_update(&self) {
        let vol = self.current_vol.load(Ordering::Relaxed);
        let muted = self.is_muted.load(Ordering::Relaxed);
        let sink = crate::util::lock(&self.current_sink).clone();
        crate::modules::broadcast(&self.subscribers, |orient| {
            Self::format_state_with_config(&self.config, vol, muted, &sink, orient)
        });
    }

    /// Cycle sink + re-read state off the GTK thread, then broadcast.
    /// Guarded: concurrent right-clicks coalesce instead of piling threads.
    fn cycle_sink_async(&self) {
        static IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if IN_FLIGHT.swap(true, Ordering::SeqCst) {
            log_debug!("volume", "Sink cycle already in flight; coalescing click");
            return;
        }
        let cur_vol = Arc::clone(&self.current_vol);
        let cur_muted = Arc::clone(&self.is_muted);
        let cur_sink = Arc::clone(&self.current_sink);
        let subs = Arc::clone(&self.subscribers);
        let cfg = self.config.clone();
        thread::spawn(move || {
            Self::cycle_audio_output();
            let (vol, muted) = Self::read_system_volume();
            let sink = Self::read_current_sink();
            cur_vol.store(vol, Ordering::Relaxed);
            cur_muted.store(muted, Ordering::Relaxed);
            {
                let mut guard = crate::util::lock(&cur_sink);
                *guard = sink.clone();
            }
            crate::modules::broadcast(&subs, |orient| {
                Self::format_state_with_config(&cfg, vol, muted, &sink, orient)
            });
            IN_FLIGHT.store(false, Ordering::SeqCst);
        });
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

    fn subscribe(&self, orientation: Orientation) -> async_channel::Receiver<ModuleState> {
        crate::modules::subscribe_to(&self.subscribers, orientation)
    }

    fn handle_click(&self, button: MouseButton) {
        match button {
            MouseButton::Left => {
                if let Some(ref cmd) = self.config.click.on_click_left {
                    let c = cmd.trim();
                    if !c.is_empty() {
                        spawn_command(c);
                        return;
                    }
                }

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

                    let max_fraction = (max_vol as f64) / 100.0;
                    let vol_fraction = (target_vol as f64) / 100.0;
                    spawn_command(&format!(
                        "wpctl set-mute @DEFAULT_AUDIO_SINK@ 0 && wpctl set-volume -l {max_fraction:.2} @DEFAULT_AUDIO_SINK@ {vol_fraction:.2}"
                    ));
                } else {
                    let new_muted = !is_muted;
                    self.is_muted.store(new_muted, Ordering::Relaxed);
                    self.dispatch_immediate_update();

                    spawn_command("wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle");
                }
            }
            MouseButton::Right => {
                if let Some(ref cmd) = self.config.click.on_click_right {
                    let c = cmd.trim();
                    if !c.is_empty() {
                        spawn_command(c);
                        return;
                    }
                }

                // Default Right Click action: cycle to the next audio output (sink).
                // `wpctl status` forks + parses on every click; never block the
                // GTK main thread on it. The background `pw-mon` poller refreshes
                // state within 250ms anyway — this thread just makes it instant.
                self.cycle_sink_async();
            }
            MouseButton::Middle => {
                if let Some(ref cmd) = self.config.click.on_click_middle {
                    let c = cmd.trim();
                    if !c.is_empty() {
                        spawn_command(c);
                    }
                }
            }
        }
    }

    fn handle_scroll(&self, direction: ScrollDirection) {
        // Rate-limit scroll spam (60ms): each event forks `sh` + `wpctl` x2
        // plus a reaper thread; unthrottled smooth scrolling piles up.
        static LAST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if now.wrapping_sub(LAST_MS.load(Ordering::Relaxed)) < 60 {
            return;
        }
        LAST_MS.store(now, Ordering::Relaxed);
        let cmd = Self::format_scroll_command(self.config.step, self.config.max_volume, direction);
        spawn_command(&cmd);
    }

    fn is_clickable(&self) -> bool {
        // Always has default actions (mute toggle / sink cycle / mixer).
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl SinkInfo {
        pub fn speaker() -> Self {
            Self {
                name: "default_speaker".to_string(),
                description: "Speaker".to_string(),
                sink_type: SinkType::Speaker,
                is_bluetooth: false,
            }
        }

        pub fn bluetooth_headset(name: &str, desc: &str) -> Self {
            Self {
                name: name.to_string(),
                description: desc.to_string(),
                sink_type: SinkType::Bluetooth,
                is_bluetooth: true,
            }
        }

        pub fn headphones(name: &str, desc: &str) -> Self {
            Self {
                name: name.to_string(),
                description: desc.to_string(),
                sink_type: SinkType::Headphones,
                is_bluetooth: false,
            }
        }

        pub fn hdmi(name: &str, desc: &str) -> Self {
            Self {
                name: name.to_string(),
                description: desc.to_string(),
                sink_type: SinkType::Hdmi,
                is_bluetooth: false,
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
    fn test_volume_click_behavior() {
        let config = VolumeConfig::default();
        let module = VolumeModule::new(config);

        // When volume is 0, clicking left should set volume to step (10) and un-mute
        module.current_vol.store(0, Ordering::Relaxed);
        module.is_muted.store(true, Ordering::Relaxed);
        module.handle_click(MouseButton::Left);
        assert_eq!(module.current_vol.load(Ordering::Relaxed), 10);
        assert!(!module.is_muted.load(Ordering::Relaxed));

        // When volume is 50, clicking left should toggle mute while preserving volume 50
        module.current_vol.store(50, Ordering::Relaxed);
        module.is_muted.store(false, Ordering::Relaxed);
        module.handle_click(MouseButton::Left);
        assert_eq!(module.current_vol.load(Ordering::Relaxed), 50);
        assert!(module.is_muted.load(Ordering::Relaxed));

        // Clicking left again un-mutes and retains volume 50
        module.handle_click(MouseButton::Left);
        assert_eq!(module.current_vol.load(Ordering::Relaxed), 50);
        assert!(!module.is_muted.load(Ordering::Relaxed));

        // Middle click with default config (None) is a no-op
        module.handle_click(MouseButton::Middle);
        assert_eq!(module.current_vol.load(Ordering::Relaxed), 50);
        assert!(!module.is_muted.load(Ordering::Relaxed));
    }

    #[test]
    fn test_format_scroll_command() {
        // Default step (10) and default max (100)
        let cmd_up = VolumeModule::format_scroll_command(0, 0, ScrollDirection::Up);
        assert_eq!(
            cmd_up,
            "wpctl set-mute @DEFAULT_AUDIO_SINK@ 0 && wpctl set-volume -l 1.00 @DEFAULT_AUDIO_SINK@ 10%+"
        );

        let cmd_down = VolumeModule::format_scroll_command(0, 0, ScrollDirection::Down);
        assert_eq!(
            cmd_down,
            "wpctl set-mute @DEFAULT_AUDIO_SINK@ 0 && wpctl set-volume -l 1.00 @DEFAULT_AUDIO_SINK@ 10%-"
        );

        // Custom step (5) and custom max (150)
        let cmd_custom_up = VolumeModule::format_scroll_command(5, 150, ScrollDirection::Up);
        assert_eq!(
            cmd_custom_up,
            "wpctl set-mute @DEFAULT_AUDIO_SINK@ 0 && wpctl set-volume -l 1.50 @DEFAULT_AUDIO_SINK@ 5%+"
        );

        let cmd_custom_down = VolumeModule::format_scroll_command(5, 150, ScrollDirection::Down);
        assert_eq!(
            cmd_custom_down,
            "wpctl set-mute @DEFAULT_AUDIO_SINK@ 0 && wpctl set-volume -l 1.50 @DEFAULT_AUDIO_SINK@ 5%-"
        );
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
            VolumeModule::adapt_icon_for_sink("\n50%", 50, false, &SinkType::Speaker),
            "\n50%"
        );
        assert_eq!(
            VolumeModule::adapt_icon_for_sink("\nM", 50, true, &SinkType::Speaker),
            "\nM"
        );

        // Headset / Bluetooth / Headphones adapt to 󰋋
        assert_eq!(
            VolumeModule::adapt_icon_for_sink("\n50%", 50, false, &SinkType::Headset),
            "󰋋\n50%"
        );
        assert_eq!(
            VolumeModule::adapt_icon_for_sink("\n100%", 100, false, &SinkType::Bluetooth),
            "󰋋\n100%"
        );
        assert_eq!(
            VolumeModule::adapt_icon_for_sink("\nM", 50, true, &SinkType::Headphones),
            "󰋋\nM"
        );

        // HDMI adapts to 󰍹
        assert_eq!(
            VolumeModule::adapt_icon_for_sink("\n50%", 50, false, &SinkType::Hdmi),
            "󰍹\n50%"
        );
    }

    #[test]
    fn test_live_system_sinks_query() {
        let current = VolumeModule::read_current_sink();
        assert!(!current.name.is_empty());
    }

    #[test]
    fn test_parse_wpctl_sinks() {
        let wpctl_output = r#"
PipeWire 'pipewire-0' [1.6.9, user@linux, cookie:3057743936]
 └─ Clients:
        33. WirePlumber                         [1.6.9, user@linux, pid:1382]

Audio
 ├─ Devices:
 │      49. GA106 High Definition Audio Controller [alsa]
 │      50. Ryzen HD Audio Controller           [alsa]
 │  
 ├─ Sinks:
 │  *   57. Ryzen HD Audio Controller Speaker   [vol: 1.16]
 │      62. WH-1000XM4 Wireless Headphones     [vol: 0.80]
 │  
 ├─ Sources:
 │      58. Ryzen HD Audio Controller Stereo Microphone [vol: 1.00]
 │  *   59. Ryzen HD Audio Controller Digital Microphone [vol: 1.00]
"#;
        let (sinks, default_idx) = VolumeModule::parse_wpctl_sinks(wpctl_output);
        assert_eq!(sinks.len(), 2);
        assert_eq!(sinks[0].name, "57");
        assert_eq!(sinks[0].description, "Ryzen HD Audio Controller Speaker");
        assert_eq!(sinks[0].sink_type, SinkType::Speaker);
        assert_eq!(sinks[1].name, "62");
        assert_eq!(sinks[1].description, "WH-1000XM4 Wireless Headphones");
        assert_eq!(sinks[1].sink_type, SinkType::Headphones);
        assert_eq!(default_idx, Some(0));
    }

    #[test]
    fn test_parse_wpctl_sinks_garbage() {
        // Empty, garbage, and header-only outputs must yield no sinks —
        // never panic, never fabricate.
        for garbage in ["", "\n\n", "not pipewire output\n{{{\n", "Audio\n  ├─ Sinks:\n"] {
            let (sinks, default_idx) = VolumeModule::parse_wpctl_sinks(garbage);
            assert!(sinks.is_empty(), "garbage parsed as sinks: {garbage:?}");
            assert_eq!(default_idx, None);
        }
        // Non-numeric sink ids are skipped, valid ones kept.
        let mixed =
            "Audio\n  ├─ Sinks:\n  │  *   abc. Weird Sink   [vol: 1.00]\n  │      62. Real Sink   [vol: 0.80]\n";
        let (sinks, default_idx) = VolumeModule::parse_wpctl_sinks(mixed);
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks[0].name, "62");
        assert_eq!(default_idx, None);
    }

    #[test]
    fn test_is_volume_event_line() {
        assert!(VolumeModule::is_volume_event_line(
            "Prop: key Spa:Pod:Object:Param:Props:channelVolumes (65544)"
        ));
        assert!(VolumeModule::is_volume_event_line(
            "Prop: key Spa:Pod:Object:Param:Props:mute (65540)"
        ));
        assert!(VolumeModule::is_volume_event_line(
            "Prop: key Spa:Pod:Object:Param:Props:volume (65539)"
        ));
        assert!(VolumeModule::is_volume_event_line("Audio/Sink"));
        assert!(VolumeModule::is_volume_event_line("ParamId:Route"));
        assert!(!VolumeModule::is_volume_event_line("core.name = \"pipewire-0\""));
    }

    #[test]
    fn test_classify_sink() {
        let n = VolumeModule::classify_sink;
        // Plain speaker is the default.
        assert_eq!(
            n("alsa_output", "Ryzen HD Audio Speaker", None, None, None, None).0,
            SinkType::Speaker
        );
        // Bluetooth via bus, bluez_ prefix, or icon.
        assert!(n("bluez_sink.01", "Speaker", None, None, None, None).1);
        assert_eq!(
            n("bluez_sink.01", "Speaker", None, None, None, None).0,
            SinkType::Bluetooth
        );
        assert!(
            n(
                "alsa_bt",
                "Speaker",
                None,
                Some("audio-bluetooth"),
                Some("bluetooth"),
                None
            )
            .1
        );
        // Bluetooth headphones keep the Bluetooth type (not plain Headphones).
        assert_eq!(
            n("bluez_sink.01", "WH-1000XM4", None, None, Some("bluetooth"), None).0,
            SinkType::Bluetooth
        );
        // Wired headphones via form factor, desc keywords, or port.
        assert_eq!(
            n("alsa_hp", "Headphones", Some("headphone"), None, None, None).0,
            SinkType::Headphones
        );
        assert_eq!(
            n("alsa_hp", "Pixel Buds Pro", None, None, None, None).0,
            SinkType::Headphones
        );
        assert_eq!(n("alsa_hp", "AirPods", None, None, None, None).0, SinkType::Headphones);
        assert_eq!(
            n("alsa_hp", "Output", None, None, None, Some("headphone")).0,
            SinkType::Headphones
        );
        // Headset without headphone markers.
        assert_eq!(
            n("usb_dongle", "Jabra Headset", None, None, None, None).0,
            SinkType::Headset
        );
        // HDMI via name, desc, or DisplayPort alias.
        assert_eq!(n("alsa_hdmi", "Output", None, None, None, None).0, SinkType::Hdmi);
        assert_eq!(
            n("alsa_dp", "DisplayPort Monitor", None, None, None, None).0,
            SinkType::Hdmi
        );
    }
}
