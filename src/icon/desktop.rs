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

fn get_data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let data_home = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")));

    if let Ok(ref user_data) = data_home {
        dirs.push(user_data.clone());
        dirs.push(user_data.join("flatpak/exports/share"));
    }
    if let Ok(xdg_data_dirs) = std::env::var("XDG_DATA_DIRS") {
        for dir in xdg_data_dirs.split(':') {
            if !dir.is_empty() {
                dirs.push(PathBuf::from(dir));
            }
        }
    } else {
        dirs.push(PathBuf::from("/usr/local/share"));
        dirs.push(PathBuf::from("/usr/share"));
        dirs.push(PathBuf::from("/var/lib/flatpak/exports/share"));
    }
    dirs
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
}
