use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

const DEFAULT_CONFIG_TOML: &str = include_str!("../resources/config.default.toml");

/// Bar anchor edge. Vertical (`Left`/`Right`) bars stack modules top-down;
/// horizontal (`Top`/`Bottom`) bars lay them out left-to-right.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BarPosition {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

impl BarPosition {
    /// True for `Left`/`Right` (vertical layout).
    pub fn is_vertical(&self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

/// Pick a display template for a numeric `value` (volume %, brightness %,
/// battery %).
///
/// Numeric keys are thresholds (`"0"`, `"33"`, `"100"`, optional trailing
/// `%`); the highest threshold `<= value` wins and `%v` is substituted.
/// A `special_state` (`"muted"`, `"charging"`, `"full"`) takes precedence
/// when present. Returns the rendered text and the matched threshold.
pub fn resolve_level(
    levels: &HashMap<String, String>,
    value: u32,
    special_state: Option<&str>,
) -> (String, Option<u32>) {
    // 1. If special state is passed (e.g. "muted", "charging", "full"), check if it exists in levels
    if let Some(state) = special_state {
        if let Some(template) = levels.get(state) {
            let replaced = template.replace("%v", &value.to_string());
            return (replaced, None);
        }
    }

    // 2. Parse numeric thresholds from the map
    let mut numeric_thresholds: Vec<(u32, &String)> = Vec::new();
    for (k, v) in levels {
        let clean_k = k.trim().trim_end_matches('%');
        if let Ok(threshold) = clean_k.parse::<u32>() {
            numeric_thresholds.push((threshold, v));
        }
    }

    numeric_thresholds.sort_by_key(|(t, _)| *t);

    if numeric_thresholds.is_empty() {
        return (format!("{value}%"), None);
    }

    // Find the highest threshold <= value
    let mut chosen_threshold = numeric_thresholds[0].0;
    let mut chosen_template = numeric_thresholds[0].1;

    for (threshold, template) in &numeric_thresholds {
        if *threshold <= value {
            chosen_threshold = *threshold;
            chosen_template = template;
        } else {
            break;
        }
    }

    let replaced = chosen_template.replace("%v", &value.to_string());
    (replaced, Some(chosen_threshold))
}

/// Pick a display template for a named mode (`wifi`/`lan`/`disconnected`,
/// `on`/`off`/`connected`, ...), substituting `%`-style placeholders.
/// Unknown modes fall back to the mode name itself.
pub fn resolve_mode(modes: &HashMap<String, String>, mode: &str, placeholders: &[(&str, &str)]) -> String {
    if let Some(template) = modes.get(mode) {
        let mut result = template.clone();
        for (k, v) in placeholders {
            result = result.replace(k, v);
        }
        result
    } else {
        mode.to_string()
    }
}

/// Optional shell commands run on mouse clicks (via `sh -c`, so `&&`,
/// pipes, `~` and env vars work). Set from the user's own `config.toml`;
/// never from D-Bus/IPC data.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClickActions {
    #[serde(default)]
    pub on_click_left: Option<String>,
    #[serde(default)]
    pub on_click_right: Option<String>,
    #[serde(default)]
    pub on_click_middle: Option<String>,
}

impl ClickActions {
    /// Commands for (left, right, middle) clicks.
    pub fn commands(&self) -> (Option<&str>, Option<&str>, Option<&str>) {
        (
            self.on_click_left.as_deref(),
            self.on_click_right.as_deref(),
            self.on_click_middle.as_deref(),
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskbarConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_icon_size")]
    pub icon_size: i32,
    #[serde(default = "default_true")]
    pub only_current_workspace: bool,
    #[serde(default = "default_true")]
    pub show_tooltips: bool,
}

impl Default for TaskbarConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            icon_size: 32,
            only_current_workspace: true,
            show_tooltips: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrayConfig {
    #[serde(default = "default_tray_icon_size")]
    pub icon_size: i32,
    #[serde(default = "default_tray_spacing")]
    pub spacing: i32,
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self {
            icon_size: 22,
            spacing: 4,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModulesConfig {
    #[serde(default = "default_module_order")]
    pub order: Vec<String>,
    #[serde(default)]
    pub start: Option<Vec<String>>,
    #[serde(default)]
    pub end: Option<Vec<String>>,
}

impl Default for ModulesConfig {
    fn default() -> Self {
        Self {
            order: default_module_order(),
            start: None,
            end: None,
        }
    }
}

impl ModulesConfig {
    pub fn start_modules(&self) -> Vec<String> {
        self.start
            .clone()
            .unwrap_or_else(|| vec!["spacer".to_string(), "taskbar".to_string()])
    }

    pub fn end_modules(&self) -> Vec<String> {
        self.end.clone().unwrap_or_else(|| self.order.clone())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct VolumeConfig {
    #[serde(default = "default_step")]
    pub step: u32,
    #[serde(default = "default_max_volume")]
    pub max_volume: u32,
    #[serde(default = "default_volume_levels")]
    pub levels: HashMap<String, String>,
    pub levels_headphones: Option<HashMap<String, String>>,
    pub levels_bluetooth: Option<HashMap<String, String>>,
    pub levels_hdmi: Option<HashMap<String, String>>,
    #[serde(flatten)]
    pub click: ClickActions,
}

impl VolumeConfig {
    pub fn get_levels_for_sink(
        &self,
        is_bluetooth: bool,
        is_headphones: bool,
        is_hdmi: bool,
    ) -> Option<&HashMap<String, String>> {
        if is_bluetooth && self.levels_bluetooth.is_some() {
            return self.levels_bluetooth.as_ref();
        }
        if is_headphones && self.levels_headphones.is_some() {
            return self.levels_headphones.as_ref();
        }
        if is_hdmi && self.levels_hdmi.is_some() {
            return self.levels_hdmi.as_ref();
        }
        None
    }
}

impl Default for VolumeConfig {
    fn default() -> Self {
        Self {
            step: 10,
            max_volume: 150,
            levels: default_volume_levels(),
            levels_headphones: None,
            levels_bluetooth: None,
            levels_hdmi: None,
            click: ClickActions::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BluetoothConfig {
    #[serde(default = "default_bluetooth_modes")]
    pub modes: HashMap<String, String>,
    #[serde(flatten)]
    pub click: ClickActions,
}

impl Default for BluetoothConfig {
    fn default() -> Self {
        Self {
            modes: default_bluetooth_modes(),
            click: ClickActions::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkConfig {
    #[serde(default = "default_network_modes")]
    pub modes: HashMap<String, String>,
    #[serde(flatten)]
    pub click: ClickActions,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            modes: default_network_modes(),
            click: ClickActions::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_memory_format")]
    pub format: String,
    #[serde(flatten)]
    pub click: ClickActions,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            format: default_memory_format(),
            click: ClickActions::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BrightnessConfig {
    #[serde(default = "default_step")]
    pub step: u32,
    #[serde(default = "default_brightness_levels")]
    pub levels: HashMap<String, String>,
    #[serde(flatten)]
    pub click: ClickActions,
}

impl Default for BrightnessConfig {
    fn default() -> Self {
        Self {
            step: 10,
            levels: default_brightness_levels(),
            click: ClickActions::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BatteryConfig {
    #[serde(default = "default_warning_threshold")]
    pub warning_threshold: u8,
    #[serde(default = "default_critical_threshold")]
    pub critical_threshold: u8,
    #[serde(default = "default_battery_levels")]
    pub levels: HashMap<String, String>,
    #[serde(flatten)]
    pub click: ClickActions,
}

impl Default for BatteryConfig {
    fn default() -> Self {
        Self {
            warning_threshold: 30,
            critical_threshold: 15,
            levels: default_battery_levels(),
            click: ClickActions::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClockConfig {
    #[serde(default = "default_clock_vertical")]
    pub format_vertical: String,
    #[serde(default = "default_clock_horizontal")]
    pub format_horizontal: String,
    #[serde(flatten)]
    pub click: ClickActions,
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            format_vertical: default_clock_vertical(),
            format_horizontal: default_clock_horizontal(),
            click: ClickActions::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpacerConfig {
    #[serde(default = "default_spacer_size")]
    pub size: i32,
    #[serde(flatten)]
    pub click: ClickActions,
}

impl Default for SpacerConfig {
    fn default() -> Self {
        Self {
            size: default_spacer_size(),
            click: ClickActions::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub position: BarPosition,
    #[serde(default = "default_size")]
    pub size: i32,
    #[serde(default)]
    pub spacing: i32,
    #[serde(default)]
    pub taskbar: TaskbarConfig,
    #[serde(default)]
    pub tray: TrayConfig,
    #[serde(default)]
    pub modules: ModulesConfig,
    #[serde(default)]
    pub volume: VolumeConfig,
    #[serde(default)]
    pub bluetooth: BluetoothConfig,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub brightness: BrightnessConfig,
    #[serde(default)]
    pub battery: BatteryConfig,
    #[serde(default)]
    pub clock: ClockConfig,
    #[serde(default)]
    pub spacer: SpacerConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        toml::from_str(DEFAULT_CONFIG_TOML).unwrap_or_else(|_| AppConfig {
            position: BarPosition::Left,
            size: 50,
            spacing: 0,
            taskbar: TaskbarConfig::default(),
            tray: TrayConfig::default(),
            modules: ModulesConfig::default(),
            volume: VolumeConfig::default(),
            bluetooth: BluetoothConfig::default(),
            network: NetworkConfig::default(),
            memory: MemoryConfig::default(),
            brightness: BrightnessConfig::default(),
            battery: BatteryConfig::default(),
            clock: ClockConfig::default(),
            spacer: SpacerConfig::default(),
        })
    }
}

pub fn get_config_dir() -> PathBuf {
    if let Ok(cfg) = std::env::var("XDG_CONFIG_HOME") {
        if !cfg.trim().is_empty() {
            return PathBuf::from(cfg).join("niri-bar");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home).join(".config").join("niri-bar");
        }
    }
    PathBuf::from(".config/niri-bar")
}

/// Load `config.toml`, writing the embedded default on first launch.
///
/// Never panics: a missing file is created, and parse/I/O errors fall back
/// to [`AppConfig::default`] with a warning. Unknown top-level keys are
/// logged (typo guard) without nuking the whole file.
/// Live-reloads via the file watcher in `main.rs`; no restart needed.
///
/// Returns the effective config plus `Some(error_message)` when the file
/// could not be read or parsed, so the bar can display an error badge
/// on startup.
pub fn load_config_with_error() -> (AppConfig, Option<String>) {
    load_config_with_error_in(&get_config_dir())
}

/// Same as [`load_config_with_error`] but rooted at `config_dir`.
/// Exists so tests can use an isolated temp dir instead of mutating the
/// real `XDG_CONFIG_HOME` (env writes are `unsafe` and denied here).
pub fn load_config_with_error_in(config_dir: &std::path::Path) -> (AppConfig, Option<String>) {
    let config_path = config_dir.join("config.toml");

    if !config_path.exists() {
        if let Err(e) = fs::create_dir_all(config_dir) {
            crate::logger::emit("WARN", "config", &format!("Failed to create {config_dir:?}: {e}"));
        } else if let Err(e) = fs::write(&config_path, DEFAULT_CONFIG_TOML) {
            crate::logger::emit(
                "WARN",
                "config",
                &format!("Failed to write default {config_path:?}: {e}"),
            );
        }
        return (AppConfig::default().validated(), None);
    }

    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("Failed to read {}: {e}", config_path.display());
            crate::logger::emit("WARN", "config", &format!("{msg}. Falling back to default."));
            return (AppConfig::default().validated(), Some(format!("Config error: {msg}")));
        }
    };

    match reload_config_from_str(&content) {
        Ok(cfg) => (cfg, None),
        Err(err) => {
            crate::logger::emit(
                "WARN",
                "config",
                &format!(
                    "Failed to parse {}: {err}. Falling back to default.",
                    config_path.display()
                ),
            );
            (AppConfig::default().validated(), Some(err))
        }
    }
}

/// Parse a TOML string into an [`AppConfig`], returning an error message with line/details on failure.
pub fn reload_config_from_str(content: &str) -> Result<AppConfig, String> {
    let table: toml::Table = toml::from_str(content).map_err(|e| format!("Syntax error in config.toml: {e}"))?;
    Ok(parse_config_table(&table).validated())
}

/// Read and parse the on-disk `config.toml`, returning `Ok(AppConfig)` or `Err(error_details)`.
pub fn reload_config() -> Result<AppConfig, String> {
    let config_dir = get_config_dir();
    let config_path = config_dir.join("config.toml");
    let content =
        fs::read_to_string(&config_path).map_err(|e| format!("Failed to read {}: {e}", config_path.display()))?;
    reload_config_from_str(&content)
}

/// Build [`AppConfig`] from an already-parsed TOML table with per-section
/// fallback: one bad value (e.g. `position = "centre"`) resets only its
/// section, never the whole bar. Pure apart from warning logs, so tests
/// can exercise typo resilience without touching the filesystem.
pub fn parse_config_table(table: &toml::Table) -> AppConfig {
    const KNOWN: &[&str] = &[
        "position",
        "size",
        "spacing",
        "taskbar",
        "tray",
        "modules",
        "volume",
        "bluetooth",
        "network",
        "memory",
        "brightness",
        "battery",
        "clock",
        "spacer",
    ];
    for key in table.keys() {
        if !KNOWN.contains(&key.as_str()) {
            crate::logger::emit("WARN", "config", &format!("Unknown config key '{key}', ignoring"));
        }
    }
    fn section<T>(table: &toml::Table, key: &str) -> T
    where
        T: for<'de> serde::Deserialize<'de> + Default,
    {
        let Some(v) = table.get(key) else {
            return T::default();
        };
        // Fast path: whole section parses.
        if let Ok(s) = v.clone().try_into() {
            return s;
        }
        // Slow path (startup errors only): per-field fallback. If dropping
        // exactly one key makes the section parse, only that field was bad —
        // keep everything else. Bounded O(n) parses on tiny tables.
        if let Some(map) = v.as_table() {
            for k in map.keys() {
                let mut candidate = map.clone();
                candidate.remove(k);
                if let Ok(s) = toml::Value::Table(candidate).try_into() {
                    crate::logger::emit(
                        "WARN",
                        "config",
                        &format!("Invalid [{key}] field '{k}'; using default for it"),
                    );
                    return s;
                }
            }
        }
        // Unsalvageable (multiple bad fields, or section isn't a table):
        // default the section, keep the rest of the file.
        crate::logger::emit(
            "WARN",
            "config",
            &format!("Invalid [{key}] section; using defaults for it"),
        );
        T::default()
    }
    let cfg = AppConfig {
        position: section(table, "position"),
        size: table
            .get("size")
            .and_then(|v| v.as_integer())
            .map(|n| n as i32)
            .unwrap_or(50),
        spacing: table
            .get("spacing")
            .and_then(|v| v.as_integer())
            .map(|n| n as i32)
            .unwrap_or(0),
        taskbar: section(table, "taskbar"),
        tray: section(table, "tray"),
        modules: section(table, "modules"),
        volume: section(table, "volume"),
        bluetooth: section(table, "bluetooth"),
        network: section(table, "network"),
        memory: section(table, "memory"),
        brightness: section(table, "brightness"),
        battery: section(table, "battery"),
        clock: section(table, "clock"),
        spacer: section(table, "spacer"),
    };
    // Scalar type errors (e.g. `size = "big"`) fall back with a hint.
    if table.get("size").is_some_and(|v| v.as_integer().is_none()) {
        crate::logger::emit("WARN", "config", "Invalid `size`, using 50");
    }
    if table.get("spacing").is_some_and(|v| v.as_integer().is_none()) {
        crate::logger::emit("WARN", "config", "Invalid `spacing`, using 0");
    }
    cfg.validated()
}

impl AppConfig {
    /// Clamp invalid numerics with warnings instead of broken layout.
    /// `step = 0` / `max_volume = 0` are handled at the use sites
    /// (`volume`/`brightness` treat 0 as 10 / 100).
    pub fn validated(mut self) -> Self {
        if self.size <= 0 || self.size > 500 {
            crate::logger::emit("WARN", "config", &format!("Invalid size {}, using 50", self.size));
            self.size = 50;
        }
        if self.spacing < 0 || self.spacing > 100 {
            crate::logger::emit("WARN", "config", &format!("Invalid spacing {}, using 0", self.spacing));
            self.spacing = 0;
        }
        if self.taskbar.icon_size <= 0 || self.taskbar.icon_size > 256 {
            crate::logger::emit("WARN", "config", "Invalid taskbar.icon_size, using 32");
            self.taskbar.icon_size = 32;
        }
        if self.tray.icon_size <= 0 || self.tray.icon_size > 256 {
            crate::logger::emit("WARN", "config", "Invalid tray.icon_size, using 22");
            self.tray.icon_size = 22;
        }
        if self.tray.spacing < 0 || self.tray.spacing > 100 {
            crate::logger::emit("WARN", "config", "Invalid tray.spacing, using 4");
            self.tray.spacing = 4;
        }
        if self.spacer.size < 0 || self.spacer.size > 500 {
            crate::logger::emit("WARN", "config", "Invalid spacer.size, using 5");
            self.spacer.size = 5;
        }
        if self.battery.warning_threshold <= self.battery.critical_threshold {
            crate::logger::emit(
                "WARN",
                "config",
                &format!(
                    "battery.warning_threshold ({}) must exceed critical ({}); swapping",
                    self.battery.warning_threshold, self.battery.critical_threshold
                ),
            );
            std::mem::swap(
                &mut self.battery.warning_threshold,
                &mut self.battery.critical_threshold,
            );
        }
        self
    }
}

fn default_true() -> bool {
    true
}
fn default_icon_size() -> i32 {
    32
}
fn default_tray_icon_size() -> i32 {
    22
}
fn default_tray_spacing() -> i32 {
    4
}
fn default_size() -> i32 {
    50
}
fn default_step() -> u32 {
    10
}
fn default_max_volume() -> u32 {
    150
}
fn default_memory_format() -> String {
    "󰍛\n%uG".to_string()
}
fn default_warning_threshold() -> u8 {
    30
}
fn default_critical_threshold() -> u8 {
    15
}
fn default_clock_vertical() -> String {
    "%H:%M\n%a\n%b\n%d/%m".to_string()
}
fn default_clock_horizontal() -> String {
    "%H:%M %a %b %d/%m".to_string()
}
fn default_spacer_size() -> i32 {
    5
}
fn default_module_order() -> Vec<String> {
    vec![
        "tray".into(),
        "volume".into(),
        "bluetooth".into(),
        "network".into(),
        "memory".into(),
        "brightness".into(),
        "battery".into(),
        "clock".into(),
    ]
}

fn default_volume_levels() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("0".into(), "\n%v%".into());
    m.insert("1".into(), "\n%v%".into());
    m.insert("100".into(), "\n%v%".into());
    m.insert("muted".into(), "\nM".into());
    m
}

fn default_brightness_levels() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("0".into(), "󰃞\n%v%".into());
    m.insert("33".into(), "󰃟\n%v%".into());
    m.insert("66".into(), "󰃠\n%v%".into());
    m.insert("100".into(), "󰃡\n%v%".into());
    m
}

fn default_battery_levels() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("0".into(), "\n%v%".into());
    m.insert("33".into(), "\n%v%".into());
    m.insert("66".into(), "\n%v%".into());
    m.insert("100".into(), "\n%v%".into());
    m.insert("charging".into(), "\n%v%".into());
    m.insert("full".into(), "\nFull".into());
    m
}

fn default_network_modes() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("wifi".into(), "󰤨\nWiFi".into());
    m.insert("lan".into(), "󰈀\nLAN".into());
    m.insert("disconnected".into(), "󰤮\nOff".into());
    m
}

fn default_bluetooth_modes() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("connected".into(), "󰂱\nOn".into());
    m.insert("on".into(), "\nOn".into());
    m.insert("off".into(), "󰂲\nOff".into());
    m.insert("loading".into(), "󰂯\n...".into());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_level_thresholds() {
        let levels = default_volume_levels();

        // Value 0
        let (res_0, t_0) = resolve_level(&levels, 0, None);
        assert_eq!(res_0, "\n0%");
        assert_eq!(t_0, Some(0));

        // Value 25 -> highest <= 25 is 1
        let (res_25, t_25) = resolve_level(&levels, 25, None);
        assert_eq!(res_25, "\n25%");
        assert_eq!(t_25, Some(1));

        // Value 45 -> highest <= 45 is 1
        let (res_45, t_45) = resolve_level(&levels, 45, None);
        assert_eq!(res_45, "\n45%");
        assert_eq!(t_45, Some(1));

        // Value 80 -> highest <= 80 is 1
        let (res_80, t_80) = resolve_level(&levels, 80, None);
        assert_eq!(res_80, "\n80%");
        assert_eq!(t_80, Some(1));

        // Value 100
        let (res_100, t_100) = resolve_level(&levels, 100, None);
        assert_eq!(res_100, "\n100%");
        assert_eq!(t_100, Some(100));

        // Special state "muted"
        let (res_muted, t_muted) = resolve_level(&levels, 50, Some("muted"));
        assert_eq!(res_muted, "\nM");
        assert_eq!(t_muted, None);
    }

    #[test]
    fn test_resolve_mode() {
        let modes = default_network_modes();
        let wifi = resolve_mode(&modes, "wifi", &[]);
        assert_eq!(wifi, "󰤨\nWiFi");

        let lan = resolve_mode(&modes, "lan", &[]);
        assert_eq!(lan, "󰈀\nLAN");

        let off = resolve_mode(&modes, "disconnected", &[]);
        assert_eq!(off, "󰤮\nOff");

        let mut custom_modes = HashMap::new();
        custom_modes.insert("wifi".into(), "󰤨\n%s".into());
        let custom_wifi = resolve_mode(&custom_modes, "wifi", &[("%s", "MyHomeWiFi")]);
        assert_eq!(custom_wifi, "󰤨\nMyHomeWiFi");
    }

    #[test]
    fn test_default_config_parses_successfully() {
        let cfg: AppConfig = toml::from_str(DEFAULT_CONFIG_TOML).expect("Default config toml must be valid");
        assert_eq!(cfg.position, BarPosition::Left);
        assert!(cfg.position.is_vertical());
        assert_eq!(cfg.size, 50);
        assert!(cfg.taskbar.enabled);
        assert_eq!(cfg.taskbar.icon_size, 32);
        assert_eq!(cfg.battery.warning_threshold, 30);
        assert_eq!(cfg.battery.critical_threshold, 15);
        assert_eq!(cfg.spacer.size, 5);
        assert_eq!(cfg.modules.start_modules(), vec!["spacer", "taskbar"]);
        assert_eq!(
            cfg.modules.end_modules(),
            vec![
                "tray",
                "volume",
                "bluetooth",
                "network",
                "memory",
                "brightness",
                "battery",
                "clock"
            ]
        );
    }

    #[test]
    fn test_custom_granular_levels() {
        let mut custom_levels = HashMap::new();
        custom_levels.insert("0%".to_string(), "LOW\n%v%".to_string());
        custom_levels.insert("10%".to_string(), "10-STEPS\n%v%".to_string());
        custom_levels.insert("20%".to_string(), "20-STEPS\n%v%".to_string());
        custom_levels.insert("30%".to_string(), "30-STEPS\n%v%".to_string());
        custom_levels.insert("40%".to_string(), "40-STEPS\n%v%".to_string());
        custom_levels.insert("50%".to_string(), "HALF\n%v%".to_string());
        custom_levels.insert("90%".to_string(), "HIGH\n%v%".to_string());

        let (res_15, t_15) = resolve_level(&custom_levels, 15, None);
        assert_eq!(res_15, "10-STEPS\n15%");
        assert_eq!(t_15, Some(10));

        let (res_35, t_35) = resolve_level(&custom_levels, 35, None);
        assert_eq!(res_35, "30-STEPS\n35%");
        assert_eq!(t_35, Some(30));

        let (res_99, t_99) = resolve_level(&custom_levels, 99, None);
        assert_eq!(res_99, "HIGH\n99%");
        assert_eq!(t_99, Some(90));
    }

    #[test]
    fn test_volume_config_max_volume() {
        let default_cfg: VolumeConfig = toml::from_str("").unwrap();
        assert_eq!(default_cfg.max_volume, 150);

        let custom_toml = r#"
            step = 5
            max_volume = 100
        "#;
        let custom_cfg: VolumeConfig = toml::from_str(custom_toml).unwrap();
        assert_eq!(custom_cfg.max_volume, 100);
        assert_eq!(custom_cfg.step, 5);
    }

    #[test]
    fn test_resolve_level_above_100() {
        let default_levels = default_volume_levels();
        let (res_150, t_150) = resolve_level(&default_levels, 150, None);
        assert_eq!(res_150, "\n150%");
        assert_eq!(t_150, Some(100));

        let mut boost_levels = default_levels.clone();
        boost_levels.insert("150".to_string(), "🔊\n%v%".to_string());
        let (res_boost, t_boost) = resolve_level(&boost_levels, 150, None);
        assert_eq!(res_boost, "🔊\n150%");
        assert_eq!(t_boost, Some(150));
    }

    #[test]
    fn test_bar_position_vertical() {
        assert!(BarPosition::Left.is_vertical());
        assert!(BarPosition::Right.is_vertical());
        assert!(!BarPosition::Top.is_vertical());
        assert!(!BarPosition::Bottom.is_vertical());
    }

    #[test]
    fn test_partial_config_parsing_uses_defaults() {
        // Empty TOML should parse successfully with all default values
        let cfg: AppConfig = toml::from_str("").expect("Empty TOML should use defaults");
        assert_eq!(cfg.position, BarPosition::Left);
        assert_eq!(cfg.size, 50);
        assert_eq!(cfg.spacing, 0);
        assert!(cfg.taskbar.enabled);
        assert_eq!(cfg.taskbar.icon_size, 32);
        assert_eq!(cfg.tray.icon_size, 22);
        assert_eq!(cfg.battery.warning_threshold, 30);
        assert_eq!(cfg.battery.critical_threshold, 15);
        assert_eq!(cfg.volume.step, 10);
        assert_eq!(cfg.volume.max_volume, 150);
        assert_eq!(cfg.brightness.step, 10);
        assert_eq!(cfg.spacer.size, 5);
    }

    #[test]
    fn test_custom_module_order_parsing() {
        let toml_str = r#"
            [modules]
            order = ["clock", "battery", "volume"]
        "#;
        let cfg: AppConfig = toml::from_str(toml_str).expect("Valid modules order TOML");
        assert_eq!(cfg.modules.order, vec!["clock", "battery", "volume"]);
        assert_eq!(cfg.modules.start_modules(), vec!["spacer", "taskbar"]);
        assert_eq!(cfg.modules.end_modules(), vec!["clock", "battery", "volume"]);
    }

    #[test]
    fn test_modules_start_and_end_parsing() {
        let toml_str = r#"
            [modules]
            start = ["spacer", "taskbar"]
            end = ["tray", "clock"]
        "#;
        let cfg: AppConfig = toml::from_str(toml_str).expect("Valid modules start/end TOML");
        assert_eq!(cfg.modules.start_modules(), vec!["spacer", "taskbar"]);
        assert_eq!(cfg.modules.end_modules(), vec!["tray", "clock"]);
    }

    #[test]
    fn test_modules_default_start_and_end() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.modules.start_modules(), vec!["spacer", "taskbar"]);
        assert_eq!(
            cfg.modules.end_modules(),
            vec![
                "tray",
                "volume",
                "bluetooth",
                "network",
                "memory",
                "brightness",
                "battery",
                "clock"
            ]
        );
    }

    #[test]
    fn test_module_click_actions_parsing() {
        let toml_str = r#"
            [volume]
            on_click_left = "pavucontrol"
            on_click_right = "wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle"
            on_click_middle = "playerctl play-pause"

            [bluetooth]
            on_click_left = "blueman-manager"
            on_click_right = "bluetoothctl power toggle"

            [network]
            on_click_left = "nm-connection-editor"

            [battery]
            on_click_left = "xfce4-power-manager-settings"

            [memory]
            on_click_left = "htop"

            [brightness]
            on_click_left = "brightnessctl set 50%"

            [clock]
            on_click_left = "gnome-calendar"

            [spacer]
            on_click_left = "notify-send spacer"
        "#;
        let cfg: AppConfig = toml::from_str(toml_str).expect("Valid click actions TOML");
        assert_eq!(cfg.volume.click.on_click_left.as_deref(), Some("pavucontrol"));
        assert_eq!(
            cfg.volume.click.on_click_right.as_deref(),
            Some("wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle")
        );
        assert_eq!(
            cfg.volume.click.on_click_middle.as_deref(),
            Some("playerctl play-pause")
        );
        assert_eq!(cfg.bluetooth.click.on_click_left.as_deref(), Some("blueman-manager"));
        assert_eq!(
            cfg.bluetooth.click.on_click_right.as_deref(),
            Some("bluetoothctl power toggle")
        );
        assert_eq!(cfg.network.click.on_click_left.as_deref(), Some("nm-connection-editor"));
        assert_eq!(
            cfg.battery.click.on_click_left.as_deref(),
            Some("xfce4-power-manager-settings")
        );
        assert_eq!(cfg.memory.click.on_click_left.as_deref(), Some("htop"));
        assert_eq!(
            cfg.brightness.click.on_click_left.as_deref(),
            Some("brightnessctl set 50%")
        );
        assert_eq!(cfg.clock.click.on_click_left.as_deref(), Some("gnome-calendar"));
        assert_eq!(cfg.spacer.click.on_click_left.as_deref(), Some("notify-send spacer"));
    }

    #[test]
    fn test_volume_sink_specific_levels() {
        let toml_str = r#"
            [volume]
            [volume.levels]
            0 = "\n0%"
            1 = "\n%v%"
            [volume.levels_bluetooth]
            0 = "󰂲\n0%"
            1 = "󰂱\n%v%"
            [volume.levels_headphones]
            0 = "󰟎\n0%"
            1 = "󰋋\n%v%"
            [volume.levels_hdmi]
            0 = "󰡁\n0%"
            1 = "󰡁\n%v%"
        "#;
        let cfg: AppConfig = toml::from_str(toml_str).expect("Valid sink levels TOML");
        assert!(cfg.volume.get_levels_for_sink(true, false, false).is_some());
        assert!(cfg.volume.get_levels_for_sink(false, true, false).is_some());
        assert!(cfg.volume.get_levels_for_sink(false, false, true).is_some());
        assert!(cfg.volume.get_levels_for_sink(false, false, false).is_none());

        let bt_levels = cfg.volume.get_levels_for_sink(true, false, false).unwrap();
        let (bt_res, _) = resolve_level(bt_levels, 50, None);
        assert_eq!(bt_res, "󰂱\n50%");
    }

    #[test]
    fn test_empty_levels_numeric_resolution_fallback() {
        let empty_levels = HashMap::new();
        let (res, threshold) = resolve_level(&empty_levels, 42, None);
        assert_eq!(res, "42%");
        assert_eq!(threshold, None);
    }

    #[test]
    fn test_resolve_mode_fallback_unknown_mode() {
        let modes = HashMap::new();
        let res = resolve_mode(&modes, "custom_unconfigured_mode", &[("%s", "test")]);
        assert_eq!(res, "custom_unconfigured_mode");
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn test_validated_clamps_and_swaps() {
        let mut cfg = AppConfig::default();
        cfg.size = -5;
        cfg.taskbar.icon_size = 0;
        cfg.tray.icon_size = -1;
        cfg.battery.warning_threshold = 10;
        cfg.battery.critical_threshold = 30;
        let v = cfg.validated();
        assert_eq!(v.size, 50);
        assert_eq!(v.taskbar.icon_size, 32);
        assert_eq!(v.tray.icon_size, 22);
        assert!(v.battery.warning_threshold > v.battery.critical_threshold);
    }

    #[test]
    fn test_per_section_fallback_keeps_good_sections() {
        let table: toml::Table = toml::from_str(
            r#"
            position = "centre"
            size = 60
            [volume]
            step = 5
            max_volume = 150
            [battery]
            warning_threshold = "high"
            "#,
        )
        .expect("test TOML parses as generic table");
        let cfg = parse_config_table(&table);
        // Bad position falls back, good scalars/sections survive.
        assert_eq!(cfg.position, BarPosition::Left);
        assert_eq!(cfg.size, 60);
        assert_eq!(cfg.volume.step, 5);
        assert_eq!(cfg.volume.max_volume, 150);
        // Bad battery section falls back to defaults.
        assert_eq!(cfg.battery.warning_threshold, 30);
    }

    #[test]
    fn test_section_fallback_is_per_field() {
        // One bad field costs one field, not the section: valid `step = 5`
        // survives the invalid `max_volume`, which falls back to 150.
        let table: toml::Table = toml::from_str(
            r#"
            [volume]
            step = 5
            max_volume = "loud"
            "#,
        )
        .expect("test TOML parses as generic table");
        let cfg = parse_config_table(&table);
        assert_eq!(cfg.volume.step, 5);
        assert_eq!(cfg.volume.max_volume, 150);
    }

    #[test]
    fn test_clock_on_click_survives_parsing() {
        // Regression test: a user's calendar command must survive
        // per-section parsing + validation and reach the click handler.
        let table: toml::Table = toml::from_str(
            r#"
            [clock]
            format_vertical = "%H:%M"
            on_click_left = "xdg-open https://calendar.google.com/calendar"
            "#,
        )
        .expect("test TOML parses as generic table");
        let cfg = parse_config_table(&table);
        let (left, _, _) = cfg.clock.click.commands();
        assert_eq!(left, Some("xdg-open https://calendar.google.com/calendar"));

        use crate::modules::BarModule;
        let clock = crate::modules::clock::ClockModule::new(cfg.clock);
        assert!(clock.is_clickable());
        let (left2, _, _) = clock.click_commands();
        assert_eq!(left2, Some("xdg-open https://calendar.google.com/calendar"));
    }

    #[test]
    fn test_reload_config_from_str() {
        let valid_toml = r#"
            position = "top"
            size = 40
        "#;
        let res = reload_config_from_str(valid_toml);
        assert!(res.is_ok());
        let cfg = res.unwrap();
        assert_eq!(cfg.position, BarPosition::Top);
        assert_eq!(cfg.size, 40);

        let invalid_toml = r#"
            position = [unclosed
        "#;
        let err = reload_config_from_str(invalid_toml);
        assert!(err.is_err());
        let msg = err.unwrap_err();
        assert!(msg.to_lowercase().contains("error") || msg.to_lowercase().contains("invalid"));
    }

    #[test]
    fn test_load_config_with_error_uses_isolated_dir() {
        // Must not touch the real ~/.config: exercise the dir-parameterized
        // loader in a temp dir (no env mutation needed).
        let tmp = std::env::temp_dir().join(format!("niri_cfg_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let dir = tmp.join("niri-bar");
        let (cfg, _err) = load_config_with_error_in(&dir);
        assert!(!cfg.modules.start_modules().is_empty());
        // First launch writes defaults into the isolated dir only.
        assert!(dir.join("config.toml").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
