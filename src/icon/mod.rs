mod asar;
mod desktop;
mod exe;
mod pixbuf;

pub use desktop::{find_app_icon, preload_desktop_entries_async};
pub use exe::{load_pixbuf_from_exe_path, load_pixbuf_from_pid, warm_window_icons_off_thread};
pub use pixbuf::{normalize_pixbuf_to_size, pixmap_to_pixbuf, RawIconPixmap};

use exe::app_id_exe_cache;
use gdk_pixbuf::Pixbuf;
use gtk::prelude::*;
use gtk::Image;
use pixbuf::load_file_pixbuf_fast;
use std::path::Path;

/// Sanitize a D-Bus/tray icon name: reject absolute paths from untrusted
/// sources, path separators, `..`, NUL, and overlong names. Returns the
/// basename-safe name or `None`.
pub fn sanitize_icon_name(name: &str) -> Option<String> {
    if name.is_empty() || name.len() > 128 || name.contains('\0') {
        return None;
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return None;
    }
    Some(name.to_string())
}

/// Look up `key` (or its `_`-delimited prefix, e.g. `Antigravity_status_icon_1` -> `antigravity`)
/// in `APP_ID_EXE_CACHE`.
fn lookup_cached_exe_pixbuf(safe_key: &str, size: i32) -> Option<Pixbuf> {
    let lower = safe_key.to_lowercase();
    let prefix = lower.split('_').next().unwrap_or("");
    let candidates = [safe_key, lower.as_str(), prefix];
    for cand in candidates {
        if cand.is_empty() {
            continue;
        }
        let cached_exe = {
            let map = crate::util::lock(app_id_exe_cache());
            map.get(cand).cloned().flatten()
        };
        if let Some(exe_path) = cached_exe {
            if let Some(pb) = load_pixbuf_from_exe_path(&exe_path, cand, size) {
                return Some(pb);
            }
        }
    }
    None
}

/// Load an unnormalized pixbuf at a target size from desktop entries, XDG icon dirs, or exe cache.
/// Never initializes `GtkIconTheme` or `libglycin`, avoiding ~12 MB RSS and 14 background threads.
fn load_raw_pixbuf_at_size(icon_key: &str, target_size: i32) -> Option<Pixbuf> {
    let size = if target_size <= 0 { 32 } else { target_size }.min(512);

    // 1. Direct absolute path (only from trusted taskbar app_ids; tray
    // callers must sanitize first — absolute tray paths are rejected).
    if Path::new(icon_key).is_absolute() {
        return load_file_pixbuf_fast(Path::new(icon_key), size);
    }
    let safe_key = sanitize_icon_name(icon_key)?;

    // 2. Resolve via desktop entries
    if let Some(resolved) = find_app_icon(&safe_key) {
        if Path::new(&resolved).is_absolute() {
            if let Some(pb) = load_file_pixbuf_fast(Path::new(&resolved), size) {
                return Some(pb);
            }
        } else if let Some(png_path) = desktop::resolve_xdg_png_icon(&resolved) {
            if let Some(pb) = load_file_pixbuf_fast(&png_path, size) {
                return Some(pb);
            }
        }
    }

    // 3. Direct XDG raster `.png` lookup (bypasses GtkIconTheme + libglycin)
    let lower = safe_key.to_lowercase();
    for candidate in [safe_key.as_str(), lower.as_str()] {
        if let Some(png_path) = desktop::resolve_xdg_png_icon(candidate) {
            if let Some(pb) = load_file_pixbuf_fast(&png_path, size) {
                return Some(pb);
            }
        }
    }

    // 4. Check if `app_id` (or tray prefix like `Antigravity_status_icon_1`) was mapped to an executable path via PID
    if let Some(pb) = lookup_cached_exe_pixbuf(&safe_key, size) {
        return Some(pb);
    }

    // 5. Generic executable fallback (in-process, no libglycin)
    pixbuf::fallback_executable_pixbuf(size)
}

/// Load a normalized square pixbuf directly at a target size from desktop entries, theme, or path.
pub fn load_pixbuf_at_size(icon_key: &str, target_size: i32) -> Option<Pixbuf> {
    let size = if target_size <= 0 { 32 } else { target_size }.min(512);
    load_raw_pixbuf_at_size(icon_key, size).map(|pb| normalize_pixbuf_to_size(pb, size))
}

fn apply_pixbuf_to_image(img: &Image, pixbuf: &Pixbuf, logical_size: i32, _scale: i32) {
    let texture = gdk::Texture::for_pixbuf(pixbuf);
    img.set_paintable(Some(&texture));
    img.set_pixel_size(logical_size);
}

/// Create a new `gtk::Image` representing an application, properly scaled for HiDPI.
#[cfg(test)]
pub fn create_app_image(app_id: &str, logical_size: i32, scale_factor: i32) -> Image {
    create_app_image_with_pid(app_id, None, logical_size, scale_factor)
}

/// Create a new `gtk::Image` representing an application with optional process `pid` fallback.
pub fn create_app_image_with_pid(app_id: &str, pid: Option<u32>, logical_size: i32, scale_factor: i32) -> Image {
    let img = Image::new();
    img.set_halign(gtk::Align::Center);
    img.set_valign(gtk::Align::Center);
    update_app_image_with_pid(&img, app_id, pid, logical_size, scale_factor);
    img
}

/// Update an existing `gtk::Image` widget for an application with optional process `pid` fallback.
pub fn update_app_image_with_pid(img: &Image, app_id: &str, pid: Option<u32>, logical_size: i32, scale_factor: i32) {
    let size = if logical_size <= 0 { 32 } else { logical_size };
    let scale = scale_factor.max(1);
    let load_size = size * scale;

    let set_pixbuf_from_file = |path: &Path| -> bool {
        if let Some(pixbuf) = load_file_pixbuf_fast(path, load_size) {
            let square = normalize_pixbuf_to_size(pixbuf, load_size);
            apply_pixbuf_to_image(img, &square, size, scale);
            return true;
        }
        false
    };

    // 1. Direct absolute file path
    let app_path = Path::new(app_id);
    if app_path.is_absolute() && app_path.exists() && set_pixbuf_from_file(app_path) {
        return;
    }

    // 2. Resolve via desktop entry cache + direct XDG raster `.png` lookup (zero GtkIconTheme/libglycin overhead)
    let resolved_opt = find_app_icon(app_id);
    if let Some(ref resolved) = resolved_opt {
        let res_path = Path::new(resolved);
        if res_path.is_absolute() && res_path.exists() && set_pixbuf_from_file(res_path) {
            return;
        }
        if let Some(png_path) = desktop::resolve_xdg_png_icon(resolved) {
            if set_pixbuf_from_file(&png_path) {
                return;
            }
        }
    }
    let lower = app_id.to_lowercase();
    for candidate in [app_id, lower.as_str()] {
        if let Some(png_path) = desktop::resolve_xdg_png_icon(candidate) {
            if set_pixbuf_from_file(&png_path) {
                return;
            }
        }
    }

    // 3. Inspect process executable directory (`PID` -> `/proc/<pid>/exe`) for portable apps / Electron `.asar`
    if let Some(p) = pid {
        if let Some(square) = load_pixbuf_from_pid(p, app_id, load_size) {
            apply_pixbuf_to_image(img, &square, size, scale);
            return;
        }
    } else if let Some(safe_key) = sanitize_icon_name(app_id) {
        if let Some(square) = lookup_cached_exe_pixbuf(&safe_key, load_size) {
            apply_pixbuf_to_image(img, &square, size, scale);
            return;
        }
    }

    // 4. Generic executable fallback (in-process memory texture, never triggers GtkIconTheme/libglycin)
    if let Some(fallback) = pixbuf::fallback_executable_pixbuf(load_size) {
        apply_pixbuf_to_image(img, &fallback, size, scale);
    }
}

/// Resolve tray icon pixbuf from all StatusNotifierItem sources.
pub fn resolve_tray_pixbuf_scaled(
    icon_name: Option<&str>,
    icon_pixmap: Option<&[RawIconPixmap]>,
    icon_theme_path: Option<&str>,
    id: &str,
    target_size: i32,
) -> Option<Pixbuf> {
    let unscaled: Option<Pixbuf> = (|| {
        // 1. Direct ARGB32 pixmap stream from D-Bus
        if let Some(pixmaps) = icon_pixmap {
            if let Some(pb) = pixmap_to_pixbuf(pixmaps) {
                return Some(pb);
            }
        }

        // 2. Named icon lookup
        if let Some(name) = icon_name.map(str::trim).filter(|n| !n.is_empty()) {
            // 2a. Check icon_theme_path directory directly (must be a safe basename)
            if let Some(safe_name) = sanitize_icon_name(name) {
                if let Some(theme_path) = icon_theme_path.map(str::trim).filter(|p| !p.is_empty()) {
                    let dir = Path::new(theme_path);
                    for ext in &["", ".png", ".svg", ".ico", ".xpm", ".tga"] {
                        let filename = format!("{safe_name}{ext}");
                        let candidates = [
                            dir.join(&filename),
                            dir.join("hicolor").join("scalable").join("apps").join(&filename),
                            dir.join("hicolor").join("48x48").join("apps").join(&filename),
                            dir.join("hicolor").join("32x32").join("apps").join(&filename),
                            dir.join("hicolor").join("24x24").join("apps").join(&filename),
                            dir.join("hicolor").join("22x22").join("apps").join(&filename),
                            dir.join("hicolor").join("16x16").join("apps").join(&filename),
                        ];
                        for candidate in &candidates {
                            if candidate.exists() {
                                if let Some(pb) = load_file_pixbuf_fast(candidate, target_size) {
                                    return Some(pb);
                                }
                            }
                        }
                    }
                }
            }

            // 2b. Check if icon_name itself is an absolute path (without traversal)
            let path = Path::new(name);
            if path.is_absolute() && !name.contains("..") && !name.contains('\0') && path.exists() {
                if let Some(pb) = load_file_pixbuf_fast(path, target_size) {
                    return Some(pb);
                }
            }

            // 2c. Standard GTK icon theme & search paths
            if let Some(safe_name) = sanitize_icon_name(name) {
                if let Some(pb) = load_raw_pixbuf_at_size(&safe_name, target_size) {
                    return Some(pb);
                }
            }
        }

        // 3. Fallback: Check item.id as icon name
        if let Some(safe_id) = sanitize_icon_name(id.trim()) {
            if let Some(pb) = load_raw_pixbuf_at_size(&safe_id, target_size) {
                return Some(pb);
            }
        }

        None
    })();

    unscaled.map(|pb| normalize_pixbuf_to_size(pb, target_size))
}

/// Update an existing `gtk::Image` widget for a StatusNotifierItem tray item.
pub fn update_tray_image(
    image: &Image,
    icon_name: Option<&str>,
    icon_pixmap: Option<&[RawIconPixmap]>,
    icon_theme_path: Option<&str>,
    id: &str,
    icon_size: i32,
    scale_factor: i32,
) {
    let scale = scale_factor.max(1);
    let size = if icon_size <= 0 { 22 } else { icon_size };
    let target_size = size * scale;

    if let Some(pb) = resolve_tray_pixbuf_scaled(icon_name, icon_pixmap, icon_theme_path, id, target_size) {
        apply_pixbuf_to_image(image, &pb, size, scale);
    } else if let Some(fallback) = pixbuf::fallback_executable_pixbuf(target_size) {
        apply_pixbuf_to_image(image, &fallback, size, scale);
    }
}

#[cfg(test)]
pub(crate) mod tests_common {
    use std::fs;
    use std::path::{Path, PathBuf};

    pub(crate) struct TestTempDir(PathBuf);

    impl TestTempDir {
        pub(crate) fn new(prefix: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("niri_bar_test_{prefix}_{pid}_{id}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl std::ops::Deref for TestTempDir {
        type Target = Path;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn write_test_png(path: &Path, w: i32, h: i32) {
        let surf = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).unwrap();
        let ctx = cairo::Context::new(&surf).unwrap();
        ctx.set_source_rgba(0.2, 0.4, 0.8, 1.0);
        ctx.paint().unwrap();
        let mut file = fs::File::create(path).unwrap();
        surf.write_to_png(&mut file).unwrap();
    }

    pub(crate) fn build_fake_asar(files_json: &str, payload: &[u8]) -> Vec<u8> {
        let str_len = files_json.len() as u32;
        let padded_str_len = (str_len + 3) & !3;
        let u2 = 4 + padded_str_len;
        let u1 = 4 + u2;
        let mut out = Vec::new();
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&u1.to_le_bytes());
        out.extend_from_slice(&u2.to_le_bytes());
        out.extend_from_slice(&str_len.to_le_bytes());
        out.extend_from_slice(files_json.as_bytes());
        out.resize(16 + padded_str_len as usize, 0);
        out.extend_from_slice(payload);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tests_common::{write_test_png, TestTempDir};

    #[test]
    fn test_sanitize_icon_name() {
        assert_eq!(sanitize_icon_name("firefox"), Some("firefox".to_string()));
        assert_eq!(sanitize_icon_name(""), None);
        assert_eq!(sanitize_icon_name("/etc/passwd"), None);
        assert_eq!(sanitize_icon_name("../x"), None);
        assert_eq!(sanitize_icon_name("a/b"), None);
        assert_eq!(sanitize_icon_name(&"x".repeat(200)), None);
    }

    #[test]
    fn test_resolve_tray_pixbuf_rejects_path_traversal_in_theme_path() {
        let parent_dir = TestTempDir::new("traversal");
        let sub_dir = parent_dir.join("theme_subdir");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let outside_icon = parent_dir.join("outside_secret.png");

        write_test_png(&outside_icon, 22, 22);

        let res = resolve_tray_pixbuf_scaled(Some("../outside_secret"), None, Some(sub_dir.to_str().unwrap()), "", 22);

        assert!(
            res.is_none(),
            "resolve_tray_pixbuf_scaled must reject '../' path traversal in icon_name"
        );
    }

    #[test]
    fn test_create_app_image_theme_icon() {
        crate::util::run_gtk_test(|| {
            let img = create_app_image("application-x-executable", 32, 2);
            assert_eq!(img.storage_type(), gtk::ImageType::Paintable);
            assert_eq!(img.pixel_size(), 32);
        });
    }

    #[test]
    fn test_resolve_tray_pixbuf_scaled() {
        let pixmaps = vec![RawIconPixmap {
            width: 48,
            height: 48,
            pixels: vec![255; 48 * 48 * 4],
        }];
        let pb = resolve_tray_pixbuf_scaled(None, Some(&pixmaps), None, "test-app", 44);
        assert!(pb.is_some());
        let p = pb.unwrap();
        assert_eq!(p.width(), 44);
        assert_eq!(p.height(), 44);
    }

    #[test]
    fn test_update_tray_image_with_icon_theme_path() {
        crate::util::run_gtk_test(|| {
            let temp_dir = TestTempDir::new("tray");
            let test_icon_path = temp_dir.join("toolbox-tray-color.png");

            write_test_png(&test_icon_path, 22, 22);

            let img = Image::new();
            update_tray_image(
                &img,
                Some("toolbox-tray-color"),
                None,
                Some(temp_dir.to_str().unwrap()),
                "toolbox",
                22,
                1,
            );

            assert_eq!(
                img.storage_type(),
                gtk::ImageType::Paintable,
                "Expected Paintable from icon_theme_path, but got {:?}",
                img.storage_type()
            );
        });
    }
}
