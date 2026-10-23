use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::pixbuf::{has_valid_image_magic, MAX_ASAR_ICON_BYTES};

const MAX_ASAR_HEADER_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Default, serde::Deserialize)]
struct AsarLeafEntry {
    #[serde(default)]
    size: Option<u64>,
    #[serde(default, deserialize_with = "de_opt_u64_or_str")]
    offset: Option<u64>,
    #[serde(default)]
    unpacked: Option<bool>,
}

fn de_opt_u64_or_str<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum NumOrStr {
        Num(u64),
        Str(String),
    }
    match <Option<NumOrStr> as serde::Deserialize>::deserialize(deserializer)? {
        Some(NumOrStr::Num(n)) => Ok(Some(n)),
        Some(NumOrStr::Str(s)) => Ok(s.parse::<u64>().ok()),
        None => Ok(None),
    }
}

#[derive(Default, serde::Deserialize)]
struct AsarSubDir {
    #[serde(default)]
    files: Option<AsarSubFiles>,
}

#[derive(Default, serde::Deserialize)]
struct AsarSubFiles {
    #[serde(rename = "icon.png", default)]
    icon_png: Option<AsarLeafEntry>,
    #[serde(rename = "icon.svg", default)]
    icon_svg: Option<AsarLeafEntry>,
}

/// Selective ASAR root header that skips `node_modules` and other large subtrees
/// at the serde token level without allocating `serde_json::Value` maps.
#[derive(Default, serde::Deserialize)]
struct AsarSelectiveRoot {
    #[serde(default)]
    files: Option<AsarTopFiles>,
}

#[derive(Default, serde::Deserialize)]
struct AsarTopFiles {
    #[serde(rename = "icon.png", default)]
    icon_png: Option<AsarLeafEntry>,
    #[serde(rename = "icon.svg", default)]
    icon_svg: Option<AsarLeafEntry>,
    #[serde(rename = "package.json", default)]
    package_json: Option<AsarLeafEntry>,
    #[serde(default)]
    assets: Option<AsarSubDir>,
    #[serde(default)]
    resources: Option<AsarSubDir>,
    #[serde(default)]
    build: Option<AsarSubDir>,
    #[serde(default)]
    dist: Option<AsarSubDir>,
}

#[derive(Default, serde::Deserialize)]
struct AsarPackageJson {
    #[serde(default)]
    icon: Option<String>,
}

/// Safely extract an application icon from an Electron `app.asar` archive without writing to disk.
///
/// Uses a zero-tree selective serde visitor (`AsarSelectiveRoot`) so large `node_modules`
/// trees in the JSON header are skipped in microseconds without heap allocations.
pub fn extract_icon_bytes_from_asar(asar_path: &Path) -> Option<Vec<u8>> {
    extract_icon_bytes_from_asar_selective(asar_path)
}

pub fn extract_icon_bytes_from_asar_selective(asar_path: &Path) -> Option<Vec<u8>> {
    let mut file = fs::File::open(asar_path).ok()?;
    let file_len = file.metadata().ok()?.len();
    if file_len < 16 {
        return None;
    }

    let mut pre = [0u8; 16];
    file.read_exact(&mut pre).ok()?;
    let u0 = u32::from_le_bytes([pre[0], pre[1], pre[2], pre[3]]);
    let u1 = u64::from(u32::from_le_bytes([pre[4], pre[5], pre[6], pre[7]]));
    let json_len = u64::from(u32::from_le_bytes([pre[12], pre[13], pre[14], pre[15]]));

    if u0 != 4 || json_len == 0 || json_len > MAX_ASAR_HEADER_BYTES || u1 > MAX_ASAR_HEADER_BYTES {
        return None;
    }
    let data_base = 8u64.checked_add(u1)?;
    if data_base > file_len || 16u64.checked_add(json_len)? > file_len {
        return None;
    }

    let mut json_buf = vec![0u8; json_len as usize];
    file.read_exact(&mut json_buf).ok()?;
    let parsed: AsarSelectiveRoot = serde_json::from_slice(&json_buf).ok()?;
    let top = parsed.files?;

    let resolve_leaf = |leaf: Option<&AsarLeafEntry>| -> Option<(u64, u64)> {
        let l = leaf?;
        if l.unpacked == Some(true) {
            return None;
        }
        Some((l.offset?, l.size?))
    };

    let lookup_static = |rel_path: &str| -> Option<(u64, u64)> {
        match rel_path {
            "icon.png" => resolve_leaf(top.icon_png.as_ref()),
            "icon.svg" => resolve_leaf(top.icon_svg.as_ref()),
            "assets/icon.png" => resolve_leaf(top.assets.as_ref()?.files.as_ref()?.icon_png.as_ref()),
            "assets/icon.svg" => resolve_leaf(top.assets.as_ref()?.files.as_ref()?.icon_svg.as_ref()),
            "resources/icon.png" => resolve_leaf(top.resources.as_ref()?.files.as_ref()?.icon_png.as_ref()),
            "resources/icon.svg" => resolve_leaf(top.resources.as_ref()?.files.as_ref()?.icon_svg.as_ref()),
            "build/icon.png" => resolve_leaf(top.build.as_ref()?.files.as_ref()?.icon_png.as_ref()),
            "build/icon.svg" => resolve_leaf(top.build.as_ref()?.files.as_ref()?.icon_svg.as_ref()),
            "dist/icon.png" => resolve_leaf(top.dist.as_ref()?.files.as_ref()?.icon_png.as_ref()),
            "dist/icon.svg" => resolve_leaf(top.dist.as_ref()?.files.as_ref()?.icon_svg.as_ref()),
            _ => None,
        }
    };

    let read_entry = |file: &mut fs::File, offset: u64, size: u64| -> Option<Vec<u8>> {
        if size == 0 || size > MAX_ASAR_ICON_BYTES {
            return None;
        }
        let abs_start = data_base.checked_add(offset)?;
        let abs_end = abs_start.checked_add(size)?;
        if abs_end > file_len {
            return None;
        }
        file.seek(SeekFrom::Start(abs_start)).ok()?;
        let mut buf = vec![0u8; size as usize];
        file.read_exact(&mut buf).ok()?;
        Some(buf)
    };

    // Fast path: check top-level `icon.png` / `icon.svg` first (covers Antigravity 2 and 95% of Electron apps without even reading `package.json`)
    for fast in ["icon.png", "icon.svg"] {
        if let Some((off, sz)) = lookup_static(fast) {
            if let Some(bytes) = read_entry(&mut file, off, sz) {
                if has_valid_image_magic(&bytes) {
                    return Some(bytes);
                }
            }
        }
    }

    // Check if `package.json` specifies a custom `"icon"` path
    let mut custom_candidate: Option<String> = None;
    if let Some((pkg_off, pkg_sz)) = resolve_leaf(top.package_json.as_ref()) {
        if pkg_sz <= 64 * 1024 {
            if let Some(pkg_bytes) = read_entry(&mut file, pkg_off, pkg_sz) {
                if let Ok(pkg_val) = serde_json::from_slice::<AsarPackageJson>(&pkg_bytes) {
                    if let Some(ic) = pkg_val.icon {
                        custom_candidate = Some(ic.trim_start_matches("./").to_string());
                    }
                }
            }
        }
    }

    let lookup_dynamic_custom = |rel_path: &str| -> Option<(u64, u64)> {
        let root: serde_json::Value = serde_json::from_slice(&json_buf).ok()?;
        let mut cur = &root;
        for part in rel_path.split('/').filter(|p| !p.is_empty()) {
            if part == ".." || part == "." {
                return None;
            }
            cur = cur.get("files")?.get(part)?;
        }
        if cur.get("unpacked").and_then(serde_json::Value::as_bool) == Some(true) {
            return None;
        }
        let size = cur.get("size")?.as_u64()?;
        let offset = cur
            .get("offset")
            .and_then(|v| v.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| v.as_u64()))?;
        Some((offset, size))
    };

    if let Some(ref custom) = custom_candidate {
        if let Some((off, sz)) = lookup_static(custom).or_else(|| lookup_dynamic_custom(custom)) {
            if let Some(bytes) = read_entry(&mut file, off, sz) {
                if has_valid_image_magic(&bytes) {
                    return Some(bytes);
                }
            }
        }
    }

    let subdir_candidates = [
        "assets/icon.png",
        "assets/icon.svg",
        "resources/icon.png",
        "resources/icon.svg",
        "build/icon.png",
        "dist/icon.png",
    ];

    for candidate in subdir_candidates {
        if let Some((off, sz)) = lookup_static(candidate) {
            if let Some(bytes) = read_entry(&mut file, off, sz) {
                if has_valid_image_magic(&bytes) {
                    return Some(bytes);
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icon::pixbuf::decode_png_bytes_to_pixbuf;
    use crate::icon::tests_common::{build_fake_asar, write_test_png, TestTempDir};

    #[test]
    fn test_extract_icon_from_asar_valid_png() {
        let temp_dir = TestTempDir::new("asar_valid");
        let png_path = temp_dir.join("sample.png");
        write_test_png(&png_path, 32, 32);
        let png_bytes = std::fs::read(&png_path).unwrap();

        let header = format!(
            r#"{{"files":{{"icon.png":{{"size":{},"offset":"0"}}}}}}"#,
            png_bytes.len()
        );
        let asar_bytes = build_fake_asar(&header, &png_bytes);
        let asar_path = temp_dir.join("app.asar");
        std::fs::write(&asar_path, &asar_bytes).unwrap();

        let extracted = extract_icon_bytes_from_asar(&asar_path);
        assert_eq!(extracted.as_deref(), Some(png_bytes.as_slice()));
    }

    #[test]
    fn test_extract_icon_from_asar_rejects_oversize_and_out_of_bounds() {
        let temp_dir = TestTempDir::new("asar_bounds");

        // 1. Offset + size exceeds file length
        let bad_offset_header = r#"{"files":{"icon.png":{"size":100,"offset":"999999"}}}"#;
        let asar_oob = build_fake_asar(bad_offset_header, b"\x89PNG\r\n\x1a\n1234");
        let oob_path = temp_dir.join("oob.asar");
        std::fs::write(&oob_path, &asar_oob).unwrap();
        assert!(extract_icon_bytes_from_asar(&oob_path).is_none());

        // 2. Non-image payload (e.g. arbitrary text/script pretending to be icon.png)
        let bad_magic_header = r#"{"files":{"icon.png":{"size":16,"offset":"0"}}}"#;
        let asar_bad_magic = build_fake_asar(bad_magic_header, b"#!/bin/sh\necho x");
        let magic_path = temp_dir.join("magic.asar");
        std::fs::write(&magic_path, &asar_bad_magic).unwrap();
        assert!(extract_icon_bytes_from_asar(&magic_path).is_none());

        // 3. Oversize header claim (> 16MB)
        let mut huge_header = Vec::new();
        huge_header.extend_from_slice(&4u32.to_le_bytes());
        huge_header.extend_from_slice(&(32 * 1024 * 1024u32).to_le_bytes());
        huge_header.extend_from_slice(&(32 * 1024 * 1024u32).to_le_bytes());
        huge_header.extend_from_slice(&(32 * 1024 * 1024u32).to_le_bytes());
        let huge_path = temp_dir.join("huge.asar");
        std::fs::write(&huge_path, &huge_header).unwrap();
        assert!(extract_icon_bytes_from_asar(&huge_path).is_none());
    }

    #[test]
    fn test_asar_selective_parser_ignores_huge_node_modules() {
        let temp_dir = TestTempDir::new("asar_fast");
        let png_path = temp_dir.join("fast.png");
        write_test_png(&png_path, 16, 16);
        let png_bytes = std::fs::read(&png_path).unwrap();

        let mut nm_entries = String::new();
        for i in 0..2000 {
            if i > 0 {
                nm_entries.push(',');
            }
            nm_entries.push_str(&format!(
                r#""pkg_{i}":{{"files":{{"index.js":{{"size":123,"offset":"0"}}}}}}"#
            ));
        }
        let header = format!(
            r#"{{"files":{{"node_modules":{{"files":{{{nm_entries}}}}},"icon.png":{{"size":{},"offset":"0"}}}}}}"#,
            png_bytes.len()
        );
        let asar_bytes = build_fake_asar(&header, &png_bytes);
        let asar_path = temp_dir.join("app.asar");
        std::fs::write(&asar_path, &asar_bytes).unwrap();

        let t0 = std::time::Instant::now();
        let extracted = extract_icon_bytes_from_asar_selective(&asar_path);
        let elapsed = t0.elapsed();

        assert_eq!(extracted.as_deref(), Some(png_bytes.as_slice()));
        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "Selective ASAR parser took too long: {elapsed:?}"
        );
    }

    #[test]
    fn test_extract_icon_from_realistic_fixture_asar() {
        // Committed fixture `src/icon/testdata/sample_app.asar` is a synthetic,
        // personal-information-free Electron-style archive: pickle header, nested
        // directories, node_modules noise, and PNG payloads. See its generator note
        // in `src/icon/testdata/README.md`.
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/icon/testdata/sample_app.asar");
        let bytes = extract_icon_bytes_from_asar_selective(&fixture)
            .expect("Fixture sample_app.asar must yield top-level icon.png bytes");
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        let pb = decode_png_bytes_to_pixbuf(&bytes, 24).expect("Must decode fixture PNG via cairo fast path");
        assert_eq!(pb.width(), 24);
        assert_eq!(pb.height(), 24);
    }
}
