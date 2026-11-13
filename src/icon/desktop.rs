use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static ICON_CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
static DESKTOP_ENTRIES: OnceLock<Mutex<Option<HashMap<String, String>>>> = OnceLock::new();

fn icon_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    ICON_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn desktop_entries() -> &'static Mutex<Option<HashMap<String, String>>> {
    DESKTOP_ENTRIES.get_or_init(|| Mutex::new(None))
}

fn resolve_data_dirs(user_data: Option<&Path>, xdg_data_dirs: Option<&str>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut push_unique = |p: PathBuf| {
        if !dirs.contains(&p) {
            dirs.push(p);
        }
    };

    if let Some(user_dir) = user_data {
        push_unique(user_dir.to_path_buf());
        push_unique(user_dir.join("flatpak/exports/share"));
    }
    if let Some(xdg) = xdg_data_dirs {
        for dir in xdg.split(':') {
            if !dir.is_empty() {
                push_unique(PathBuf::from(dir));
            }
        }
    } else {
        push_unique(PathBuf::from("/usr/local/share"));
        push_unique(PathBuf::from("/usr/share"));
        push_unique(PathBuf::from("/var/lib/flatpak/exports/share"));
    }
    let system_flatpak = PathBuf::from("/var/lib/flatpak/exports/share");
    if !dirs.contains(&system_flatpak) && system_flatpak.is_dir() {
        dirs.push(system_flatpak);
    }
    dirs
}

fn get_data_dirs() -> Vec<PathBuf> {
    let data_home = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok();
    let xdg_data_dirs = std::env::var("XDG_DATA_DIRS").ok();
    resolve_data_dirs(data_home.as_deref(), xdg_data_dirs.as_deref())
}

/// Extracts `(icon, startup_wm_class, name)` from the `[Desktop Entry]` group
/// without allocating maps for localized `Name[xx]`/`Comment[xx]` keys or actions.
pub fn parse_desktop_entry_fields(content: &str) -> Option<(String, Option<String>, Option<String>)> {
    let mut in_desktop_entry = false;
    let mut icon: Option<String> = None;
    let mut wm_class: Option<String> = None;
    let mut name: Option<String> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            if in_desktop_entry {
                break;
            }
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry {
            continue;
        }
        if let Some(val) = line.strip_prefix("Icon=") {
            let trimmed = val.trim();
            if !trimmed.is_empty() && icon.is_none() {
                icon = Some(trimmed.to_string());
            }
        } else if let Some(val) = line.strip_prefix("StartupWMClass=") {
            let trimmed = val.trim();
            if !trimmed.is_empty() && wm_class.is_none() {
                wm_class = Some(trimmed.to_string());
            }
        } else if let Some(val) = line.strip_prefix("Name=") {
            let trimmed = val.trim();
            if !trimmed.is_empty() && name.is_none() {
                name = Some(trimmed.to_string());
            }
        }
        if icon.is_some() && wm_class.is_some() && name.is_some() {
            break;
        }
    }

    icon.map(|ic| (ic, wm_class, name))
}

fn load_desktop_entries() -> HashMap<String, String> {
    use std::io::Read;
    let mut map = HashMap::new();
    let data_dirs = get_data_dirs();
    let mut buf = String::with_capacity(16 * 1024);

    for data_dir in data_dirs {
        let app_dir = data_dir.join("applications");
        if !app_dir.exists() {
            continue;
        }

        if let Ok(entries) = fs::read_dir(app_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("desktop") {
                    buf.clear();
                    if let Ok(mut file) = fs::File::open(&path) {
                        if file.read_to_string(&mut buf).is_ok() {
                            if let Some((icon_str, startup_wm_class, name)) = parse_desktop_entry_fields(&buf) {
                                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                                    map.entry(stem.to_lowercase()).or_insert_with(|| icon_str.clone());
                                }
                                if let Some(wm_class) = startup_wm_class {
                                    map.entry(wm_class.to_lowercase()).or_insert_with(|| icon_str.clone());
                                }
                                if let Some(app_name) = name {
                                    map.entry(app_name.to_lowercase()).or_insert(icon_str);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    map.shrink_to_fit();
    map
}

pub fn match_app_id_to_icon(app_id: &str, entries: &HashMap<String, String>) -> Option<String> {
    let key = app_id.to_lowercase();

    // 1. Direct match in desktop entries
    if let Some(icon_name) = entries.get(&key) {
        return resolve_icon_path_or_name(icon_name);
    }

    // 2. Prefix / suffix matches (e.g. org.mozilla.firefox -> firefox)
    if let Some(last_part) = key.rsplit('.').next() {
        if let Some(icon_name) = entries.get(last_part) {
            return resolve_icon_path_or_name(icon_name);
        }
    }

    // 3. Fallback: app_id itself might be the icon name
    resolve_icon_path_or_name(app_id)
}

pub fn find_app_icon(app_id: &str) -> Option<String> {
    const MAX_CACHE: usize = 512;
    // Fast path without holding any lock across I/O.
    {
        let cache = crate::util::lock(icon_cache());
        if let Some(cached) = cache.get(app_id) {
            return cached.clone();
        }
    }
    // Load entries without holding the icon cache lock (fixes
    // icon_cache -> desktop_entries nesting across filesystem I/O).
    let res = {
        let mut entries_lock = crate::util::lock(desktop_entries());
        let entries = entries_lock.get_or_insert_with(load_desktop_entries);
        match_app_id_to_icon(app_id, entries)
    };
    {
        let mut cache = crate::util::lock(icon_cache());
        // Bound growth: transient app_ids must not OOM a resident bar.
        if cache.len() >= MAX_CACHE {
            cache.clear();
        }
        cache.insert(app_id.to_string(), res.clone());
    }
    res
}

/// Preload desktop entries off the GTK thread at startup.
pub fn preload_desktop_entries_async() {
    crate::runtime::spawn(async {
        let mut entries_lock = crate::util::lock(desktop_entries());
        entries_lock.get_or_insert_with(load_desktop_entries);
    });
}

pub(crate) fn resolve_icon_path_or_name(icon: &str) -> Option<String> {
    let path = Path::new(icon);
    if path.is_absolute() && path.exists() {
        return Some(icon.to_string());
    }

    // Check pixmaps in data dirs
    for base in &["/usr/local/share/pixmaps", "/usr/share/pixmaps"] {
        for ext in &["png", "svg", "xpm"] {
            let p = PathBuf::from(base).join(format!("{icon}.{ext}"));
            if p.exists() {
                return Some(p.to_string_lossy().to_string());
            }
        }
    }

    Some(icon.to_string())
}

pub(crate) fn resolve_xdg_icon(name: &str) -> Option<PathBuf> {
    let safe = crate::icon::sanitize_icon_name(name)?;
    resolve_xdg_icon_in_dirs(&safe, &get_data_dirs())
}

const XDG_SIZES: &[&str] = &[
    "64x64", "48x48", "128x128", "32x32", "256x256", "512x512", "24x24", "22x22", "16x16",
];
const XDG_CATEGORIES: &[&str] = &[
    "apps",
    "status",
    "devices",
    "places",
    "mimetypes",
    "categories",
    "legacy",
];
const XDG_THEMES: &[&str] = &["hicolor", "AdwaitaLegacy", "Adwaita"];

fn find_in_theme_subdir(subdir: &Path, filenames: &[&str]) -> Option<PathBuf> {
    if !subdir.is_dir() {
        return None;
    }
    for cat in XDG_CATEGORIES {
        let cat_dir = subdir.join(cat);
        for file in filenames {
            let candidate = cat_dir.join(file);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn find_in_themes(data_dirs: &[PathBuf], mut probe_theme: impl FnMut(&Path) -> Option<PathBuf>) -> Option<PathBuf> {
    for data_dir in data_dirs {
        let icons_dir = data_dir.join("icons");
        if icons_dir.is_dir() {
            for theme in XDG_THEMES {
                let theme_dir = icons_dir.join(theme);
                if theme_dir.is_dir() {
                    if let Some(hit) = probe_theme(&theme_dir) {
                        return Some(hit);
                    }
                }
            }
        }
    }
    None
}

/// Search XDG data dirs (`hicolor`, `AdwaitaLegacy`, `Adwaita`, `pixmaps`) for an application icon.
///
/// 1. Searches for raster `.png` first across fixed sizes and pixmaps so it can be decoded
///    directly in-process via `libpng16` with zero background threads.
/// 2. If no raster PNG is found, searches `scalable/` (and fixed-size dirs / pixmaps) for vector
///    `.svg` / `.svgz` icons (used by many Flatpak and modern desktop apps).
/// 3. Falls back to case-insensitive (lowercased) match and symbolic icons.
pub(crate) fn resolve_xdg_icon_in_dirs(safe_name: &str, data_dirs: &[PathBuf]) -> Option<PathBuf> {
    let lower = safe_name.to_lowercase();
    let names = if lower != safe_name {
        vec![safe_name, lower.as_str()]
    } else {
        vec![safe_name]
    };

    // 1. Try raster PNG first (fast in-process decode via libpng16)
    for name in &names {
        if let Some(png) = resolve_xdg_png_icon_in_dirs(name, data_dirs) {
            return Some(png);
        }
    }

    // 2. Try scalable / vector SVG icons and pixmaps
    for name in &names {
        let scalable_files = [format!("{name}.svg"), format!("{name}.svgz"), format!("{name}.png")];
        let scalable_refs: Vec<&str> = scalable_files.iter().map(String::as_str).collect();
        let fixed_svg = [format!("{name}.svg")];
        let fixed_refs = [fixed_svg[0].as_str()];

        if let Some(hit) = find_in_themes(data_dirs, |theme_dir| {
            find_in_theme_subdir(&theme_dir.join("scalable"), &scalable_refs).or_else(|| {
                XDG_SIZES
                    .iter()
                    .find_map(|sz| find_in_theme_subdir(&theme_dir.join(sz), &fixed_refs))
            })
        }) {
            return Some(hit);
        }

        for data_dir in data_dirs {
            let pixmap_svg = data_dir.join("pixmaps").join(&fixed_svg[0]);
            if pixmap_svg.is_file() {
                return Some(pixmap_svg);
            }
        }
    }

    // 3. Fallback: symbolic icons
    for name in &names {
        let sym_files = [format!("{name}-symbolic.svg"), format!("{name}.svg")];
        let sym_refs = [sym_files[0].as_str(), sym_files[1].as_str()];
        if let Some(hit) = find_in_themes(data_dirs, |theme_dir| {
            find_in_theme_subdir(&theme_dir.join("symbolic"), &sym_refs)
        }) {
            return Some(hit);
        }
    }

    None
}

/// Search XDG data dirs (`hicolor`, `AdwaitaLegacy`, `pixmaps`) for a raster `.png` icon
/// matching `name` so it can be decoded directly in-process via `libpng16` without
/// initializing `GtkIconTheme` or `libglycin`.
pub(crate) fn resolve_xdg_png_icon(name: &str) -> Option<PathBuf> {
    let safe = crate::icon::sanitize_icon_name(name)?;
    resolve_xdg_png_icon_in_dirs(&safe, &get_data_dirs())
}

pub(crate) fn resolve_xdg_png_icon_in_dirs(safe_name: &str, data_dirs: &[PathBuf]) -> Option<PathBuf> {
    let filename = format!("{safe_name}.png");
    let refs = [filename.as_str()];
    for data_dir in data_dirs {
        let icons_dir = data_dir.join("icons");
        if icons_dir.is_dir() {
            for theme in XDG_THEMES {
                let theme_dir = icons_dir.join(theme);
                if theme_dir.is_dir() {
                    if let Some(hit) = XDG_SIZES
                        .iter()
                        .find_map(|sz| find_in_theme_subdir(&theme_dir.join(sz), &refs))
                    {
                        return Some(hit);
                    }
                }
            }
        }
        let pixmap = data_dir.join("pixmaps").join(&filename);
        if pixmap.is_file() {
            return Some(pixmap);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_match_app_id_direct() {
        let mut entries = HashMap::new();
        entries.insert("firefox".to_string(), "firefox-browser".to_string());
        entries.insert("kitty".to_string(), "kitty".to_string());

        let res = match_app_id_to_icon("firefox", &entries);
        assert_eq!(res, Some("firefox-browser".to_string()));

        let res_case = match_app_id_to_icon("Firefox", &entries);
        assert_eq!(res_case, Some("firefox-browser".to_string()));
    }

    #[test]
    fn test_match_app_id_reverse_dns() {
        let mut entries = HashMap::new();
        entries.insert("firefox".to_string(), "firefox-browser".to_string());
        entries.insert("studio".to_string(), "com.obsproject.Studio".to_string());

        let res = match_app_id_to_icon("org.mozilla.firefox", &entries);
        assert_eq!(res, Some("firefox-browser".to_string()));

        let res_obs = match_app_id_to_icon("com.obsproject.Studio", &entries);
        assert_eq!(res_obs, Some("com.obsproject.Studio".to_string()));
    }

    #[test]
    fn test_match_app_id_fallback_when_not_in_entries() {
        let entries = HashMap::new();
        let res = match_app_id_to_icon("alacritty", &entries);
        assert_eq!(res, Some("alacritty".to_string()));
    }

    #[test]
    fn test_resolve_icon_path_or_name() {
        let named = resolve_icon_path_or_name("utilities-terminal");
        assert_eq!(named, Some("utilities-terminal".to_string()));
    }

    #[test]
    fn test_parse_desktop_entry_fields() {
        let sample = "\
# Comment
[Desktop Entry]
Name=Firefox Web Browser
Name[de]=Firefox Webbrowser
StartupWMClass=firefox-aurora
Icon=firefox
Exec=firefox %u

[Desktop Action new-window]
Name=New Window
Icon=list-add
";
        let (icon, wm_class, name) = parse_desktop_entry_fields(sample).expect("must parse Desktop Entry");
        assert_eq!(icon, "firefox");
        assert_eq!(wm_class.as_deref(), Some("firefox-aurora"));
        assert_eq!(name.as_deref(), Some("Firefox Web Browser"));
    }

    #[test]
    fn test_resolve_xdg_png_icon_in_dirs() {
        let dir = crate::icon::tests_common::TestTempDir::new("xdg_png");
        let apps_dir = dir.join("icons/hicolor/48x48/apps");
        fs::create_dir_all(&apps_dir).unwrap();
        let png_path = apps_dir.join("org.example.App.png");
        crate::icon::tests_common::write_test_png(&png_path, 48, 48);

        let found = resolve_xdg_png_icon_in_dirs("org.example.App", &[dir.to_path_buf()]);
        assert_eq!(found, Some(png_path));
    }

    #[test]
    fn test_resolve_xdg_icon_finds_scalable_svg() {
        let dir = crate::icon::tests_common::TestTempDir::new("xdg_svg");
        let apps_dir = dir.join("icons/hicolor/scalable/apps");
        fs::create_dir_all(&apps_dir).unwrap();
        let svg_path = apps_dir.join("org.example.ScalableApp.svg");
        fs::write(&svg_path, r#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"><circle cx="16" cy="16" r="16" fill="blue"/></svg>"#).unwrap();

        let found = resolve_xdg_icon_in_dirs("org.example.ScalableApp", &[dir.to_path_buf()]);
        assert_eq!(found, Some(svg_path));
    }

    #[test]
    fn test_resolve_xdg_icon_prefers_raster_png_over_svg() {
        let dir = crate::icon::tests_common::TestTempDir::new("xdg_pref");
        let png_dir = dir.join("icons/hicolor/48x48/apps");
        let svg_dir = dir.join("icons/hicolor/scalable/apps");
        fs::create_dir_all(&png_dir).unwrap();
        fs::create_dir_all(&svg_dir).unwrap();
        let png_path = png_dir.join("org.example.App.png");
        let svg_path = svg_dir.join("org.example.App.svg");
        crate::icon::tests_common::write_test_png(&png_path, 48, 48);
        fs::write(&svg_path, "<svg></svg>").unwrap();

        let found = resolve_xdg_icon_in_dirs("org.example.App", &[dir.to_path_buf()]);
        assert_eq!(found, Some(png_path));
    }

    #[test]
    fn test_resolve_xdg_icon_case_fallback() {
        let dir = crate::icon::tests_common::TestTempDir::new("xdg_case");
        let svg_dir = dir.join("icons/hicolor/scalable/apps");
        fs::create_dir_all(&svg_dir).unwrap();
        let svg_path = svg_dir.join("com.example.app.svg");
        fs::write(&svg_path, "<svg></svg>").unwrap();

        let found = resolve_xdg_icon_in_dirs("com.example.App", &[dir.to_path_buf()]);
        assert_eq!(found, Some(svg_path));
    }

    #[test]
    fn test_resolve_data_dirs_deduplicates_xdg_entries() {
        let user_dir = PathBuf::from("/tmp/niri_test_data_home");
        let dirs = resolve_data_dirs(
            Some(&user_dir),
            Some("/usr/local/share:/usr/share:/usr/share::/tmp/niri_test_data_home"),
        );
        let share_count = dirs.iter().filter(|p| *p == Path::new("/usr/share")).count();
        let home_count = dirs.iter().filter(|p| *p == user_dir.as_path()).count();
        assert_eq!(share_count, 1, "Duplicate XDG_DATA_DIRS entries must be deduplicated");
        assert_eq!(
            home_count, 1,
            "User data dir repeated in XDG_DATA_DIRS must appear once"
        );
    }
}
