//! UI font + size. Nothing is bundled or hardcoded: by default Veracage uses the
//! font AND point size the host desktop is actually using (KDE `kreadconfig`,
//! GNOME `gsettings`, else fontconfig's `sans-serif`), resolved live. The user
//! can override the family (a short curated list, incl. a monospace face) and the
//! size in Settings; a chosen family that isn't installed falls back to egui's
//! built-in face.

use std::process::Command;

use eframe::egui;

/// Selectable font keys, in display order, with a human label. "system" follows
/// the host desktop; the rest map to a fontconfig family via `family_for`.
pub const CHOICES: &[(&str, &str)] = &[
    ("system", "Host system font"),
    ("noto", "Noto Sans"),
    ("liberation", "Liberation Sans"),
    ("dejavu", "DejaVu Sans"),
    ("dejavu-mono", "DejaVu Sans Mono"),
];

/// Selectable size values (stored string, label). "system" follows the host.
/// Numeric sizes are typographic POINTS, the same unit the host desktop's font
/// settings use, and the labels say so.
pub const SIZE_CHOICES: &[(&str, &str)] = &[
    ("system", "Host system size"),
    ("10", "10 pt"),
    ("11", "11 pt"),
    ("12", "12 pt"),
    ("13", "13 pt"),
    ("14", "14 pt"),
    ("16", "16 pt"),
    ("18", "18 pt"),
];

/// Fallback base POINT size when the host size can't be read.
pub const FALLBACK_PT: f32 = 10.0;
/// Fallback base size in LOGICAL PIXELS (used before the first resolve).
pub const FALLBACK_SIZE: f32 = 13.0;

pub fn is_valid_font(key: &str) -> bool {
    CHOICES.iter().any(|(k, _)| *k == key)
}

/// A size value is "system" or an integer point size in a sane range.
pub fn is_valid_size(s: &str) -> bool {
    s == "system" || s.parse::<u32>().map(|n| (6..=48).contains(&n)).unwrap_or(false)
}

/// Family + POINT size for apps inside the sandbox (kdeglobals stores points),
/// from the same settings that size the Veracage UI. "system" on either follows
/// the host desktop.
pub fn app_font(font_key: &str, size_key: &str) -> (String, f32) {
    let (host_family, host_pt) = host_ui_font();
    let family = if font_key == "system" {
        host_family.clone()
    } else {
        family_for(font_key).map(str::to_string)
    };
    let points = if size_key == "system" {
        host_pt.unwrap_or(FALLBACK_PT)
    } else {
        size_key.parse().unwrap_or(FALLBACK_PT)
    };
    (
        family
            .or(host_family)
            .unwrap_or_else(|| "Noto Sans".to_string()),
        points,
    )
}

/// The fontconfig family a key maps to, or None for "system"/unknown (host font).
fn family_for(key: &str) -> Option<&'static str> {
    match key {
        "noto" => Some("Noto Sans"),
        "liberation" => Some("Liberation Sans"),
        "dejavu" => Some("DejaVu Sans"),
        "dejavu-mono" => Some("DejaVu Sans Mono"),
        _ => None,
    }
}

/// Run a command and return trimmed stdout, or None on any failure.
pub(crate) fn run(bin: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// The host desktop's UI font family and point size, if detectable. KDE first
/// (`kreadconfig`), then GNOME (`gsettings`), then fontconfig's `sans-serif`
/// (family only). Used for the "system" font/size defaults.
pub fn host_ui_font() -> (Option<String>, Option<f32>) {
    // KDE: "Noto Sans,12,-1,5,50,0,0,0,0,0" (family, size, ...).
    for tool in ["kreadconfig6", "kreadconfig5"] {
        if let Some(v) = run(tool, &["--group", "General", "--key", "font"]) {
            if let Some(parsed) = parse_kde_font(&v) {
                return parsed;
            }
        }
    }
    // GNOME: "'Cantarell 11'" (family + trailing size) or a comma form.
    if let Some(v) = run("gsettings", &["get", "org.gnome.desktop.interface", "font-name"]) {
        if let Some(parsed) = parse_desc_font(v.trim_matches(['\'', '"'])) {
            return parsed;
        }
    }
    // Fallback: fontconfig's default sans family (no size).
    if let Some(v) = run("fc-match", &["--format=%{family}", "sans-serif"]) {
        let fam = v.split(',').next().unwrap_or("").trim().to_string();
        if !fam.is_empty() {
            return (Some(fam), None);
        }
    }
    (None, None)
}

/// Parse a KDE font string ("Family,size,...") into (family, size).
fn parse_kde_font(v: &str) -> Option<(Option<String>, Option<f32>)> {
    let mut it = v.split(',');
    let fam = it.next()?.trim().to_string();
    if fam.is_empty() {
        return None;
    }
    let size = it.next().and_then(|s| s.trim().parse::<f32>().ok());
    Some((Some(fam), size))
}

/// Parse a font description ("Family Size" or "Family, Size") into (family, size).
fn parse_desc_font(v: &str) -> Option<(Option<String>, Option<f32>)> {
    if v.contains(',') {
        return parse_kde_font(v);
    }
    // Trailing number is the size; the rest is the family.
    let v = v.trim();
    match v.rsplit_once(' ') {
        Some((fam, sz)) if sz.parse::<f32>().is_ok() => {
            Some((Some(fam.trim().to_string()), sz.parse::<f32>().ok()))
        }
        _ => (!v.is_empty()).then(|| (Some(v.to_string()), None)),
    }
}

/// The base body size for egui, in LOGICAL PIXELS. The configured/host size is in
/// typographic points ("system" -> host point size, else the number); this
/// converts points -> pixels via the font DPI, so a 12pt/96dpi desktop UI renders
/// at 16px (not 12) - matching Qt/KDE - then applies the resolved font's hinting
/// correction so the optical height matches Qt's too. Physical HiDPI scaling is
/// handled separately by egui's pixels_per_point.
pub fn base_size(font_key: &str, size_key: &str) -> f32 {
    let pt = if size_key == "system" {
        host_ui_font().1.unwrap_or(FALLBACK_PT)
    } else {
        size_key.parse::<f32>().unwrap_or(FALLBACK_PT)
    };
    let px = (pt * font_dpi() / 72.0).round().clamp(6.0, 72.0);
    let factor = resolve_font_file(font_key)
        .and_then(|p| read_font_file(&p))
        .map(|bytes| hinted_ascent_factor(&bytes, px))
        .unwrap_or(1.0);
    (px * factor).clamp(6.0, 72.0)
}

/// Qt/FreeType hint the font's ascender UP to the pixel grid; egui does not
/// hint at all, so at the same nominal size its text renders one pixel shorter
/// than every Qt app. Derive Qt's rounding from the font's own head/hhea
/// tables: scale so the unhinted ascender lands on the hinted integer. 1.0
/// when the tables can't be read. (Mirrors config.py _hinted_ascent_factor.)
fn hinted_ascent_factor(font: &[u8], px: f32) -> f32 {
    let u16at = |o: usize| -> Option<u16> {
        Some(u16::from_be_bytes([*font.get(o)?, *font.get(o + 1)?]))
    };
    let u32at = |o: usize| -> Option<u32> {
        Some(u32::from_be_bytes([
            *font.get(o)?,
            *font.get(o + 1)?,
            *font.get(o + 2)?,
            *font.get(o + 3)?,
        ]))
    };
    let parse = || -> Option<f32> {
        let base = if font.get(..4)? == b"ttcf" { u32at(12)? as usize } else { 0 };
        let num = u16at(base + 4)? as usize;
        let (mut head, mut hhea) = (None, None);
        for i in 0..num.min(64) {
            let e = base + 12 + 16 * i;
            match font.get(e..e + 4)? {
                b"head" => head = Some(u32at(e + 8)? as usize),
                b"hhea" => hhea = Some(u32at(e + 8)? as usize),
                _ => {}
            }
        }
        let upem = u16at(head? + 18)? as f32;
        let ascender = u16at(hhea? + 4)? as i16 as f32;
        if upem <= 0.0 || ascender <= 0.0 {
            return None;
        }
        let ascent_px = px * ascender / upem;
        Some((ascent_px.ceil() / ascent_px).clamp(1.0, 1.25))
    };
    parse().unwrap_or(1.0)
}

/// The font DPI Qt/KDE renders points at: KDE's forceFontDPI override if set,
/// else the CSS/logical 96 (HiDPI scaling is handled by pixels_per_point).
fn font_dpi() -> f32 {
    for tool in ["kreadconfig6", "kreadconfig5"] {
        if let Some(v) = run(tool, &["--group", "General", "--key", "forceFontDPI"]) {
            if let Ok(dpi) = v.parse::<f32>() {
                if dpi > 0.0 {
                    return dpi;
                }
            }
        }
    }
    96.0
}

/// Resolve the configured font key to a system font FILE path (via fontconfig),
/// or None to keep egui's built-in face ("system" with no detectable host font,
/// or a chosen family that isn't installed).
pub fn resolve_font_file(font_key: &str) -> Option<String> {
    let family = if font_key == "system" {
        host_ui_font().0?
    } else {
        family_for(font_key)?.to_string()
    };
    font_file(&family)
}

/// fontconfig-resolve a family to its regular face file path. None if the family
/// isn't installed (fontconfig substitutes an unrelated one, detected by
/// comparing the matched family) or the file is missing / too large.
fn font_file(family: &str) -> Option<String> {
    let out = run(
        "fc-match",
        &["--format=%{family}|%{file}", &format!("{family}:style=Regular")],
    )?;
    let (matched, path) = out.split_once('|')?;
    if !matched.to_lowercase().contains(&family.to_lowercase()) {
        return None;
    }
    let path = path.trim();
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() || md.len() > 20_000_000 {
        return None;
    }
    Some(path.to_string())
}

/// Read a validated font file (regular file, size-capped).
pub fn read_font_file(path: &str) -> Option<Vec<u8>> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() || md.len() > 20_000_000 {
        return None;
    }
    std::fs::read(path).ok()
}

/// Install the configured font family on `ctx` (replaces `FontDefinitions`;
/// rebuilds the atlas, so call only when the family changes). Sizes are applied
/// separately by `theme`.
pub fn install(ctx: &egui::Context, font_key: &str) {
    let mut fonts = egui::FontDefinitions::default();
    if let Some(bytes) = resolve_font_file(font_key).and_then(|p| read_font_file(&p)) {
        fonts.font_data.insert("veracage-ui".to_owned(), egui::FontData::from_owned(bytes));
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "veracage-ui".to_owned());
    }
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    #[test]
    fn app_font_uses_the_configured_family_and_points() {
        // Points, not pixels: kdeglobals is what Qt apps read, and it stores
        // typographic points, so no DPI conversion happens here.
        assert_eq!(super::app_font("noto", "12"), ("Noto Sans".to_string(), 12.0));
        assert_eq!(
            super::app_font("dejavu-mono", "18"),
            ("DejaVu Sans Mono".to_string(), 18.0)
        );
    }

    use super::*;

    #[test]
    fn parse_kde_font_family_and_size() {
        assert_eq!(parse_kde_font("Noto Sans,12,-1,5,50,0,0,0,0,0"),
                   Some((Some("Noto Sans".into()), Some(12.0))));
        assert_eq!(parse_kde_font("Cantarell").unwrap().0, Some("Cantarell".into()));
        assert!(parse_kde_font("").is_none());
    }

    #[test]
    fn parse_desc_font_trailing_size() {
        assert_eq!(parse_desc_font("Cantarell 11"),
                   Some((Some("Cantarell".into()), Some(11.0))));
        // Multi-word family with a trailing size.
        assert_eq!(parse_desc_font("DejaVu Sans 10").unwrap(),
                   (Some("DejaVu Sans".into()), Some(10.0)));
        // Comma form defers to the KDE parser.
        assert_eq!(parse_desc_font("Noto Sans,12").unwrap().1, Some(12.0));
    }

    #[test]
    fn is_valid_size_bounds() {
        assert!(is_valid_size("system"));
        assert!(is_valid_size("12"));
        assert!(!is_valid_size("5")); // below range
        assert!(!is_valid_size("49")); // above range
        assert!(!is_valid_size("abc"));
    }

    #[test]
    fn hinted_ascent_factor_on_a_real_font() {
        // The bundled icon is a PNG, so read a real system font's bytes.
        let path = font_file("DejaVu Sans").or_else(|| font_file("Noto Sans"));
        if let Some(bytes) = path.and_then(|p| read_font_file(&p)) {
            let f = hinted_ascent_factor(&bytes, 16.0);
            assert!((1.0..=1.25).contains(&f), "factor out of clamp: {f}");
        }
        // Garbage bytes must not panic and yield the neutral 1.0.
        assert_eq!(hinted_ascent_factor(&[0u8; 4], 16.0), 1.0);
        assert_eq!(hinted_ascent_factor(b"not a font at all", 16.0), 1.0);
    }
}
