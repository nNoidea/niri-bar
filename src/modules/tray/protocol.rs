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

pub fn parse_icon_pixmap(val: &zvariant::Value<'_>) -> Option<Vec<TrayIconPixmap>> {
    const MAX_DIM: i32 = 512;
    const MAX_PIXELS: u64 = 512 * 512;
    const MAX_PIXMAPS: usize = 8;
    const MAX_STORED_DIM: i32 = 64;

    let arr = match val {
        zvariant::Value::Array(a) => a,
        zvariant::Value::Value(inner) => return parse_icon_pixmap(inner),
        _ => return None,
    };

    let mut best: Option<(i32, i32, &zvariant::Array<'_>)> = None;
    for elem in arr.iter().take(MAX_PIXMAPS + 1) {
        let structure = match elem {
            zvariant::Value::Structure(s) => s,
            _ => continue,
        };
        let fields = structure.fields();
        if fields.len() < 3 {
            continue;
        }
        let w = match &fields[0] {
            zvariant::Value::I32(v) => *v,
            zvariant::Value::U32(v) => (*v).min(i32::MAX as u32) as i32,
            zvariant::Value::I16(v) => *v as i32,
            zvariant::Value::U16(v) => *v as i32,
            _ => continue,
        };
        let h = match &fields[1] {
            zvariant::Value::I32(v) => *v,
            zvariant::Value::U32(v) => (*v).min(i32::MAX as u32) as i32,
            zvariant::Value::I16(v) => *v as i32,
            zvariant::Value::U16(v) => *v as i32,
            _ => continue,
        };
        // Reject absurd dimensions before any allocation (DoS guard).
        if w <= 0 || h <= 0 || w > MAX_DIM || h > MAX_DIM {
            continue;
        }
        let expected = match (w as u64).checked_mul(h as u64).and_then(|n| n.checked_mul(4)) {
            Some(n) if n <= MAX_PIXELS * 4 && n <= (isize::MAX as u64) => n as usize,
            _ => continue,
        };
        let data_arr = match &fields[2] {
            zvariant::Value::Array(a) if a.len() == expected => a,
            _ => continue,
        };
        let area = (w as u64) * (h as u64);
        if best
            .as_ref()
            .is_none_or(|(bw, bh, _)| area >= (*bw as u64) * (*bh as u64))
        {
            best = Some((w, h, data_arr));
        }
    }

    let (w, h, data_arr) = best?;
    let (dst_w, dst_h) = scaled_pixmap_dims(w, h, MAX_STORED_DIM);

    let dst_len = (dst_w as usize) * (dst_h as usize) * 4;
    let mut pixels = Vec::with_capacity(dst_len);

    if dst_w == w && dst_h == h {
        for b in data_arr.iter() {
            if let zvariant::Value::U8(byte) = b {
                pixels.push(*byte);
            } else {
                return None;
            }
        }
    } else {
        for dy in 0..dst_h {
            let sy = ((dy as i64 * h as i64) / dst_h as i64) as usize;
            for dx in 0..dst_w {
                let sx = ((dx as i64 * w as i64) / dst_w as i64) as usize;
                let base = (sy * (w as usize) + sx) * 4;
                for c in 0..4 {
                    if let Ok(Some(byte)) = data_arr.get::<u8>(base + c) {
                        pixels.push(byte);
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    if pixels.len() != dst_len || pixels.is_empty() {
        None
    } else {
        Some(vec![TrayIconPixmap {
            width: dst_w,
            height: dst_h,
            pixels,
        }])
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
/// reply bodies (signature `v` wrapping `a(iiay)`).
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

    // 1. Variant signature `g`: 1-byte length + ASCII signature + NUL
    let sig_len = *data.first()? as usize;
    let sig_end = 1usize.checked_add(sig_len)?;
    if data.len() <= sig_end || data.get(1..sig_end)? != b"a(iiay)" || data[sig_end] != 0 {
        return None;
    }
    let mut pos = sig_end + 1;

    // 2. Array byte length (4-byte aligned)
    pos = (pos + 3) & !3;
    let arr_byte_len = read_u32(data.get(pos..pos + 4)?) as usize;
    pos += 4;

    // 3. Struct elements `(iiay)` are 8-byte aligned in D-Bus 1.0
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

pub fn parse_tooltip(props: &HashMap<String, zvariant::OwnedValue>) -> Option<TrayTooltip> {
    const MAX_TOOLTIP: usize = 512;
    let trunc = |s: String| {
        if s.len() <= MAX_TOOLTIP {
            s
        } else {
            s.chars().take(MAX_TOOLTIP).collect()
        }
    };
    let val = props.get("ToolTip").or_else(|| props.get("Tooltip"))?;
    match val.deref() {
        zvariant::Value::Structure(s) => {
            let fields = s.fields();
            let title = fields
                .get(2)
                .and_then(|v| match v {
                    zvariant::Value::Str(st) => Some(trunc(st.as_str().to_string())),
                    _ => None,
                })
                .unwrap_or_default();
            let description = fields
                .get(3)
                .and_then(|v| match v {
                    zvariant::Value::Str(st) => Some(trunc(st.as_str().to_string())),
                    _ => None,
                })
                .unwrap_or_default();
            Some(TrayTooltip { title, description })
        }
        zvariant::Value::Str(s) => {
            let text = trunc(s.as_str().to_string());
            Some(TrayTooltip {
                title: text.clone(),
                description: text,
            })
        }
        _ => None,
    }
}

#[derive(serde::Deserialize, zvariant::Type, Debug, Clone)]
pub struct RawMenuItem {
    pub id: i32,
    pub props: HashMap<String, zvariant::OwnedValue>,
    pub children: Vec<zvariant::OwnedValue>,
}

impl<'a> TryFrom<&'a zvariant::Value<'a>> for RawMenuItem {
    type Error = zvariant::Error;

    fn try_from(val: &'a zvariant::Value<'a>) -> Result<Self, Self::Error> {
        match val {
            zvariant::Value::Structure(s) => {
                let fields = s.fields();
                if fields.len() < 3 {
                    return Err(zvariant::Error::IncorrectType);
                }
                let id = match &fields[0] {
                    zvariant::Value::I32(i) => *i,
                    zvariant::Value::U32(u) => *u as i32,
                    _ => return Err(zvariant::Error::IncorrectType),
                };
                let mut props = HashMap::new();
                if let zvariant::Value::Dict(d) = &fields[1] {
                    for (k, v) in d.iter() {
                        if let zvariant::Value::Str(ks) = k {
                            if let Ok(ov) = zvariant::OwnedValue::try_from(v.clone()) {
                                props.insert(ks.as_str().to_string(), ov);
                            }
                        }
                    }
                }
                let mut children = Vec::new();
                if let zvariant::Value::Array(arr) = &fields[2] {
                    for elem in arr.iter() {
                        if let Ok(ov) = zvariant::OwnedValue::try_from(elem.clone()) {
                            children.push(ov);
                        }
                    }
                }
                Ok(RawMenuItem { id, props, children })
            }
            zvariant::Value::Value(v) => RawMenuItem::try_from(&**v),
            _ => Err(zvariant::Error::IncorrectType),
        }
    }
}

impl TryFrom<&zvariant::OwnedValue> for RawMenuItem {
    type Error = zvariant::Error;

    fn try_from(val: &zvariant::OwnedValue) -> Result<Self, Self::Error> {
        RawMenuItem::try_from(val.deref())
    }
}

fn extract_val_str<'a>(val: &'a zvariant::Value<'a>) -> Option<&'a str> {
    match val {
        zvariant::Value::Str(s) => Some(s.as_str()),
        zvariant::Value::Value(inner) => extract_val_str(inner),
        _ => None,
    }
}

fn extract_val_bool(val: &zvariant::Value<'_>) -> Option<bool> {
    match val {
        zvariant::Value::Bool(b) => Some(*b),
        zvariant::Value::I32(i) => Some(*i != 0),
        zvariant::Value::U32(u) => Some(*u != 0),
        zvariant::Value::Value(inner) => extract_val_bool(inner),
        _ => None,
    }
}

fn extract_val_i32(val: &zvariant::Value<'_>) -> Option<i32> {
    match val {
        zvariant::Value::I32(i) => Some(*i),
        zvariant::Value::U32(u) => Some(*u as i32),
        zvariant::Value::Bool(b) => Some(if *b { 1 } else { 0 }),
        zvariant::Value::Value(inner) => extract_val_i32(inner),
        _ => None,
    }
}

pub fn parse_raw_menu_item(raw: &RawMenuItem) -> MenuItem {
    parse_raw_menu_item_depth(raw, 0)
}

const MAX_MENU_DEPTH: u8 = 8;
const MAX_MENU_CHILDREN: usize = 500;
const MAX_LABEL_LEN: usize = 256;

fn parse_raw_menu_item_depth(raw: &RawMenuItem, depth: u8) -> MenuItem {
    let raw_label = raw
        .props
        .get("label")
        .and_then(|v| extract_val_str(v.deref()).map(truncate_label));
    let item_type = raw
        .props
        .get("type")
        .and_then(|v| extract_val_str(v.deref()))
        .unwrap_or("standard");
    let is_separator = item_type == "separator";

    let enabled = raw
        .props
        .get("enabled")
        .and_then(|v| extract_val_bool(v.deref()))
        .unwrap_or(true);
    let visible = raw
        .props
        .get("visible")
        .and_then(|v| extract_val_bool(v.deref()))
        .unwrap_or(true);

    let toggle_type = raw
        .props
        .get("toggle-type")
        .and_then(|v| match extract_val_str(v.deref()) {
            Some("checkmark") => Some(ToggleType::Checkmark),
            Some("radio") => Some(ToggleType::Radio),
            _ => None,
        });

    let toggle_state = raw
        .props
        .get("toggle-state")
        .and_then(|v| extract_val_i32(v.deref()))
        .unwrap_or(0);

    let mut submenu = Vec::new();
    if depth < MAX_MENU_DEPTH {
        for child_val in raw.children.iter().take(MAX_MENU_CHILDREN + 1) {
            if let Ok(child_item) = RawMenuItem::try_from(child_val) {
                submenu.push(parse_raw_menu_item_depth(&child_item, depth + 1));
            }
            if submenu.len() >= MAX_MENU_CHILDREN {
                break;
            }
        }
    }

    MenuItem {
        id: raw.id,
        label: raw_label,
        enabled,
        visible,
        is_separator,
        toggle_type,
        toggle_state,
        submenu,
    }
}

/// Strip DBusMenu mnemonic underscores (`_Preferences` → `Preferences`)
/// and surrounding whitespace. Used by the GTK menu builder so labels
/// render without mnemonics and duplicates collapse.
pub fn clean_menu_label(raw: &str) -> String {
    raw.replace('_', "").trim().to_string()
}

fn truncate_label(s: &str) -> String {
    if s.len() <= MAX_LABEL_LEN {
        s.to_string()
    } else {
        s.chars().take(MAX_LABEL_LEN).collect()
    }
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
            zvariant::OwnedValue::try_from(zvariant::Value::from("submenu")).unwrap(),
        );

        let mut props_child1 = HashMap::new();
        props_child1.insert(
            "label".to_string(),
            zvariant::OwnedValue::try_from(zvariant::Value::from("Open Toolbox")).unwrap(),
        );
        props_child1.insert(
            "enabled".to_string(),
            zvariant::OwnedValue::try_from(zvariant::Value::from(true)).unwrap(),
        );
        props_child1.insert(
            "visible".to_string(),
            zvariant::OwnedValue::try_from(zvariant::Value::from(true)).unwrap(),
        );

        let mut props_child2 = HashMap::new();
        props_child2.insert(
            "label".to_string(),
            zvariant::OwnedValue::try_from(zvariant::Value::from("Quit App")).unwrap(),
        );
        props_child2.insert(
            "type".to_string(),
            zvariant::OwnedValue::try_from(zvariant::Value::from("standard")).unwrap(),
        );

        let child1_struct = zvariant::Structure::from((1i32, props_child1, Vec::<zvariant::OwnedValue>::new()));

        let child2_struct = zvariant::Structure::from((2i32, props_child2, Vec::<zvariant::OwnedValue>::new()));

        let root = RawMenuItem {
            id: 0,
            props: props_root,
            children: vec![
                zvariant::OwnedValue::try_from(zvariant::Value::Structure(child1_struct)).unwrap(),
                zvariant::OwnedValue::try_from(zvariant::Value::Structure(child2_struct)).unwrap(),
            ],
        };

        let parsed = parse_raw_menu_item(&root);
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
            zvariant::OwnedValue::try_from(zvariant::Value::from("separator")).unwrap(),
        );

        let mut props_check = HashMap::new();
        props_check.insert(
            "label".to_string(),
            zvariant::OwnedValue::try_from(zvariant::Value::from("Dark Mode")).unwrap(),
        );
        props_check.insert(
            "toggle-type".to_string(),
            zvariant::OwnedValue::try_from(zvariant::Value::from("checkmark")).unwrap(),
        );
        props_check.insert(
            "toggle-state".to_string(),
            zvariant::OwnedValue::try_from(zvariant::Value::from(1i32)).unwrap(),
        );

        let sep_struct = zvariant::Structure::from((10i32, props_sep, Vec::<zvariant::OwnedValue>::new()));

        let check_struct = zvariant::Structure::from((11i32, props_check, Vec::<zvariant::OwnedValue>::new()));

        let mut props_root = HashMap::new();
        props_root.insert(
            "children-display".to_string(),
            zvariant::OwnedValue::try_from(zvariant::Value::from("submenu")).unwrap(),
        );

        let root = RawMenuItem {
            id: 0,
            props: props_root,
            children: vec![
                zvariant::OwnedValue::try_from(zvariant::Value::Structure(sep_struct)).unwrap(),
                zvariant::OwnedValue::try_from(zvariant::Value::Structure(check_struct)).unwrap(),
            ],
        };

        let parsed = parse_raw_menu_item(&root);
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

    fn deep_menu(depth: usize) -> RawMenuItem {
        // A chain `depth` levels deep: only RawMenuItem construction, no
        // parsing — parse_raw_menu_item must cap it without stack overflow.
        let mut child = RawMenuItem {
            id: depth as i32,
            props: HashMap::new(),
            children: Vec::new(),
        };
        for id in (0..depth).rev() {
            let wrapped = zvariant::Structure::from((
                id as i32,
                HashMap::<String, zvariant::OwnedValue>::new(),
                vec![
                    zvariant::OwnedValue::try_from(zvariant::Value::Structure(zvariant::Structure::from((
                        child.id,
                        child.props.clone(),
                        child.children.clone(),
                    ))))
                    .unwrap(),
                ],
            ));
            child =
                RawMenuItem::try_from(&zvariant::OwnedValue::try_from(zvariant::Value::Structure(wrapped)).unwrap())
                    .expect("test menu nesting must parse as RawMenuItem");
        }
        child
    }

    #[test]
    fn test_menu_depth_capped() {
        let parsed = parse_raw_menu_item(&deep_menu(100));
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
            zvariant::OwnedValue::try_from(zvariant::Value::from("x".repeat(2000).as_str())).unwrap(),
        );
        let item = RawMenuItem {
            id: 1,
            props,
            children: Vec::new(),
        };
        let parsed = parse_raw_menu_item(&item);
        assert!(parsed.label.as_ref().is_some_and(|l| l.len() <= 256));
    }

    #[test]
    fn test_pixmap_dimension_caps() {
        // Absurd dimensions with a short buffer must be rejected, not allocated.
        let huge = zvariant::Value::from(vec![zvariant::Value::Structure(zvariant::Structure::from((
            99999i32,
            99999i32,
            zvariant::Value::from(vec![0u8; 16]),
        )))]);
        assert!(parse_icon_pixmap(&huge).is_none());

        // Length mismatch (declares 2x2x4, delivers 4 bytes) rejected.
        let short = zvariant::Value::from(vec![zvariant::Value::Structure(zvariant::Structure::from((
            2i32,
            2i32,
            zvariant::Value::from(vec![0u8; 4]),
        )))]);
        assert!(parse_icon_pixmap(&short).is_none());
    }

    #[test]
    fn test_pixmap_downsamples_oversized_and_keeps_single_best() {
        let val = zvariant::Value::from(vec![
            (16i32, 16i32, vec![0xAAu8; 16 * 16 * 4]),
            (512i32, 512i32, vec![0xFFu8; 512 * 512 * 4]),
        ]);
        let parsed = parse_icon_pixmap(&val).expect("Should parse valid pixmaps");
        assert_eq!(parsed.len(), 1, "Should keep only the single best pixmap");
        assert_eq!(parsed[0].width, 64);
        assert_eq!(parsed[0].height, 64);
        assert_eq!(parsed[0].pixels.len(), 64 * 64 * 4);
        assert!(parsed[0].pixels.iter().all(|&b| b == 0xFF));

        // Verify zero-copy D-Bus wire body parser produces the exact same output
        let ctxt = zvariant::serialized::Context::new_dbus(zvariant::LE, 0);
        let bytes = zvariant::to_bytes(ctxt, &val).unwrap();
        let wire_parsed = parse_icon_pixmap_dbus_body(&bytes, true).expect("Should parse D-Bus wire body");
        assert_eq!(wire_parsed, parsed);
    }
}
