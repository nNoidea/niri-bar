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

/// Unified icon request describing the target asset and resolution context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IconRequest<'a> {
    /// Window on the taskbar or drag preview
    Window { app_id: &'a str, pid: Option<u32> },
    /// StatusNotifierItem system tray item
    Tray {
        name: Option<&'a str>,
        pixmap: Option<&'a [RawIconPixmap]>,
        theme_path: Option<&'a str>,
        id: &'a str,
    },
    /// Simple app or icon name / path
    Named(&'a str),
}

/// Single Source of Truth (SSOT) for resolving any icon request (Taskbar Window, Tray item, or Named asset)
/// to a normalized square `Pixbuf` without spawning `GtkIconTheme` or `libglycin`.
pub fn resolve_icon_pixbuf(req: &IconRequest, target_size: i32) -> Option<Pixbuf> {
    let size = if target_size <= 0 { 32 } else { target_size }.min(512);

    // 1. Direct ARGB32 pixmap stream from D-Bus StatusNotifierItem
    if let IconRequest::Tray {
        pixmap: Some(pixmaps), ..
    } = req
    {
        if let Some(pb) = pixmap_to_pixbuf(pixmaps) {
            return Some(normalize_pixbuf_to_size(pb, size));
        }
    }

    // 2. Direct absolute file path check
    let direct_path = match req {
        IconRequest::Window { app_id, .. } => Some(Path::new(*app_id)),
        IconRequest::Named(name) => Some(Path::new(*name)),
        IconRequest::Tray { name: Some(n), .. } => {
            let p = Path::new(*n);
            if p.is_absolute() && !n.contains("..") && !n.contains('\0') {
                Some(p)
            } else {
                None
            }
        }
        _ => None,
    };
    if let Some(p) = direct_path {
        if p.is_absolute() && p.is_file() {
            if let Some(pb) = load_file_pixbuf_fast(p, size) {
                return Some(normalize_pixbuf_to_size(pb, size));
            }
        }
    }

    // 3. Extract resolution parameters
    let (primary_key, fallback_key, pid, theme_path) = match req {
        IconRequest::Window { app_id, pid } => (sanitize_icon_name(app_id), None, *pid, None),
        IconRequest::Named(name) => (sanitize_icon_name(name), None, None, None),
        IconRequest::Tray {
            name, theme_path, id, ..
        } => {
            let primary = name
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .and_then(sanitize_icon_name);
            let fallback = sanitize_icon_name(id.trim());
            let tp = theme_path.map(str::trim).filter(|p| !p.is_empty());
            (primary, fallback, None, tp)
        }
    };

    let resolve_key = |safe_key: &str| -> Option<Pixbuf> {
        // 3a. Custom theme directory search (for tray items)
        if let Some(tp) = theme_path {
            let dir = Path::new(tp);
            for ext in &["", ".png", ".svg", ".ico", ".xpm", ".tga"] {
                let candidate = dir.join(format!("{safe_key}{ext}"));
                if candidate.is_file() {
                    if let Some(pb) = load_file_pixbuf_fast(&candidate, size) {
                        return Some(normalize_pixbuf_to_size(pb, size));
                    }
                }
            }
            let xdg_dirs = [dir.to_path_buf(), dir.join("icons")];
            if let Some(cand) = desktop::resolve_xdg_icon_in_dirs(safe_key, &xdg_dirs) {
                if let Some(pb) = load_file_pixbuf_fast(&cand, size) {
                    return Some(normalize_pixbuf_to_size(pb, size));
                }
            }
        }

        // 3b. Resolve via desktop entries
        if let Some(resolved) = find_app_icon(safe_key) {
            let res_path = Path::new(&resolved);
            if res_path.is_absolute() && res_path.is_file() {
                if let Some(pb) = load_file_pixbuf_fast(res_path, size) {
                    return Some(normalize_pixbuf_to_size(pb, size));
                }
            } else if let Some(icon_path) = desktop::resolve_xdg_icon(&resolved) {
                if let Some(pb) = load_file_pixbuf_fast(&icon_path, size) {
                    return Some(normalize_pixbuf_to_size(pb, size));
                }
            }
        }

        // 3c. Direct XDG icon lookup (raster PNG first, then scalable SVG)
        let lower = safe_key.to_lowercase();
        let candidates = if lower != safe_key {
            vec![safe_key, lower.as_str()]
        } else {
            vec![safe_key]
        };
        for candidate in candidates {
            if let Some(icon_path) = desktop::resolve_xdg_icon(candidate) {
                if let Some(pb) = load_file_pixbuf_fast(&icon_path, size) {
                    return Some(normalize_pixbuf_to_size(pb, size));
                }
            }
        }

        // 3d. Process executable directory / Electron .asar heuristic
        if let Some(p) = pid {
            if let Some(square) = load_pixbuf_from_pid(p, safe_key, size) {
                return Some(square);
            }
        } else if let Some(square) = lookup_cached_exe_pixbuf(safe_key, size) {
            return Some(square);
        }

        None
    };

    if let Some(ref k) = primary_key {
        if let Some(pb) = resolve_key(k) {
            return Some(pb);
        }
    }
    if let Some(ref fk) = fallback_key {
        if let Some(pb) = resolve_key(fk) {
            return Some(pb);
        }
    }

    None
}

/// Apply a normalized `Pixbuf` texture to a `gtk::Image` widget at `logical_size`.
pub fn apply_pixbuf_to_image(img: &Image, pixbuf: &Pixbuf, logical_size: i32) {
    let texture = gdk::Texture::for_pixbuf(pixbuf);
    img.set_paintable(Some(&texture));
    img.set_pixel_size(logical_size);
}

/// Update an existing `gtk::Image` widget for any `IconRequest` with proper HiDPI scaling
/// and generic fallback handling.
pub fn update_image(img: &Image, req: &IconRequest, logical_size: i32, scale_factor: i32) {
    let size = if logical_size <= 0 { 32 } else { logical_size };
    let scale = scale_factor.max(1);
    let target_size = size * scale;

    let pb = resolve_icon_pixbuf(req, target_size).or_else(|| pixbuf::fallback_executable_pixbuf(target_size));

    if let Some(ref square) = pb {
        apply_pixbuf_to_image(img, square, size);
    }
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
    update_image(img, &IconRequest::Window { app_id, pid }, logical_size, scale_factor);
}

/// Load a normalized square pixbuf directly at a target size from desktop entries, theme, or path.
pub fn load_pixbuf_at_size(icon_key: &str, target_size: i32) -> Option<Pixbuf> {
    let size = if target_size <= 0 { 32 } else { target_size }.min(512);
    resolve_icon_pixbuf(&IconRequest::Named(icon_key), size).or_else(|| pixbuf::fallback_executable_pixbuf(size))
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
    update_image(
        image,
        &IconRequest::Tray {
            name: icon_name,
            pixmap: icon_pixmap,
            theme_path: icon_theme_path,
            id,
        },
        icon_size,
        scale_factor,
    );
}

/// Resolve tray icon pixbuf from all StatusNotifierItem sources.
#[cfg(test)]
pub fn resolve_tray_pixbuf_scaled(
    icon_name: Option<&str>,
    icon_pixmap: Option<&[RawIconPixmap]>,
    icon_theme_path: Option<&str>,
    id: &str,
    target_size: i32,
) -> Option<Pixbuf> {
    let size = if target_size <= 0 { 22 } else { target_size }.min(512);
    resolve_icon_pixbuf(
        &IconRequest::Tray {
            name: icon_name,
            pixmap: icon_pixmap,
            theme_path: icon_theme_path,
            id,
        },
        size,
    )
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

    #[test]
    fn test_load_pixbuf_for_scalable_svg_app() {
        let dir = TestTempDir::new("svg_app");
        let svg_dir = dir.join("icons/hicolor/scalable/apps");
        std::fs::create_dir_all(&svg_dir).unwrap();
        let svg_path = svg_dir.join("org.example.ScalableApp.svg");
        std::fs::write(
            &svg_path,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"><circle cx="16" cy="16" r="16" fill="purple"/></svg>"#,
        )
        .unwrap();

        let found = desktop::resolve_xdg_icon_in_dirs("org.example.ScalableApp", &[dir.to_path_buf()]);
        assert_eq!(found, Some(svg_path.clone()));

        let pb = pixbuf::load_file_pixbuf_fast(&svg_path, 32);
        assert!(pb.is_some(), "Pixbuf should successfully load SVG at 32x32");
        let square = pixbuf::normalize_pixbuf_to_size(pb.unwrap(), 32);
        assert_eq!(square.width(), 32);
        assert_eq!(square.height(), 32);
    }

    #[test]
    fn test_resolve_icon_pixbuf_unified_pipeline() {
        let dir = TestTempDir::new("ssot_test");
        let apps_dir = dir.join("icons/hicolor/scalable/apps");
        std::fs::create_dir_all(&apps_dir).unwrap();
        let svg_path = apps_dir.join("test.ssot.App.svg");
        std::fs::write(
            &svg_path,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"><rect width="32" height="32" fill="yellow"/></svg>"#,
        )
        .unwrap();

        // 1. Direct path request
        let req_direct = IconRequest::Named(svg_path.to_str().unwrap());
        let pb_direct = resolve_icon_pixbuf(&req_direct, 32);
        assert!(pb_direct.is_some(), "SSOT should resolve direct file path");

        // 2. Tray pixmap request
        let pixmaps = vec![RawIconPixmap {
            width: 16,
            height: 16,
            pixels: vec![255; 16 * 16 * 4],
        }];
        let req_tray = IconRequest::Tray {
            name: None,
            pixmap: Some(&pixmaps),
            theme_path: None,
            id: "tray-app",
        };
        let pb_tray = resolve_icon_pixbuf(&req_tray, 24);
        assert!(pb_tray.is_some(), "SSOT should resolve tray ARGB32 pixmaps");
    }
}
