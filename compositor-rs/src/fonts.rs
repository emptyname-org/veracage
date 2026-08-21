//! UI font for the compositor menu bar. Not bundled: the human side resolves the
//! configured font family to a file via fontconfig and passes the path in
//! `VERACAGE_FONT_FILE` (forwarded like `VERACAGE_THEME`). Empty/unset or an
//! unreadable path keeps egui's built-in face.

/// Load the font file at `path` as the proportional family. Empty path or an
/// unreadable / implausibly large file keeps egui's default. Called once.
pub fn install_from_file(ctx: &egui::Context, path: &str) {
    let mut fonts = egui::FontDefinitions::default();
    if !path.is_empty() {
        if let Some(bytes) = read_font(path) {
            fonts
                .font_data
                .insert("veracage-ui".to_owned(), egui::FontData::from_owned(bytes));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "veracage-ui".to_owned());
        }
    }
    ctx.set_fonts(fonts);
}

/// True if `bytes` starts with an sfnt/WOFF signature epaint's parser accepts:
/// TrueType (0x00010000 or "true"), OpenType ("OTTO") or a collection ("ttcf").
///
/// epaint PANICS on a face it cannot parse, and that panic runs inside the
/// compositor's redraw callback, so it takes down the window and every sandboxed
/// app of every open volume with it. The path is not attacker-supplied but it is
/// not curated either: with `ui_font = "system"` the human side publishes
/// whatever fontconfig matched for the host UI family, and fontconfig will
/// happily return a bitmap face (.pcf.gz, .otb) or a Type 1 file. Checking four
/// bytes is cheaper than a catch_unwind and keeps the built-in face instead.
pub(crate) fn is_scalable_font(bytes: &[u8]) -> bool {
    matches!(
        bytes.get(..4),
        Some(b"\x00\x01\x00\x00") | Some(b"true") | Some(b"OTTO") | Some(b"ttcf")
            | Some(b"wOFF") | Some(b"wOF2")
    )
}

fn read_font(path: &str) -> Option<Vec<u8>> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() || md.len() > 20_000_000 {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if !is_scalable_font(&bytes) {
        tracing::warn!("font {path}: not a scalable font file, keeping the default face");
        return None;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::is_scalable_font;

    #[test]
    fn only_sfnt_signatures_are_accepted() {
        // What fontconfig can hand us for a bitmap family, and what epaint panics on.
        assert!(!is_scalable_font(b"\x1f\x8b\x08\x00rest"));   // .pcf.gz
        assert!(!is_scalable_font(b"%!PS-AdobeFont-1.0"));       // Type 1
        assert!(!is_scalable_font(b""));
        assert!(!is_scalable_font(b"\x00\x01"));                 // truncated
        // The ones a face parser accepts.
        assert!(is_scalable_font(b"\x00\x01\x00\x00rest"));
        assert!(is_scalable_font(b"OTTOrest"));
        assert!(is_scalable_font(b"ttcfrest"));
        assert!(is_scalable_font(b"truerest"));
    }
}
