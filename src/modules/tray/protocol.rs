use std::collections::HashMap;
use std::ops::Deref;

use super::types::{MenuItem, ToggleType, TrayIconPixmap, TrayTooltip};

pub fn parse_item_address(service: &str, sender_opt: Option<&str>) -> (String, String, String) {
    let s = service.trim();
    if s.starts_with('/') {
        let sender = sender_opt.unwrap_or("");
        let key = format!("{}{}", sender, s);
        (sender.to_string(), s.to_string(), key)
    } else if let Some((bus, path)) = s.split_once('/') {
        let full_path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{}", path)
        };
        let key = format!("{}{}", bus, full_path);
        (bus.to_string(), full_path, key)
    } else {
        let bus = s.to_string();
        let path = "/StatusNotifierItem".to_string();
        let key = format!("{}/StatusNotifierItem", bus);
        (bus, path, key)
    }
}

fn scaled_pixmap_dims(w: i32, h: i32, max_dim: i32) -> (i32, i32) {
    if w > max_dim || h > max_dim {
        if w >= h {
            let dh = ((h as i64 * max_dim as i64) / w as i64).max(1) as i32;
            (max_dim, dh)
        } else {
            let dw = ((w as i64 * max_dim as i64) / h as i64).max(1) as i32;
            (dw, max_dim)
        }
    } else {
        (w, h)
    }
}

/// Zero-copy D-Bus 1.0 wire parser for `Properties.Get(iface, "IconPixmap")`
/// reply bodies (`v` wrapping `a(iiay)` or `a(uuay)`, including nested `v`).
///
/// Deserializing `a(iiay)` via `zvariant::OwnedValue` unpacks every pixel byte
/// into a 24-byte `zvariant::Value::U8` enum (~25 MB of heap for a 512x512 icon).
/// Reading the raw D-Bus body slice directly avoids any intermediate allocation
/// and downsamples straight into a `<= 64x64` (`<= 16 KB`) buffer.
pub fn parse_icon_pixmap_dbus_body(data: &[u8], is_le: bool) -> Option<Vec<TrayIconPixmap>> {
    const MAX_DIM: i32 = 512;
    const MAX_PIXELS: usize = 512 * 512;
    const MAX_PIXMAPS: usize = 8;
    const MAX_STORED_DIM: i32 = 64;

    let read_u32 = |slice: &[u8]| -> u32 {
        let arr: [u8; 4] = [slice[0], slice[1], slice[2], slice[3]];
        if is_le {
            u32::from_le_bytes(arr)
        } else {
            u32::from_be_bytes(arr)
        }
    };
    let read_i32 = |slice: &[u8]| -> i32 { read_u32(slice) as i32 };

    // 1. Unwrap one or more D-Bus variant signature `g` layers (1-byte length + ASCII + NUL)
    let mut pos = 0usize;
    let mut matched_sig = false;
    for _ in 0..4 {
        let sig_len = *data.get(pos)? as usize;
        let sig_start = pos + 1;
        let sig_end = sig_start.checked_add(sig_len)?;
        if data.len() <= sig_end || data[sig_end] != 0 {
            return None;
        }
        let sig = &data[sig_start..sig_end];
        pos = sig_end + 1;
        if sig == b"v" {
            continue;
        }
        if sig == b"a(iiay)" || sig == b"a(uuay)" {
            matched_sig = true;
            break;
        }
        return None;
    }
    if !matched_sig {
        return None;
    }

    // 2. Array byte length (4-byte aligned)
    pos = (pos + 3) & !3;
    let arr_byte_len = read_u32(data.get(pos..pos + 4)?) as usize;
    pos += 4;

    // 3. Struct elements `(iiay)`/`(uuay)` are 8-byte aligned in D-Bus 1.0
    pos = (pos + 7) & !7;
    let arr_end = pos.checked_add(arr_byte_len)?;
    if arr_end > data.len() {
        return None;
    }

    let mut best: Option<(i32, i32, &[u8])> = None;
    let mut count = 0usize;
    while pos < arr_end && count < MAX_PIXMAPS {
        pos = (pos + 7) & !7;
        if pos >= arr_end {
            break;
        }
        let w = read_i32(data.get(pos..pos + 4)?);
        let h = read_i32(data.get(pos + 4..pos + 8)?);
        let pix_len = read_u32(data.get(pos + 8..pos + 12)?) as usize;
        pos += 12;

        let pix_end = pos.checked_add(pix_len)?;
        if pix_end > arr_end {
            return None;
        }
        let pix_slice = &data[pos..pix_end];
        pos = pix_end;
        count += 1;

        if w <= 0 || h <= 0 || w > MAX_DIM || h > MAX_DIM {
            continue;
        }
        let expected = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
        if expected > MAX_PIXELS * 4 || pix_len != expected {
            continue;
        }
        let area = (w as u64) * (h as u64);
        if best
            .as_ref()
            .is_none_or(|(bw, bh, _)| area >= (*bw as u64) * (*bh as u64))
        {
            best = Some((w, h, pix_slice));
        }
    }

    let (w, h, src) = best?;
    let (dst_w, dst_h) = scaled_pixmap_dims(w, h, MAX_STORED_DIM);
    let dst_len = (dst_w as usize) * (dst_h as usize) * 4;
    let mut pixels = Vec::with_capacity(dst_len);

    if dst_w == w && dst_h == h {
        pixels.extend_from_slice(src);
    } else {
        for dy in 0..dst_h {
            let sy = ((dy as i64 * h as i64) / dst_h as i64) as usize;
            for dx in 0..dst_w {
                let sx = ((dx as i64 * w as i64) / dst_w as i64) as usize;
                let base = (sy * (w as usize) + sx) * 4;
                pixels.extend_from_slice(src.get(base..base + 4)?);
            }
        }
    }

    Some(vec![TrayIconPixmap {
        width: dst_w,
        height: dst_h,
        pixels,
    }])
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

pub fn parse_tooltip(val: &zbus::Variant<'_>) -> Option<TrayTooltip> {
    const MAX_TOOLTIP: usize = 512;
    match val {
        zbus::Variant::Variant(inner) => parse_tooltip(inner),
        zbus::Variant::Structure(s) => {
            let fields = s.fields();
            let title = fields
                .get(2)
                .and_then(extract_val_str)
                .map(|st| truncate_str(st, MAX_TOOLTIP))
                .unwrap_or_default();
            let description = fields
                .get(3)
                .and_then(extract_val_str)
                .map(|st| truncate_str(st, MAX_TOOLTIP))
                .unwrap_or_default();
            Some(TrayTooltip { title, description })
        }
        zbus::Variant::Str(s) => {
            let text = truncate_str(s.as_str(), MAX_TOOLTIP);
            Some(TrayTooltip {
                title: text.clone(),
                description: text,
            })
        }
        _ => None,
    }
}

fn extract_val_str<'a>(val: &'a zbus::Variant<'a>) -> Option<&'a str> {
    match val {
        zbus::Variant::Str(s) => Some(s.as_str()),
        zbus::Variant::Variant(inner) => extract_val_str(inner),
        _ => None,
    }
}

fn extract_val_bool(val: &zbus::Variant<'_>) -> Option<bool> {
    match val {
        zbus::Variant::Bool(b) => Some(*b),
        zbus::Variant::I32(i) => Some(*i != 0),
        zbus::Variant::U32(u) => Some(*u != 0),
        zbus::Variant::Variant(inner) => extract_val_bool(inner),
        _ => None,
    }
}

fn extract_val_i32(val: &zbus::Variant<'_>) -> Option<i32> {
    match val {
        zbus::Variant::I32(i) => Some(*i),
        zbus::Variant::U32(u) => Some(*u as i32),
        zbus::Variant::Bool(b) => Some(if *b { 1 } else { 0 }),
        zbus::Variant::Variant(inner) => extract_val_i32(inner),
        _ => None,
    }
}

const MAX_MENU_DEPTH: u8 = 8;
const MAX_MENU_CHILDREN: usize = 500;
const MAX_LABEL_LEN: usize = 256;

fn build_menu_item<'a>(
    id: i32,
    props_iter: impl Iterator<Item = (&'a str, &'a zbus::Variant<'a>)>,
    submenu: Vec<MenuItem>,
) -> MenuItem {
    let mut label = None;
    let mut is_separator = false;
    let mut enabled = true;
    let mut visible = true;
    let mut toggle_type = None;
    let mut toggle_state = 0;

    for (k, v) in props_iter {
        match k {
            "label" => label = extract_val_str(v).map(|s| truncate_str(s, MAX_LABEL_LEN)),
            "type" => is_separator = extract_val_str(v) == Some("separator"),
            "enabled" => {
                if let Some(b) = extract_val_bool(v) {
                    enabled = b;
                }
            }
            "visible" => {
                if let Some(b) = extract_val_bool(v) {
                    visible = b;
                }
            }
            "toggle-type" => {
                toggle_type = match extract_val_str(v) {
                    Some("checkmark") => Some(ToggleType::Checkmark),
                    Some("radio") => Some(ToggleType::Radio),
                    _ => None,
                };
            }
            "toggle-state" => {
                if let Some(s) = extract_val_i32(v) {
                    toggle_state = s;
                }
            }
            _ => {}
        }
    }

    MenuItem {
        id,
        label,
        enabled,
        visible,
        is_separator,
        toggle_type,
        toggle_state,
        submenu,
    }
}

fn parse_menu_variant(val: &zbus::Variant<'_>, depth: u8) -> Option<MenuItem> {
    match val {
        zbus::Variant::Variant(inner) => parse_menu_variant(inner, depth),
        zbus::Variant::Structure(s) => {
            let fields = s.fields();
            let id = extract_val_i32(fields.first()?)?;
            let props_iter = fields
                .get(1)
                .into_iter()
                .flat_map(|v| match v {
                    zbus::Variant::Dict(d) => Some(d),
                    _ => None,
                })
                .flat_map(|d| {
                    d.iter().filter_map(|(k, v)| match k {
                        zbus::Variant::Str(ks) => Some((ks.as_str(), v)),
                        _ => None,
                    })
                });
            let mut submenu = Vec::new();
            if depth < MAX_MENU_DEPTH {
                if let Some(zbus::Variant::Array(arr)) = fields.get(2) {
                    for child in arr.iter().take(MAX_MENU_CHILDREN) {
                        if let Some(parsed) = parse_menu_variant(child, depth + 1) {
                            submenu.push(parsed);
                        }
                    }
                }
            }
            Some(build_menu_item(id, props_iter, submenu))
        }
        _ => None,
    }
}

/// Parses a decoded DBusMenu `GetLayout` root node `(id, props, children)` without
/// building an intermediate recursive wrapper struct.
pub fn parse_dbusmenu_tuple(
    id: i32,
    props: &HashMap<String, zbus::OwnedVariant>,
    children: &[zbus::OwnedVariant],
) -> MenuItem {
    let submenu = children
        .iter()
        .take(MAX_MENU_CHILDREN)
        .filter_map(|c| parse_menu_variant(c.deref(), 1))
        .collect();
    build_menu_item(id, props.iter().map(|(k, v)| (k.as_str(), v.deref())), submenu)
}

/// Strip DBusMenu mnemonic underscores (`_Preferences` → `Preferences`)
/// and surrounding whitespace. Used by the GTK menu builder so labels
/// render without mnemonics and duplicates collapse.
pub fn clean_menu_label(raw: &str) -> String {
    raw.replace('_', "").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_item_address() {
        // Path only (Steam)
        let (bus, path, key) = parse_item_address("/org/ayatana/NotificationItem/steam", Some(":1.6300"));
        assert_eq!(bus, ":1.6300");
        assert_eq!(path, "/org/ayatana/NotificationItem/steam");
        assert_eq!(key, ":1.6300/org/ayatana/NotificationItem/steam");

        // Combined destination & path (JetBrains Toolbox)
        let (bus, path, key) = parse_item_address(":1.5215/StatusNotifierItem", None);
        assert_eq!(bus, ":1.5215");
        assert_eq!(path, "/StatusNotifierItem");
        assert_eq!(key, ":1.5215/StatusNotifierItem");

        // Service name only (OBS)
        let (bus, path, key) = parse_item_address("org.kde.StatusNotifierItem-71505-1", None);
        assert_eq!(bus, "org.kde.StatusNotifierItem-71505-1");
        assert_eq!(path, "/StatusNotifierItem");
        assert_eq!(key, "org.kde.StatusNotifierItem-71505-1/StatusNotifierItem");
    }

    #[test]
    fn test_raw_menu_parsing() {
        let mut props_root = HashMap::new();
        props_root.insert(
            "children-display".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from("submenu")).unwrap(),
        );

        let mut props_child1 = HashMap::new();
        props_child1.insert(
            "label".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from("Open Toolbox")).unwrap(),
        );
        props_child1.insert(
            "enabled".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from(true)).unwrap(),
        );
        props_child1.insert(
            "visible".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from(true)).unwrap(),
        );

        let mut props_child2 = HashMap::new();
        props_child2.insert(
            "label".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from("Quit App")).unwrap(),
        );
        props_child2.insert(
            "type".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from("standard")).unwrap(),
        );

        let child1_struct = zbus::Structure::from((1i32, props_child1, Vec::<zbus::OwnedVariant>::new()));
        let child2_struct = zbus::Structure::from((2i32, props_child2, Vec::<zbus::OwnedVariant>::new()));

        let parsed = parse_dbusmenu_tuple(
            0,
            &props_root,
            &[
                zbus::OwnedVariant::try_from(zbus::Variant::Structure(child1_struct)).unwrap(),
                zbus::OwnedVariant::try_from(zbus::Variant::Structure(child2_struct)).unwrap(),
            ],
        );
        assert_eq!(parsed.id, 0);
        assert_eq!(parsed.submenu.len(), 2);
        assert_eq!(parsed.submenu[0].label.as_deref(), Some("Open Toolbox"));
        assert_eq!(parsed.submenu[1].label.as_deref(), Some("Quit App"));
    }

    #[test]
    fn test_raw_menu_separator_and_toggle() {
        let mut props_sep = HashMap::new();
        props_sep.insert(
            "type".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from("separator")).unwrap(),
        );

        let mut props_check = HashMap::new();
        props_check.insert(
            "label".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from("Dark Mode")).unwrap(),
        );
        props_check.insert(
            "toggle-type".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from("checkmark")).unwrap(),
        );
        props_check.insert(
            "toggle-state".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from(1i32)).unwrap(),
        );

        let sep_struct = zbus::Structure::from((10i32, props_sep, Vec::<zbus::OwnedVariant>::new()));
        let check_struct = zbus::Structure::from((11i32, props_check, Vec::<zbus::OwnedVariant>::new()));

        let parsed = parse_dbusmenu_tuple(
            0,
            &HashMap::new(),
            &[
                zbus::OwnedVariant::try_from(zbus::Variant::Structure(sep_struct)).unwrap(),
                zbus::OwnedVariant::try_from(zbus::Variant::Structure(check_struct)).unwrap(),
            ],
        );
        assert_eq!(parsed.submenu.len(), 2);
        assert!(parsed.submenu[0].is_separator);
        assert_eq!(parsed.submenu[1].toggle_type, Some(ToggleType::Checkmark));
        assert_eq!(parsed.submenu[1].toggle_state, 1);
    }

    #[test]
    fn test_clean_menu_label() {
        assert_eq!(clean_menu_label("_Preferences..."), "Preferences...");
        assert_eq!(clean_menu_label("__Double__Under__"), "DoubleUnder");
        assert_eq!(clean_menu_label("  _File_  "), "File");
        assert_eq!(clean_menu_label("_"), "");
    }

    fn deep_menu_children(depth: usize) -> Vec<zbus::OwnedVariant> {
        let mut node = zbus::OwnedVariant::try_from(zbus::Variant::Structure(zbus::Structure::from((
            depth as i32,
            HashMap::<String, zbus::OwnedVariant>::new(),
            Vec::<zbus::OwnedVariant>::new(),
        ))))
        .unwrap();
        for id in (1..depth).rev() {
            node = zbus::OwnedVariant::try_from(zbus::Variant::Structure(zbus::Structure::from((
                id as i32,
                HashMap::<String, zbus::OwnedVariant>::new(),
                vec![node],
            ))))
            .unwrap();
        }
        vec![node]
    }

    #[test]
    fn test_menu_depth_capped() {
        let parsed = parse_dbusmenu_tuple(0, &HashMap::new(), &deep_menu_children(100));
        // Walk the parsed tree: depth must stop at MAX_MENU_DEPTH.
        let mut depth = 0;
        let mut node = &parsed;
        while let Some(first) = node.submenu.first() {
            depth += 1;
            node = first;
            assert!(depth <= 8, "menu recursion exceeded cap");
        }
        assert!(depth > 0);
    }

    #[test]
    fn test_menu_label_truncated() {
        let mut props = HashMap::new();
        props.insert(
            "label".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from("x".repeat(2000).as_str())).unwrap(),
        );
        let parsed = parse_dbusmenu_tuple(1, &props, &[]);
        assert!(parsed.label.as_ref().is_some_and(|l| l.len() <= 256));
    }

    #[test]
    fn test_pixmap_dimension_caps() {
        let le_ctxt = zbus::wire::serialized::Context::new(zbus::wire::LE, 0);
        // Absurd dimensions with a short buffer must be rejected, not allocated.
        let huge = zbus::Variant::from(vec![(99999i32, 99999i32, vec![0u8; 16])]);
        let huge_bytes = zbus::wire::to_bytes(le_ctxt, &huge).unwrap();
        assert!(parse_icon_pixmap_dbus_body(&huge_bytes, true).is_none());

        // Length mismatch (declares 2x2x4, delivers 4 bytes) rejected.
        let short = zbus::Variant::from(vec![(2i32, 2i32, vec![0u8; 4])]);
        let short_bytes = zbus::wire::to_bytes(le_ctxt, &short).unwrap();
        assert!(parse_icon_pixmap_dbus_body(&short_bytes, true).is_none());
    }

    #[test]
    fn test_pixmap_downsamples_oversized_and_keeps_single_best() {
        let val = zbus::Variant::from(vec![
            (16i32, 16i32, vec![0xAAu8; 16 * 16 * 4]),
            (512i32, 512i32, vec![0xFFu8; 512 * 512 * 4]),
        ]);
        let ctxt = zbus::wire::serialized::Context::new(zbus::wire::LE, 0);
        let bytes = zbus::wire::to_bytes(ctxt, &val).unwrap();
        let wire_parsed = parse_icon_pixmap_dbus_body(&bytes, true).expect("Should parse D-Bus wire body");
        assert_eq!(wire_parsed.len(), 1, "Should keep only the single best pixmap");
        assert_eq!(wire_parsed[0].width, 64);
        assert_eq!(wire_parsed[0].height, 64);
        assert_eq!(wire_parsed[0].pixels.len(), 64 * 64 * 4);
        assert!(wire_parsed[0].pixels.iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn test_nested_variant_unwrapping() {
        let inner = zbus::Variant::from(vec![(4i32, 4i32, vec![0x77u8; 4 * 4 * 4])]);
        let wrapped = zbus::Variant::Variant(Box::new(inner));
        let ctxt = zbus::wire::serialized::Context::new(zbus::wire::LE, 0);
        let bytes = zbus::wire::to_bytes(ctxt, &wrapped).unwrap();
        let parsed = parse_icon_pixmap_dbus_body(&bytes, true).expect("Nested Variant::Variant must unwrap");
        assert_eq!(parsed[0].width, 4);
        assert_eq!(parsed[0].height, 4);
    }

    #[test]
    fn test_pixmap_dbus_body_handles_unsigned_nested_and_be() {
        let val_u = zbus::Variant::from(vec![(8u32, 8u32, vec![0x55u8; 8 * 8 * 4])]);
        let le_ctxt = zbus::wire::serialized::Context::new(zbus::wire::LE, 0);
        let bytes_u = zbus::wire::to_bytes(le_ctxt, &val_u).expect("serialize a(uuay)");
        let parsed_u = parse_icon_pixmap_dbus_body(&bytes_u, true).expect("a(uuay) must parse");
        assert_eq!(parsed_u[0].width, 8);
        assert_eq!(parsed_u[0].height, 8);

        let wrapped = zbus::Variant::Variant(Box::new(val_u));
        let bytes_nested = zbus::wire::to_bytes(le_ctxt, &wrapped).expect("serialize nested v");
        let parsed_nested = parse_icon_pixmap_dbus_body(&bytes_nested, true).expect("nested variant must parse");
        assert_eq!(parsed_nested[0].width, 8);

        let be_ctxt = zbus::wire::serialized::Context::new(zbus::wire::BE, 0);
        let val_be = zbus::Variant::from(vec![(4i32, 4i32, vec![0x33u8; 4 * 4 * 4])]);
        let bytes_be = zbus::wire::to_bytes(be_ctxt, &val_be).expect("serialize BE");
        let parsed_be = parse_icon_pixmap_dbus_body(&bytes_be, false).expect("BE body must parse");
        assert_eq!(parsed_be[0].width, 4);
        assert!(parsed_be[0].pixels.iter().all(|&b| b == 0x33));
    }

    #[test]
    fn test_parse_dbusmenu_node_directly() {
        let mut child_props = HashMap::new();
        child_props.insert(
            "label".to_string(),
            zbus::OwnedVariant::try_from(zbus::Variant::from("_Settings")).expect("variant"),
        );
        let child_struct = zbus::Structure::from((7i32, child_props, Vec::<zbus::OwnedVariant>::new()));
        let child_ov = zbus::OwnedVariant::try_from(zbus::Variant::Structure(child_struct)).expect("ov");
        let parsed = parse_dbusmenu_tuple(0, &HashMap::new(), &[child_ov]);
        assert_eq!(parsed.id, 0);
        assert_eq!(parsed.submenu.len(), 1);
        assert_eq!(parsed.submenu[0].id, 7);
        assert_eq!(parsed.submenu[0].label.as_deref(), Some("_Settings"));
    }
}
