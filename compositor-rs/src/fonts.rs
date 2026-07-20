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

fn read_font(path: &str) -> Option<Vec<u8>> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() || md.len() > 20_000_000 {
        return None;
    }
    std::fs::read(path).ok()
}
