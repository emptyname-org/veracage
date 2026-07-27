//! The desktop's text lines, rasterised into a memory buffer so they can live on
//! the BACKDROP next to the Veracage icon, below the app windows. egui paints
//! above everything, so text drawn there could not sit behind a window.
//!
//! epaint (egui's own text stack, already a dependency) does the font loading,
//! shaping and glyph rasterising. All this module adds is the blit: copy each
//! glyph's coverage out of epaint's atlas into an RGBA buffer, using the same
//! placement epaint's own layout uses (`glyph.pos + uv_rect.offset`, see
//! epaint text_layout.rs).

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::Transform;

/// Gap between the two lines, and padding around the block, in points.
const LINE_GAP: f32 = 10.0;
const PADDING: i32 = 2;

/// The rasterised desktop text, rebuilt only when the lines, font or size change.
#[derive(Default)]
pub struct HintText {
    buffer: Option<MemoryRenderBuffer>,
    size: (i32, i32),
    /// What the current buffer was built from.
    built: Option<(String, String, String, u32, bool)>,
}

impl HintText {
    /// The buffer for these two lines and its size in points, rasterising it if
    /// anything changed. None when there is nothing to draw or a glyph atlas
    /// could not be built.
    pub fn buffer(
        &mut self,
        line1: &str,
        line2: &str,
        font_path: &str,
        base: f32,
        dark: bool,
    ) -> Option<(&MemoryRenderBuffer, (i32, i32))> {
        if line1.is_empty() && line2.is_empty() {
            return None;
        }
        let key = (
            line1.to_string(),
            line2.to_string(),
            font_path.to_string(),
            base.to_bits(),
            dark,
        );
        if self.built.as_ref() != Some(&key) {
            let (w, h, rgba) = rasterise(line1, line2, font_path, base, dark)?;
            self.buffer = Some(MemoryRenderBuffer::from_slice(
                &rgba,
                Fourcc::Abgr8888,
                (w, h),
                1,
                Transform::Normal,
                None,
            ));
            self.size = (w, h);
            self.built = Some(key);
        }
        self.buffer.as_ref().map(|b| (b, self.size))
    }
}

/// The two lines as premultiplied RGBA, centred, with their pixel size.
fn rasterise(
    line1: &str,
    line2: &str,
    font_path: &str,
    base: f32,
    dark: bool,
) -> Option<(i32, i32, Vec<u8>)> {
    use egui::epaint::text::{FontDefinitions, Fonts};
    use egui::{Color32, FontData, FontFamily, FontId};

    const MAX_TEXTURE_SIDE: usize = 4096;
    let mut defs = FontDefinitions::default();
    if let Some(bytes) = read_font(font_path) {
        defs.font_data
            .insert("veracage-ui".to_owned(), FontData::from_owned(bytes));
        defs.families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "veracage-ui".to_owned());
    }
    let fonts = Fonts::new(1.0, MAX_TEXTURE_SIDE, defs);
    fonts.begin_pass(1.0, MAX_TEXTURE_SIDE);

    // The same weights the egui version used: a large first line, a smaller and
    // fainter second one.
    let (strong, weak) = if dark {
        (Color32::from_gray(190), Color32::from_gray(130))
    } else {
        (Color32::from_gray(95), Color32::from_gray(130))
    };
    let big = fonts.layout_no_wrap(line1.to_owned(), FontId::proportional(base * 1.7), strong);
    let small = fonts.layout_no_wrap(line2.to_owned(), FontId::proportional(base), weak);

    let width = big.size().x.max(small.size().x).ceil() as i32 + 2 * PADDING;
    let gap = if line2.is_empty() { 0.0 } else { LINE_GAP };
    let height = (big.size().y + gap + small.size().y).ceil() as i32 + 2 * PADDING;
    if width <= 0 || height <= 0 || width > MAX_TEXTURE_SIDE as i32 {
        return None;
    }
    let mut rgba = vec![0u8; (width * height * 4) as usize];

    // The atlas must be read AFTER laying out, so it holds these glyphs.
    let atlas = fonts.image();
    let (aw, ah) = (atlas.size[0], atlas.size[1]);
    let mut blit = |galley: &egui::Galley, oy: f32, color: Color32| {
        let ox = ((width - 2 * PADDING) as f32 - galley.size().x) * 0.5 + PADDING as f32;
        for row in &galley.rows {
            for glyph in &row.glyphs {
                if glyph.uv_rect.is_nothing() {
                    continue;
                }
                let left_top = glyph.pos + glyph.uv_rect.offset;
                let (gx, gy) = ((left_top.x + ox).round() as i32, (left_top.y + oy).round() as i32);
                let (u0, v0) = (glyph.uv_rect.min[0] as usize, glyph.uv_rect.min[1] as usize);
                let (gw, gh) = (
                    glyph.uv_rect.max[0] as usize - u0,
                    glyph.uv_rect.max[1] as usize - v0,
                );
                for dy in 0..gh {
                    for dx in 0..gw {
                        let (sx, sy) = (u0 + dx, v0 + dy);
                        if sx >= aw || sy >= ah {
                            continue;
                        }
                        let coverage = atlas.pixels[sy * aw + sx];
                        if coverage <= 0.0 {
                            continue;
                        }
                        let (px, py) = (gx + dx as i32, gy + dy as i32);
                        if px < 0 || py < 0 || px >= width || py >= height {
                            continue;
                        }
                        // Premultiplied, and blended over whatever is already
                        // there so overlapping glyph boxes do not punch holes.
                        let a = coverage * (color.a() as f32 / 255.0);
                        let i = ((py * width + px) * 4) as usize;
                        for (c, channel) in [color.r(), color.g(), color.b()].iter().enumerate() {
                            let add = *channel as f32 * a;
                            rgba[i + c] = (rgba[i + c] as f32 + add).min(255.0) as u8;
                        }
                        rgba[i + 3] = (rgba[i + 3] as f32 + a * 255.0).min(255.0) as u8;
                    }
                }
            }
        }
    };
    blit(&big, PADDING as f32, strong);
    blit(&small, PADDING as f32 + big.size().y + gap, weak);
    Some((width, height, rgba))
}

fn read_font(path: &str) -> Option<Vec<u8>> {
    if path.is_empty() {
        return None;
    }
    let md = std::fs::metadata(path).ok()?;
    (md.is_file() && md.len() <= 20_000_000).then(|| std::fs::read(path).ok())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasterises_visible_glyphs_with_premultiplied_alpha() {
        // The blit is the part that is ours (epaint does layout and glyphs), so
        // check it actually produces ink: a sized buffer with non-zero coverage.
        let (w, h, rgba) = rasterise("No volume mounted", "File > Mount volume", "", 16.0, false)
            .expect("the embedded epaint font must rasterise");
        assert!(w > 100 && h > 20, "unexpected block size {w}x{h}");
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        let inked = rgba.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert!(inked > 200, "only {inked} inked pixels: the glyphs did not land");
        // Premultiplied: no channel may exceed its own alpha.
        assert!(rgba.chunks_exact(4).all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]));
    }

    #[test]
    fn second_line_is_optional_and_nothing_means_no_buffer() {
        let (_, tall, _) = rasterise("1 volume mounted (work)", "hint", "", 16.0, false).unwrap();
        let (_, short, _) = rasterise("1 volume mounted (work)", "", "", 16.0, false).unwrap();
        assert!(short < tall, "an empty second line should not reserve its height");
        assert!(HintText::default().buffer("", "", "", 16.0, false).is_none());
    }
}
