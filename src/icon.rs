use gdk::prelude::*;
use gdk_pixbuf::Pixbuf;
use gtk::prelude::*;
use gtk::Image;
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

fn load_desktop_entries() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let data_dirs = get_data_dirs();

    for data_dir in data_dirs {
        let app_dir = data_dir.join("applications");
        if !app_dir.exists() {
            continue;
        }

        if let Ok(entries) = fs::read_dir(app_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("desktop") {
                    if let Ok(entry_parsed) = freedesktop_desktop_entry::DesktopEntry::from_path(&path, None::<&[&str]>)
                    {
                        if let Some(icon) = entry_parsed.icon() {
                            let icon_str = icon.to_string();
                            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                                map.entry(stem.to_lowercase()).or_insert_with(|| icon_str.clone());
                            }
                            if let Some(startup_wm_class) = entry_parsed.startup_wm_class() {
                                map.entry(startup_wm_class.to_lowercase())
                                    .or_insert_with(|| icon_str.clone());
                            }
                            if let Some(name) = entry_parsed.name::<&str>(&[]) {
                                map.entry(name.to_lowercase()).or_insert_with(|| icon_str.clone());
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
    std::thread::spawn(|| {
        let mut entries_lock = crate::util::lock(desktop_entries());
        entries_lock.get_or_insert_with(load_desktop_entries);
    });
}

fn resolve_icon_path_or_name(icon: &str) -> Option<String> {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawIconPixmap {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u8>,
}

/// Find the non-transparent bounding box in a pixbuf to crop empty borders.
pub fn find_non_transparent_bbox(pb: &Pixbuf) -> (i32, i32, i32, i32) {
    let w = pb.width();
    let h = pb.height();
    if w <= 0 || h <= 0 || !pb.has_alpha() {
        return (0, 0, (w - 1).max(0), (h - 1).max(0));
    }

    let n_channels = pb.n_channels() as usize;
    let rowstride = pb.rowstride() as usize;
    let pixel_bytes = pb.read_pixel_bytes();
    let pixels = pixel_bytes.as_ref();

    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = -1;
    let mut max_y = -1;

    for y in 0..h {
        let row_start = (y as usize) * rowstride;
        for x in 0..w {
            let offset = row_start + (x as usize) * n_channels;
            if offset + 3 < pixels.len() {
                let alpha = pixels[offset + 3];
                if alpha > 10 {
                    if x < min_x {
                        min_x = x;
                    }
                    if x > max_x {
                        max_x = x;
                    }
                    if y < min_y {
                        min_y = y;
                    }
                    if y > max_y {
                        max_y = y;
                    }
                }
            }
        }
    }

    if max_x >= min_x && max_y >= min_y {
        (min_x, min_y, max_x, max_y)
    } else {
        (0, 0, (w - 1).max(0), (h - 1).max(0))
    }
}

/// Scale and center a pixbuf into a square of exact dimension `target_size`,
/// trimming excess transparent margins.
pub fn normalize_pixbuf_to_size(pb: Pixbuf, target_size: i32) -> Pixbuf {
    let w = pb.width();
    let h = pb.height();
    if w <= 0 || h <= 0 || target_size <= 0 {
        return pb;
    }

    let (min_x, min_y, max_x, max_y) = find_non_transparent_bbox(&pb);
    let (crop_x, crop_y, crop_w, crop_h) = if max_x >= min_x && max_y >= min_y {
        let cw = (max_x - min_x + 1).max(1);
        let ch = (max_y - min_y + 1).max(1);
        (min_x, min_y, cw, ch)
    } else {
        (0, 0, w, h)
    };

    let cropped = if crop_x == 0 && crop_y == 0 && crop_w == w && crop_h == h {
        pb
    } else {
        match Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, crop_w, crop_h) {
            Some(canvas) => {
                pb.copy_area(crop_x, crop_y, crop_w, crop_h, &canvas, 0, 0);
                canvas
            }
            None => pb,
        }
    };

    let glyph_max = if target_size >= 16 {
        target_size - 2
    } else {
        target_size
    };
    let cw = cropped.width();
    let ch = cropped.height();

    let (scaled_w, scaled_h) = if cw >= ch {
        let sw = glyph_max;
        let sh = ((ch as f64 / cw as f64) * glyph_max as f64).round().max(1.0) as i32;
        (sw, sh)
    } else {
        let sh = glyph_max;
        let sw = ((cw as f64 / ch as f64) * glyph_max as f64).round().max(1.0) as i32;
        (sw, sh)
    };

    let scaled = cropped
        .scale_simple(scaled_w, scaled_h, gdk_pixbuf::InterpType::Bilinear)
        .unwrap_or(cropped);

    if let Some(canvas) = Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, target_size, target_size) {
        canvas.fill(0x00000000);
        let offset_x = (target_size - scaled_w) / 2;
        let offset_y = (target_size - scaled_h) / 2;
        scaled.copy_area(0, 0, scaled_w, scaled_h, &canvas, offset_x, offset_y);
        canvas
    } else {
        scaled
    }
}

/// Convert ARGB32 raw byte streams (e.g. from StatusNotifierItem D-Bus) to a `Pixbuf`.
pub fn pixmap_to_pixbuf(pixmaps: &[RawIconPixmap]) -> Option<Pixbuf> {
    const MAX_DIM: i32 = 512;
    if pixmaps.is_empty() {
        return None;
    }

    // Pick largest area within caps (not just width).
    let best = pixmaps
        .iter()
        .filter(|p| p.width > 0 && p.height > 0 && p.width <= MAX_DIM && p.height <= MAX_DIM)
        .max_by_key(|p| (p.width as u64) * (p.height as u64))?;

    if best.pixels.is_empty() {
        return None;
    }

    let expected_len = match (best.width as u64)
        .checked_mul(best.height as u64)
        .and_then(|n| n.checked_mul(4))
    {
        Some(n) if n <= (isize::MAX as u64) && n == best.pixels.len() as u64 => n as usize,
        _ => {
            log_debug!(
                "icon",
                "Rejecting pixmap {}x{} (len {})",
                best.width,
                best.height,
                best.pixels.len()
            );
            return None;
        }
    };
    let mut rgba = Vec::with_capacity(expected_len);
    for chunk in best.pixels.chunks_exact(4) {
        let a = chunk[0];
        let r = chunk[1];
        let g = chunk[2];
        let b = chunk[3];
        rgba.push(r);
        rgba.push(g);
        rgba.push(b);
        rgba.push(a);
    }

    if rgba.len() != expected_len {
        return None;
    }

    Some(Pixbuf::from_mut_slice(
        rgba,
        gdk_pixbuf::Colorspace::Rgb,
        true,
        8,
        best.width,
        best.height,
        best.width * 4,
    ))
}

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

/// Load a pixbuf directly at a target size from desktop entries, theme, or path.
pub fn load_pixbuf_at_size(icon_key: &str, target_size: i32) -> Option<Pixbuf> {
    let size = if target_size <= 0 { 32 } else { target_size }.min(512);

    // 1. Direct absolute path (only from trusted taskbar app_ids; tray
    // callers must sanitize first — absolute tray paths are rejected).
    if Path::new(icon_key).is_absolute() {
        if let Ok(pb) = Pixbuf::from_file_at_scale(icon_key, size, size, true) {
            return Some(normalize_pixbuf_to_size(pb, size));
        }
        return None;
    }
    let safe_key = sanitize_icon_name(icon_key)?;

    // 2. Resolve via desktop entries
    if let Some(resolved) = find_app_icon(&safe_key) {
        if Path::new(&resolved).is_absolute() {
            if let Ok(pb) = Pixbuf::from_file_at_scale(&resolved, size, size, true) {
                return Some(normalize_pixbuf_to_size(pb, size));
            }
        } else if let Some(safe_resolved) = sanitize_icon_name(&resolved) {
            if let Some(theme) = gtk::IconTheme::default() {
                if let Ok(Some(pb)) = theme.load_icon(
                    &safe_resolved,
                    size,
                    gtk::IconLookupFlags::FORCE_SIZE | gtk::IconLookupFlags::GENERIC_FALLBACK,
                ) {
                    return Some(normalize_pixbuf_to_size(pb, size));
                }
            }
        }
    }

    // 3. Look up in GTK icon theme directly
    if let Some(theme) = gtk::IconTheme::default() {
        if let Ok(Some(pb)) = theme.load_icon(
            &safe_key,
            size,
            gtk::IconLookupFlags::FORCE_SIZE | gtk::IconLookupFlags::GENERIC_FALLBACK,
        ) {
            return Some(normalize_pixbuf_to_size(pb, size));
        }
        let lower = safe_key.to_lowercase();
        if sanitize_icon_name(&lower).is_some() {
            if let Ok(Some(pb)) = theme.load_icon(
                &lower,
                size,
                gtk::IconLookupFlags::FORCE_SIZE | gtk::IconLookupFlags::GENERIC_FALLBACK,
            ) {
                return Some(normalize_pixbuf_to_size(pb, size));
            }
        }
        if let Ok(Some(pb)) = theme.load_icon(
            "application-x-executable",
            size,
            gtk::IconLookupFlags::FORCE_SIZE | gtk::IconLookupFlags::GENERIC_FALLBACK,
        ) {
            return Some(normalize_pixbuf_to_size(pb, size));
        }
    }

    None
}

/// Create a new `gtk::Image` representing an application, properly scaled for HiDPI.
pub fn create_app_image(app_id: &str, logical_size: i32, scale_factor: i32) -> Image {
    let img = Image::new();
    img.set_halign(gtk::Align::Center);
    img.set_valign(gtk::Align::Center);
    update_app_image(&img, app_id, logical_size, scale_factor);
    img.show();
    img
}

/// Update an existing `gtk::Image` widget for an application with HiDPI support.
pub fn update_app_image(img: &Image, app_id: &str, logical_size: i32, scale_factor: i32) {
    let size = if logical_size <= 0 { 32 } else { logical_size };
    let scale = scale_factor.max(1);

    fn set_pixbuf(img: &Image, path: &str, size: i32, scale: i32) -> bool {
        let load_size = size * scale;
        if let Ok(pixbuf) = Pixbuf::from_file_at_scale(path, load_size, load_size, true) {
            let square = normalize_pixbuf_to_size(pixbuf, load_size);
            if let Some(surface) = square.create_surface(scale, Option::<&gdk::Window>::None) {
                img.set_from_surface(Some(&surface));
            } else {
                img.set_from_pixbuf(Some(&square));
            }
            img.set_pixel_size(size);
            return true;
        }
        false
    }

    // 1. Direct absolute file path
    if Path::new(app_id).is_absolute() && Path::new(app_id).exists() && set_pixbuf(img, app_id, size, scale) {
        return;
    }

    // 2. Resolve via desktop entry cache
    let resolved_opt = find_app_icon(app_id);
    if let Some(ref resolved) = resolved_opt {
        if Path::new(resolved).is_absolute() && Path::new(resolved).exists() && set_pixbuf(img, resolved, size, scale) {
            return;
        }
    }

    // 3. Named theme icon (GTK dynamically renders vectors at monitor scale)
    if let Some(theme) = gtk::IconTheme::default() {
        if let Some(ref resolved) = resolved_opt {
            if theme.has_icon(resolved) {
                img.set_from_icon_name(Some(resolved), gtk::IconSize::Button);
                img.set_pixel_size(size);
                return;
            }
        }

        if theme.has_icon(app_id) {
            img.set_from_icon_name(Some(app_id), gtk::IconSize::Button);
            img.set_pixel_size(size);
            return;
        }

        let lower = app_id.to_lowercase();
        if theme.has_icon(&lower) {
            img.set_from_icon_name(Some(&lower), gtk::IconSize::Button);
            img.set_pixel_size(size);
            return;
        }
    }

    // 4. Fallback to pixbuf loader
    let load_size = size * scale;
    if let Some(pixbuf) = load_pixbuf_at_size(app_id, load_size) {
        let square = normalize_pixbuf_to_size(pixbuf, load_size);
        if let Some(surface) = square.create_surface(scale, Option::<&gdk::Window>::None) {
            img.set_from_surface(Some(&surface));
        } else {
            img.set_from_pixbuf(Some(&square));
        }
        img.set_pixel_size(size);
        return;
    }

    // 5. Generic executable fallback
    img.set_from_icon_name(Some("application-x-executable"), gtk::IconSize::Button);
    img.set_pixel_size(size);
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
        if let Some(name) = icon_name {
            if !name.trim().is_empty() {
                // 2a. Check icon_theme_path directory directly
                if let Some(theme_path) = icon_theme_path {
                    if !theme_path.trim().is_empty() {
                        let dir = Path::new(theme_path);
                        for ext in &["", ".png", ".svg", ".ico", ".xpm", ".tga"] {
                            let filename = format!("{}{}", name, ext);
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
                                    if let Ok(pb) =
                                        Pixbuf::from_file_at_scale(candidate, target_size, target_size, true)
                                    {
                                        return Some(pb);
                                    } else if let Ok(pb) = Pixbuf::from_file(candidate) {
                                        return Some(pb);
                                    }
                                }
                            }
                        }
                    }
                }

                // 2b. Check if icon_name itself is an absolute path
                let path = Path::new(name);
                if path.is_absolute() && path.exists() {
                    if let Ok(pb) = Pixbuf::from_file_at_scale(path, target_size, target_size, true) {
                        return Some(pb);
                    } else if let Ok(pb) = Pixbuf::from_file(path) {
                        return Some(pb);
                    }
                }

                // 2c. Standard GTK icon theme & search paths
                if let Some(pb) = load_pixbuf_at_size(name, target_size) {
                    return Some(pb);
                }
            }
        }

        // 3. Fallback: Check item.id as icon name
        if !id.trim().is_empty() {
            if let Some(pb) = load_pixbuf_at_size(id, target_size) {
                return Some(pb);
            }
            let lower_id = id.to_lowercase();
            if let Some(pb) = load_pixbuf_at_size(&lower_id, target_size) {
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

    // If icon_theme_path is NOT specified, and icon_pixmap is empty,
    // check if it's an exact named icon in the system theme so GTK can render vector icons natively.
    // Note: Do NOT use GENERIC_FALLBACK lookup here because set_from_icon_name does not use
    // generic fallback suffix stripping at render time, and if icon_theme_path is provided,
    // the application's custom directory must take precedence.
    let theme = gtk::IconTheme::default();
    let is_theme_icon = icon_pixmap.is_none()
        && icon_theme_path.is_none()
        && icon_name
            .is_some_and(|name| !Path::new(name).is_absolute() && theme.as_ref().is_some_and(|t| t.has_icon(name)));

    if is_theme_icon {
        if let Some(name) = icon_name {
            image.set_from_icon_name(Some(name), gtk::IconSize::Button);
            image.set_pixel_size(size);
            return;
        }
    }

    let target_size = size * scale;
    if let Some(pb) = resolve_tray_pixbuf_scaled(icon_name, icon_pixmap, icon_theme_path, id, target_size) {
        if let Some(surface) = pb.create_surface(scale, Option::<&gdk::Window>::None) {
            image.set_from_surface(Some(&surface));
        } else {
            image.set_from_pixbuf(Some(&pb));
        }
        image.set_pixel_size(size);
    } else {
        image.set_from_icon_name(Some("application-x-executable"), gtk::IconSize::Button);
        image.set_pixel_size(size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_pixmap_rejects_oversize() {
        let huge = RawIconPixmap {
            width: 99999,
            height: 99999,
            pixels: vec![0; 16],
        };
        assert!(pixmap_to_pixbuf(&[huge]).is_none());
        let mismatch = RawIconPixmap {
            width: 2,
            height: 2,
            pixels: vec![0; 4],
        };
        assert!(pixmap_to_pixbuf(&[mismatch]).is_none());
    }

    #[test]
    fn test_normalize_pixbuf_to_size() {
        let pb = Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 48, 48).unwrap();
        let normalized = normalize_pixbuf_to_size(pb, 22);
        assert_eq!(normalized.width(), 22);
        assert_eq!(normalized.height(), 22);
    }

    #[test]
    fn test_pixmap_to_pixbuf_conversion() {
        let raw = RawIconPixmap {
            width: 2,
            height: 2,
            // ARGB32 format: A=255, R=100, G=150, B=200
            pixels: vec![
                255, 100, 150, 200, 255, 100, 150, 200, 255, 100, 150, 200, 255, 100, 150, 200,
            ],
        };
        let pb = pixmap_to_pixbuf(&[raw]);
        assert!(pb.is_some());
        let pb = pb.unwrap();
        assert_eq!(pb.width(), 2);
        assert_eq!(pb.height(), 2);
    }

    #[test]
    fn test_create_app_image_theme_icon() {
        if gtk::is_initialized() {
            if !gtk::is_initialized_main_thread() {
                eprintln!("Skipping GTK test: running on non-main GTK thread");
                return;
            }
        } else if gtk::init().is_err() {
            eprintln!("Skipping GTK test: display not available");
            return;
        }

        let img = create_app_image("application-x-executable", 32, 2);
        assert_eq!(img.storage_type(), gtk::ImageType::IconName);
        assert_eq!(img.pixel_size(), 32);
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
    fn test_find_non_transparent_bbox() {
        let pb = Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 10, 10).unwrap();
        pb.fill(0x00000000);
        let (min_x, min_y, max_x, max_y) = find_non_transparent_bbox(&pb);
        assert_eq!((min_x, min_y, max_x, max_y), (0, 0, 9, 9));
    }

    #[test]
    fn test_update_tray_image_with_icon_theme_path() {
        if gtk::is_initialized() {
            if !gtk::is_initialized_main_thread() {
                eprintln!("Skipping GTK test: running on non-main GTK thread");
                return;
            }
        } else if gtk::init().is_err() {
            eprintln!("Skipping GTK test: display not available");
            return;
        }

        let temp_dir = std::env::temp_dir().join("niri_bar_tray_test");
        let _ = std::fs::create_dir_all(&temp_dir);
        let test_icon_path = temp_dir.join("toolbox-tray-color.png");

        let pb = Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 22, 22).unwrap();
        pb.fill(0xff0000ff);
        pb.savev(&test_icon_path, "png", &[]).unwrap();

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

        let _ = std::fs::remove_file(&test_icon_path);
        let _ = std::fs::remove_dir(&temp_dir);

        // Before fix: image storage type was IconName (holding "toolbox-tray-color" which GTK fails to render)
        // With fix: image storage type should be Surface or Pixbuf (loaded from icon_theme_path)
        assert!(
            img.storage_type() == gtk::ImageType::Surface || img.storage_type() == gtk::ImageType::Pixbuf,
            "Expected Surface or Pixbuf from icon_theme_path, but got {:?}",
            img.storage_type()
        );
    }

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
}
