use gdk_pixbuf::Pixbuf;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use super::asar::extract_icon_bytes_from_asar;
use super::desktop::{find_app_icon, parse_desktop_entry_fields};
use super::pixbuf::{
    decode_png_bytes_to_pixbuf, has_valid_image_magic, load_file_pixbuf_fast, normalize_pixbuf_to_size,
    MAX_ASAR_ICON_BYTES,
};
use super::sanitize_icon_name;

const MAX_DESKTOP_FILE_BYTES: u64 = 256 * 1024;
const MAX_EXE_CACHE_ENTRIES: usize = 256;

#[derive(Clone, Debug)]
enum CachedExeIcon {
    None,
    FilePath(PathBuf),
    MemoryBytes(Vec<u8>),
}

static EXE_ICON_CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedExeIcon>>> = OnceLock::new();
static APP_ID_EXE_CACHE: OnceLock<Mutex<HashMap<String, Option<PathBuf>>>> = OnceLock::new();

fn exe_icon_cache() -> &'static Mutex<HashMap<PathBuf, CachedExeIcon>> {
    EXE_ICON_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn app_id_exe_cache() -> &'static Mutex<HashMap<String, Option<PathBuf>>> {
    APP_ID_EXE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve `/proc/<pid>/exe` to a clean executable path on Linux.
fn resolve_pid_exe_path(pid: u32) -> Option<PathBuf> {
    if pid == 0 {
        return None;
    }
    let link = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let s = link.to_string_lossy();
    if let Some(stripped) = s.strip_suffix(" (deleted)") {
        Some(PathBuf::from(stripped))
    } else {
        Some(link)
    }
}

/// Inspect an executable's installation directory for colocated icons, `.desktop` files,
/// or Electron `resources/app.asar` bundles.
fn discover_exe_icon(exe_path: &Path, app_id: &str) -> CachedExeIcon {
    let Some(exe_dir) = exe_path.parent() else {
        return CachedExeIcon::None;
    };
    // Avoid scanning system binary directories (`/usr/bin`, `/bin`, etc.)
    if matches!(
        exe_dir.to_str(),
        Some("/usr/bin" | "/bin" | "/usr/sbin" | "/sbin" | "/usr/local/bin" | "/usr/games")
    ) {
        return CachedExeIcon::None;
    }

    let exe_stem = exe_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    let app_lower = app_id.to_lowercase();

    let mut search_dirs = vec![exe_dir.to_path_buf()];
    if exe_dir.file_name().and_then(|n| n.to_str()) == Some("bin") {
        if let Some(parent) = exe_dir.parent() {
            if !matches!(parent.to_str(), Some("/usr" | "/usr/local" | "/")) {
                search_dirs.push(parent.to_path_buf());
            }
        }
    }

    for dir in &search_dirs {
        // 1. Direct colocated icon files (`<app_id>.{svg,png}`, `<exe_stem>.{svg,png}`, `.DirIcon`, `icon.{svg,png}`)
        for stem in [app_lower.as_str(), exe_stem.as_str(), "icon"] {
            if let Some(safe_stem) = sanitize_icon_name(stem) {
                for ext in ["svg", "png", "xpm", "ico"] {
                    let candidate = dir.join(format!("{safe_stem}.{ext}"));
                    if candidate.is_file() {
                        return CachedExeIcon::FilePath(candidate);
                    }
                }
            }
        }
        let dir_icon = dir.join(".DirIcon");
        if dir_icon.is_file() {
            if let Ok(mut f) = fs::File::open(&dir_icon) {
                if let Ok(meta) = f.metadata() {
                    if meta.is_file() && meta.len() > 0 && meta.len() <= MAX_ASAR_ICON_BYTES {
                        let mut head = [0u8; 512];
                        let n = f.read(&mut head).unwrap_or(0);
                        if n > 0 && has_valid_image_magic(&head[..n]) {
                            return CachedExeIcon::FilePath(dir_icon);
                        }
                    }
                }
            }
        }

        // 2. Colocated `.desktop` files in the application's directory
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten().take(64) {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("desktop") && p.is_file() {
                    if let Ok(meta) = p.metadata() {
                        if meta.len() <= MAX_DESKTOP_FILE_BYTES {
                            if let Ok(content) = fs::read_to_string(&p) {
                                if let Some((icon_val, _, _)) = parse_desktop_entry_fields(&content) {
                                    if let Some(safe_icon) = sanitize_icon_name(&icon_val) {
                                        for ext in ["", ".svg", ".png", ".xpm"] {
                                            let candidate = dir.join(format!("{safe_icon}{ext}"));
                                            if candidate.is_file() {
                                                return CachedExeIcon::FilePath(candidate);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // 3. Unpacked Electron resources (`resources/app/icon.png` or `resources/icon.png`)
        for rel in [
            "resources/icon.png",
            "resources/icon.svg",
            "resources/app/icon.png",
            "resources/app/icon.svg",
            "resources/app.asar.unpacked/icon.png",
        ] {
            let candidate = dir.join(rel);
            if candidate.is_file() {
                return CachedExeIcon::FilePath(candidate);
            }
        }

        // 4. Packed Electron `resources/app.asar`
        let asar_path = dir.join("resources").join("app.asar");
        if asar_path.is_file() {
            if let Some(bytes) = extract_icon_bytes_from_asar(&asar_path) {
                return CachedExeIcon::MemoryBytes(bytes);
            }
        }
    }

    CachedExeIcon::None
}

#[cfg(test)]
fn has_cached_exe_decision(app_id: &str) -> bool {
    let map = crate::util::lock(app_id_exe_cache());
    map.contains_key(app_id)
}

/// Populate `EXE_ICON_CACHE` and `APP_ID_EXE_CACHE` for `(exe_path, app_id)` without
/// touching any GTK objects (safe to call from background Tokio worker threads).
fn warm_exe_icon_cache_for_path(exe_path: &Path, app_id: &str) -> CachedExeIcon {
    let cached = {
        let cache = crate::util::lock(exe_icon_cache());
        cache.get(exe_path).cloned()
    };

    let resolved = match cached {
        Some(c) => c,
        None => {
            let found = discover_exe_icon(exe_path, app_id);
            let mut cache = crate::util::lock(exe_icon_cache());
            if cache.len() >= MAX_EXE_CACHE_ENTRIES {
                cache.clear();
            }
            cache.insert(exe_path.to_path_buf(), found.clone());
            found
        }
    };

    if !app_id.is_empty() {
        let mut map = crate::util::lock(app_id_exe_cache());
        if map.len() >= MAX_EXE_CACHE_ENTRIES {
            map.clear();
        }
        match resolved {
            CachedExeIcon::None => {
                // Only record a negative entry if we don't already have a working positive exe_path for this app_id
                if !matches!(map.get(app_id), Some(Some(_))) {
                    map.insert(app_id.to_string(), None);
                }
            }
            _ => {
                map.insert(app_id.to_string(), Some(exe_path.to_path_buf()));
            }
        }
    }

    resolved
}

/// Pre-warm executable icon caches for a slice of Niri windows on the background
/// Tokio worker before nudging the GTK main thread.
pub fn warm_window_icons_off_thread(windows: &[crate::niri::model::WindowInfo]) {
    for w in windows {
        let Some(ref app_id) = w.app_id else {
            continue;
        };
        let Some(pid) = w.pid else {
            continue;
        };
        // Ensure desktop entries are loaded off-thread first
        let _ = find_app_icon(app_id);
        if let Some(exe_path) = resolve_pid_exe_path(pid) {
            let _ = warm_exe_icon_cache_for_path(&exe_path, app_id);
        }
    }
}

/// Load a normalized square `Pixbuf` from a resolved executable path (cached per executable).
pub fn load_pixbuf_from_exe_path(exe_path: &Path, app_id: &str, target_size: i32) -> Option<Pixbuf> {
    let size = if target_size <= 0 { 32 } else { target_size }.min(512);
    let resolved = warm_exe_icon_cache_for_path(exe_path, app_id);

    let raw_pb = match resolved {
        CachedExeIcon::None => None,
        CachedExeIcon::FilePath(path) => load_file_pixbuf_fast(&path, size),
        CachedExeIcon::MemoryBytes(bytes) => {
            if let Some(pb) = decode_png_bytes_to_pixbuf(&bytes, size) {
                Some(pb)
            } else {
                let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from(&bytes));
                Pixbuf::from_stream_at_scale(&stream, size, size, true, Option::<&gio::Cancellable>::None).ok()
            }
        }
    }?;

    Some(normalize_pixbuf_to_size(raw_pb, size))
}

/// Load a normalized square `Pixbuf` for a process `pid` via `/proc/<pid>/exe`.
/// Always resolves the live `pid`'s executable first so another process claiming
/// the same `app_id` cannot permanently spoof or poison the icon cache.
pub fn load_pixbuf_from_pid(pid: u32, app_id: &str, target_size: i32) -> Option<Pixbuf> {
    if let Some(exe_path) = resolve_pid_exe_path(pid) {
        if let Some(pb) = load_pixbuf_from_exe_path(&exe_path, app_id, target_size) {
            return Some(pb);
        }
    }
    if !app_id.is_empty() {
        let cached_exe = {
            let map = crate::util::lock(app_id_exe_cache());
            map.get(app_id).cloned().flatten()
        };
        if let Some(exe_path) = cached_exe {
            return load_pixbuf_from_exe_path(&exe_path, app_id, target_size);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icon::tests_common::{build_fake_asar, write_test_png, TestTempDir};

    #[test]
    fn test_resolve_icon_from_exe_dir_colocated_and_asar() {
        crate::util::run_gtk_test(|| {
            let temp_dir = TestTempDir::new("exe_dir");

            // Case 1: Portable app directory with colocated `<stem>.svg` or `<stem>.png` (like Blender)
            let blender_dir = temp_dir.join("blender-portable");
            std::fs::create_dir_all(&blender_dir).unwrap();
            let blender_exe = blender_dir.join("blender");
            std::fs::write(&blender_exe, b"").unwrap();
            let blender_icon = blender_dir.join("blender.png");
            write_test_png(&blender_icon, 48, 48);

            let loaded_colocated = load_pixbuf_from_exe_path(&blender_exe, "blender", 32);
            assert!(
                loaded_colocated.is_some(),
                "Should load colocated icon next to executable"
            );

            // Case 2: Portable Electron app directory with `resources/app.asar` (like Antigravity 2)
            let electron_dir = temp_dir.join("Antigravity-x64");
            let resources_dir = electron_dir.join("resources");
            std::fs::create_dir_all(&resources_dir).unwrap();
            let electron_exe = electron_dir.join("antigravity");
            std::fs::write(&electron_exe, b"").unwrap();

            let png_bytes = std::fs::read(&blender_icon).unwrap();
            let header = format!(
                r#"{{"files":{{"icon.png":{{"size":{},"offset":"0"}}}}}}"#,
                png_bytes.len()
            );
            let asar_bytes = build_fake_asar(&header, &png_bytes);
            std::fs::write(resources_dir.join("app.asar"), &asar_bytes).unwrap();

            let loaded_asar = load_pixbuf_from_exe_path(&electron_exe, "antigravity", 32);
            assert!(loaded_asar.is_some(), "Should load icon from resources/app.asar");
        });
    }

    #[test]
    fn test_preload_window_icons_caches_negative_and_positive() {
        let temp_dir = TestTempDir::new("preload");
        let app_dir = temp_dir.join("custom-app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let exe_path = app_dir.join("custom-bin");
        std::fs::write(&exe_path, b"").unwrap();
        let icon_path = app_dir.join("icon.png");
        write_test_png(&icon_path, 24, 24);

        warm_exe_icon_cache_for_path(&exe_path, "custom-preload-app");
        assert!(has_cached_exe_decision("custom-preload-app"));

        // System binary path should also record a negative decision so `/proc/<pid>/exe` is never polled again
        warm_exe_icon_cache_for_path(Path::new("/usr/bin/env"), "system-env-test-app");
        assert!(has_cached_exe_decision("system-env-test-app"));
    }

    #[test]
    fn test_exe_icon_spoofing_overwritten_by_genuine_exe() {
        let temp_dir = TestTempDir::new("spoof");

        // 1. Impostor or no-icon process in `/usr/bin/env` claims `app_id = "spoofable-app"` first
        warm_exe_icon_cache_for_path(Path::new("/usr/bin/env"), "spoofable-app");

        // 2. Genuine portable app with the same `app_id = "spoofable-app"` starts from its real folder
        let real_dir = temp_dir.join("real-app");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real_exe = real_dir.join("spoofable-app");
        std::fs::write(&real_exe, b"").unwrap();
        let real_icon = real_dir.join("icon.png");
        write_test_png(&real_icon, 32, 32);

        let pb = load_pixbuf_from_exe_path(&real_exe, "spoofable-app", 24);
        assert!(
            pb.is_some(),
            "Genuine executable must load its icon even after an earlier negative cache entry"
        );

        // And `APP_ID_EXE_CACHE` must now point to the genuine executable
        let cached = crate::util::lock(app_id_exe_cache())
            .get("spoofable-app")
            .cloned()
            .flatten();
        assert_eq!(cached.as_deref(), Some(real_exe.as_path()));
    }

    #[test]
    fn test_discover_exe_icon_rejects_script_diricon_and_supports_arbitrary_package_json_icon() {
        let temp_dir = TestTempDir::new("diricon_and_pkg");
        let app_dir = temp_dir.join("app");
        let res_dir = app_dir.join("resources");
        std::fs::create_dir_all(&res_dir).unwrap();
        let exe_path = app_dir.join("my-electron-app");
        std::fs::write(&exe_path, b"").unwrap();

        // Non-image `.DirIcon` (e.g. a shell script) must be rejected
        std::fs::write(app_dir.join(".DirIcon"), b"#!/bin/sh\necho bad\n").unwrap();

        // `app.asar` with a custom `package.json` `"icon": "custom/sub/logo.png"`
        let tmp_png = temp_dir.join("logo.png");
        write_test_png(&tmp_png, 24, 24);
        let png_bytes = std::fs::read(&tmp_png).unwrap();
        let pkg_json = br#"{"name":"my-electron-app","icon":"./custom/sub/logo.png"}"#;
        let mut payload = Vec::new();
        payload.extend_from_slice(pkg_json);
        payload.extend_from_slice(&png_bytes);

        let header = format!(
            r#"{{"files":{{"package.json":{{"size":{},"offset":"0"}},"custom":{{"files":{{"sub":{{"files":{{"logo.png":{{"size":{},"offset":"{}"}}}}}}}}}}}}}}"#,
            pkg_json.len(),
            png_bytes.len(),
            pkg_json.len()
        );
        let asar_bytes = build_fake_asar(&header, &payload);
        std::fs::write(res_dir.join("app.asar"), &asar_bytes).unwrap();

        let pb = load_pixbuf_from_exe_path(&exe_path, "my-electron-app", 24);
        assert!(
            pb.is_some(),
            "Should skip non-image .DirIcon and resolve custom package.json icon path from app.asar"
        );
    }
}
