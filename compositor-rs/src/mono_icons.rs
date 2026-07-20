//! Veracage's own menu-item glyphs, drawn in-process: a strict two-color set
//! (one ink on transparency) that looks identical on every host. No icon theme
//! is consulted: host themes differ per machine and their small icons are
//! rarely true two-color. App icons (colored, from the host) are separate.
//!
//! Rendering: each glyph is a painter's-algorithm list of shapes over the unit
//! square, rasterized 4x supersampled into an RGBA image, so edges are smooth
//! without any vector dependency.

/// Output icon side in pixels (drawn once, GPU-scaled to the menu's 16pt).
const SIZE: usize = 44;
/// Supersampling factor per axis (16 coverage samples per pixel).
const SS: usize = 4;

/// A paint step: inside `test`, ink is applied (or erased with `erase`).
struct Shape {
    test: Box<dyn Fn(f32, f32) -> bool>,
    erase: bool,
}

fn add(test: impl Fn(f32, f32) -> bool + 'static) -> Shape {
    Shape { test: Box::new(test), erase: false }
}

fn cut(test: impl Fn(f32, f32) -> bool + 'static) -> Shape {
    Shape { test: Box::new(test), erase: true }
}

fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> impl Fn(f32, f32) -> bool {
    move |x, y| x >= x0 && x <= x1 && y >= y0 && y <= y1
}

/// Axis-aligned rounded rectangle with corner radius `r`.
fn rrect(x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> impl Fn(f32, f32) -> bool {
    move |x, y| {
        if x < x0 || x > x1 || y < y0 || y > y1 {
            return false;
        }
        let cx = x.clamp(x0 + r, x1 - r);
        let cy = y.clamp(y0 + r, y1 - r);
        (x - cx).powi(2) + (y - cy).powi(2) <= r * r
    }
}

fn circle(cx: f32, cy: f32, r: f32) -> impl Fn(f32, f32) -> bool {
    move |x, y| (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

/// Triangle via three half-plane sign tests (any winding).
fn tri(a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> impl Fn(f32, f32) -> bool {
    let sign = |p: (f32, f32), q: (f32, f32), x: f32, y: f32| {
        (x - q.0) * (p.1 - q.1) - (p.0 - q.0) * (y - q.1)
    };
    move |x, y| {
        let d1 = sign(a, b, x, y);
        let d2 = sign(b, c, x, y);
        let d3 = sign(c, a, x, y);
        let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
        let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
        !(neg && pos)
    }
}

/// The glyph shapes for a menu-icon key. Unit coordinates, y down.
fn glyph(key: &str) -> Vec<Shape> {
    match key {
        // A drive: rounded body, punched indicator dot + slot.
        "mount" => vec![
            add(rrect(0.10, 0.28, 0.90, 0.72, 0.08)),
            cut(circle(0.74, 0.55, 0.055)),
            cut(rect(0.20, 0.42, 0.62, 0.50)),
        ],
        // An open folder: tab + body.
        "exchange" => vec![
            add(rrect(0.08, 0.20, 0.48, 0.40, 0.04)),
            add(rrect(0.08, 0.30, 0.92, 0.78, 0.05)),
        ],
        // Eject: triangle over a bar.
        "unmount" => vec![
            add(tri((0.50, 0.16), (0.13, 0.56), (0.87, 0.56))),
            add(rrect(0.13, 0.66, 0.87, 0.80, 0.03)),
        ],
        // Power symbol: ring with a notch, vertical bar through it.
        "quit" => vec![
            add(circle(0.50, 0.56, 0.34)),
            cut(circle(0.50, 0.56, 0.22)),
            cut(rect(0.38, 0.10, 0.62, 0.44)),
            add(rect(0.44, 0.10, 0.56, 0.52)),
        ],
        // Copy out: tray with an arrow leaving upward.
        "copy_out" => vec![
            add(rrect(0.12, 0.72, 0.88, 0.86, 0.03)),
            add(rect(0.44, 0.34, 0.56, 0.64)),
            add(tri((0.50, 0.10), (0.28, 0.38), (0.72, 0.38))),
        ],
        // Paste in: tray with an arrow arriving downward.
        "paste_in" => vec![
            add(rrect(0.12, 0.72, 0.88, 0.86, 0.03)),
            add(rect(0.44, 0.12, 0.56, 0.42)),
            add(tri((0.50, 0.66), (0.28, 0.38), (0.72, 0.38))),
        ],
        // Apps: a 2x2 grid of tiles.
        "configure_apps" => vec![
            add(rrect(0.12, 0.12, 0.44, 0.44, 0.06)),
            add(rrect(0.56, 0.12, 0.88, 0.44, 0.06)),
            add(rrect(0.12, 0.56, 0.44, 0.88, 0.06)),
            add(rrect(0.56, 0.56, 0.88, 0.88, 0.06)),
        ],
        // Settings: three slider rows with offset knobs.
        "settings" => vec![
            add(rect(0.12, 0.225, 0.88, 0.285, )),
            add(rect(0.12, 0.475, 0.88, 0.535)),
            add(rect(0.12, 0.725, 0.88, 0.785)),
            add(circle(0.64, 0.255, 0.085)),
            add(circle(0.32, 0.505, 0.085)),
            add(circle(0.72, 0.755, 0.085)),
        ],
        // Keyboard: outline with key dots and a spacebar.
        "shortcuts" => vec![
            add(rrect(0.06, 0.24, 0.94, 0.76, 0.05)),
            cut(rrect(0.12, 0.30, 0.88, 0.70, 0.03)),
            add(rect(0.18, 0.36, 0.26, 0.44)),
            add(rect(0.32, 0.36, 0.40, 0.44)),
            add(rect(0.46, 0.36, 0.54, 0.44)),
            add(rect(0.60, 0.36, 0.68, 0.44)),
            add(rect(0.74, 0.36, 0.82, 0.44)),
            add(rect(0.30, 0.54, 0.70, 0.62)),
        ],
        // Help: an open book (two pages).
        "help" => vec![
            add(rrect(0.10, 0.24, 0.90, 0.76, 0.05)),
            cut(rect(0.46, 0.24, 0.54, 0.76)),
        ],
        // About: an "i" in a ring.
        "about" => vec![
            add(circle(0.50, 0.50, 0.40)),
            cut(circle(0.50, 0.50, 0.33)),
            add(circle(0.50, 0.30, 0.055)),
            add(rect(0.452, 0.42, 0.548, 0.72)),
        ],
        _ => Vec::new(),
    }
}

/// Rasterize the glyph for `key` in `ink` (the theme's icon color). Every pixel
/// is `ink` with a coverage-derived alpha: a strict two-color icon.
pub fn icon(key: &str, ink: egui::Color32) -> Option<egui::ColorImage> {
    let shapes = glyph(key);
    if shapes.is_empty() {
        return None;
    }
    let mut pixels = Vec::with_capacity(SIZE * SIZE);
    let step = 1.0 / (SIZE * SS) as f32;
    for py in 0..SIZE {
        for px in 0..SIZE {
            let mut hits = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px * SS + sx) as f32 * step + step / 2.0;
                    let y = (py * SS + sy) as f32 * step + step / 2.0;
                    let mut on = false;
                    for s in &shapes {
                        if (s.test)(x, y) {
                            on = !s.erase;
                        }
                    }
                    hits += on as u32;
                }
            }
            let a = (hits * 255 / (SS * SS) as u32) as u8;
            pixels.push(egui::Color32::from_rgba_unmultiplied(
                ink.r(),
                ink.g(),
                ink.b(),
                a,
            ));
        }
    }
    Some(egui::ColorImage { size: [SIZE, SIZE], pixels })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_key_renders_full_size_with_coverage() {
        let img = icon("mount", egui::Color32::from_gray(35)).unwrap();
        assert_eq!(img.size, [SIZE, SIZE]);
        assert_eq!(img.pixels.len(), SIZE * SIZE);
        // A real shape rendered: some fully-opaque ink and some transparency.
        assert!(img.pixels.iter().any(|p| p.a() == 255));
        assert!(img.pixels.iter().any(|p| p.a() == 0));
    }

    #[test]
    fn unknown_key_is_none() {
        assert!(icon("does-not-exist", egui::Color32::WHITE).is_none());
    }

    #[test]
    fn every_menu_glyph_exists() {
        for key in crate::toolbar::MENU_ICON_KEYS {
            assert!(icon(key, egui::Color32::WHITE).is_some(), "missing glyph: {key}");
        }
    }
}
