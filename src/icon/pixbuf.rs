use gdk_pixbuf::Pixbuf;
use std::fs;
use std::path::Path;

pub(crate) const MAX_ASAR_ICON_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawIconPixmap {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u8>,
}

/// Verify that extracted bytes start with known safe image magic signatures (PNG, ICO, SVG/XML).
pub(crate) fn has_valid_image_magic(bytes: &[u8]) -> bool {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        return true;
    }
    let prefix_len = bytes.len().min(512);
    if let Ok(head) = std::str::from_utf8(&bytes[..prefix_len]) {
        let trimmed = head.trim_start();
        if trimmed.starts_with("<svg") || trimmed.starts_with("<?xml") {
            return true;
        }
    }
    false
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

/// Decode PNG bytes directly in-process via `cairo::ImageSurface::create_from_png` (`libpng16`),
/// completely bypassing `gdk-pixbuf`'s `libglycin` / `bwrap` sandboxed loader subprocesses and threads.
/// Safe to call from any thread even before `gdk::init`.
pub fn decode_png_bytes_to_pixbuf(png_bytes: &[u8], target_size: i32) -> Option<Pixbuf> {
    if !png_bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let mut cursor = std::io::Cursor::new(png_bytes);
    let mut surface = cairo::ImageSurface::create_from_png(&mut cursor).ok()?;
    let w = surface.width();
    let h = surface.height();
    let stride = surface.stride();
    if w <= 0 || h <= 0 || w > 4096 || h > 4096 || stride < w * 4 {
        return None;
    }
    let format = surface.format();
    if !matches!(format, cairo::Format::ARgb32 | cairo::Format::Rgb24) {
        return None;
    }
    let has_alpha = format == cairo::Format::ARgb32;
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    {
        let data = surface.data().ok()?;
        for y in 0..h as usize {
            let row = &data[y * (stride as usize)..y * (stride as usize) + (w as usize) * 4];
            for px in row.chunks_exact(4) {
                let raw = u32::from_ne_bytes([px[0], px[1], px[2], px[3]]);
                let a = if has_alpha { ((raw >> 24) & 0xff) as u8 } else { 0xff };
                let r_pre = (raw >> 16) & 0xff;
                let g_pre = (raw >> 8) & 0xff;
                let b_pre = raw & 0xff;
                let (r, g, b) = if a == 0 {
                    (0, 0, 0)
                } else if a == 255 {
                    (r_pre as u8, g_pre as u8, b_pre as u8)
                } else {
                    let a32 = u32::from(a);
                    (
                        ((r_pre * 255 + a32 / 2) / a32).min(255) as u8,
                        ((g_pre * 255 + a32 / 2) / a32).min(255) as u8,
                        ((b_pre * 255 + a32 / 2) / a32).min(255) as u8,
                    )
                };
                rgba.push(r);
                rgba.push(g);
                rgba.push(b);
                rgba.push(a);
            }
        }
    }
    let pb = Pixbuf::from_mut_slice(rgba, gdk_pixbuf::Colorspace::Rgb, true, 8, w, h, w * 4);
    let size = if target_size <= 0 { 32 } else { target_size }.min(512);
    if w > size || h > size {
        let (sw, sh) = if w >= h {
            (size, ((h as f64 / w as f64) * size as f64).round().max(1.0) as i32)
        } else {
            (((w as f64 / h as f64) * size as f64).round().max(1.0) as i32, size)
        };
        pb.scale_simple(sw, sh, gdk_pixbuf::InterpType::Bilinear).or(Some(pb))
    } else {
        Some(pb)
    }
}

/// Load a file into a `Pixbuf` at `target_size`, enforcing `MAX_ASAR_ICON_BYTES` (4 MB)
/// and regular-file checks before loading. Uses fast in-process Cairo `libpng16`
/// decoding for `.png` files so `libglycin` / `bwrap` is never spawned for trusted PNGs.
pub(crate) fn load_file_pixbuf_fast(path: &Path, target_size: i32) -> Option<Pixbuf> {
    let size = if target_size <= 0 { 32 } else { target_size }.min(512);
    let meta = path.metadata().ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_ASAR_ICON_BYTES {
        return None;
    }
    if let Ok(bytes) = fs::read(path) {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            if let Some(pb) = decode_png_bytes_to_pixbuf(&bytes, size) {
                return Some(pb);
            }
        }
    }
    Pixbuf::from_file_at_scale(path, size, size, true)
        .ok()
        .or_else(|| Pixbuf::from_file(path).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icon::tests_common::TestTempDir;

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
    fn test_find_non_transparent_bbox() {
        let pb = Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 10, 10).unwrap();
        pb.fill(0x00000000);
        let (min_x, min_y, max_x, max_y) = find_non_transparent_bbox(&pb);
        assert_eq!((min_x, min_y, max_x, max_y), (0, 0, 9, 9));
    }

    #[test]
    fn test_decode_png_via_cairo_never_spawns_glycin_threads() {
        let surf = cairo::ImageSurface::create(cairo::Format::ARgb32, 32, 32).unwrap();
        let mut png_bytes = Vec::new();
        surf.write_to_png(&mut png_bytes).unwrap();

        let before = crate::runtime::min_thread_count();
        let pb = decode_png_bytes_to_pixbuf(&png_bytes, 24);
        assert!(pb.is_some(), "Should decode PNG via cairo in-process");
        let pb = pb.unwrap();
        assert_eq!(pb.width(), 24);
        assert_eq!(pb.height(), 24);

        let has_glycin_thread = std::fs::read_dir("/proc/self/task")
            .unwrap()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path().join("comm")).ok())
            .any(|name| name.starts_with("gly-"));
        assert!(
            !has_glycin_thread,
            "Decoding PNG via cairo must not spawn any glycin threads (threads before: {before})"
        );
    }

    #[test]
    fn test_decode_rgb24_png_via_cairo() {
        let surf = cairo::ImageSurface::create(cairo::Format::Rgb24, 16, 16).unwrap();
        let ctx = cairo::Context::new(&surf).unwrap();
        ctx.set_source_rgb(0.8, 0.2, 0.4);
        ctx.paint().unwrap();
        let mut png_bytes = Vec::new();
        surf.write_to_png(&mut png_bytes).unwrap();

        let pb = decode_png_bytes_to_pixbuf(&png_bytes, 16).expect("Rgb24 PNG must decode via cairo fast path");
        assert_eq!(pb.width(), 16);
        assert_eq!(pb.height(), 16);
    }

    #[test]
    fn test_load_file_pixbuf_fast_rejects_oversize_file() {
        let temp_dir = TestTempDir::new("oversize_file");
        let huge_file = temp_dir.join("huge.png");
        {
            let f = std::fs::File::create(&huge_file).unwrap();
            f.set_len(MAX_ASAR_ICON_BYTES + 1024).unwrap();
        }
        let res = load_file_pixbuf_fast(&huge_file, 32);
        assert!(
            res.is_none(),
            "Oversized icon file (> 4MB) must be rejected without falling through"
        );
    }
}
