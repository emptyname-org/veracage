//! Soft drop shadows under the sandbox windows.
//!
//! App windows float on the compositor's backdrop, and without a shadow they read
//! as flat rectangles pasted onto it. One small texture holds the blur profile:
//! opaque along its centre cross, fading to nothing at the rim, with a radial
//! falloff in the corners so they round off. Every window stretches that texture
//! as a 9-slice ring around its geometry - the centre tile is never drawn,
//! because the window covers it.
//!
//! The ring is composited UNDER the windows (see the element order in winit.rs).
//! Drawing it on top would be simpler but wrong: with two overlapping windows,
//! the lower one's shadow would smear across the upper one.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::{Kind, NamespacedElement};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Physical, Point, Rectangle, Transform};

/// How far the shadow reaches beyond the window edge, in logical points.
const SPREAD: i32 = 18;
/// Opacity right at the window edge, falling to zero at `SPREAD`.
const EDGE_ALPHA: f32 = 0.38;
/// The shadow sits slightly below the window, like the host's own window shadows.
const OFFSET_Y: i32 = 4;

/// The blur profile as (side, RGBA bytes): black pixels whose alpha is full
/// strength along the centre cross and fades to nothing at the rim, radially in
/// the corners so they round off. Separate from the buffer so the falloff itself
/// is unit-tested without a renderer.
fn profile_rgba() -> (i32, Vec<u8>) {
    let side = 2 * SPREAD + 1;
    let radius = SPREAD as f32;
    let mut rgba = vec![0u8; (side * side * 4) as usize];
    for y in 0..side {
        for x in 0..side {
            // Distance from the centre cross, radial in the corners.
            let dx = (x - SPREAD).abs() as f32;
            let dy = (y - SPREAD).abs() as f32;
            let t = (dx.hypot(dy) / radius).min(1.0);
            // Quadratic falloff: dense at the edge, a long thin tail outward.
            let alpha = EDGE_ALPHA * (1.0 - t) * (1.0 - t);
            let i = ((y * side + x) * 4) as usize;
            // Black, premultiplied (rgb = 0 either way). RGBA order == Abgr8888.
            rgba[i + 3] = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    (side, rgba)
}

/// Build the blur-profile texture, `2*SPREAD+1` square. Done once at startup.
pub fn build_buffer() -> MemoryRenderBuffer {
    let (side, rgba) = profile_rgba();
    MemoryRenderBuffer::from_slice(&rgba, Fourcc::Abgr8888, (side, side), 1, Transform::Normal, None)
}

/// One slice of one window's shadow ring. Every slice samples the SAME profile
/// buffer, and a memory element takes its damage-tracking id from that buffer, so
/// each one must be namespaced or the tracker would treat all of them as a single
/// element jumping around the screen (and damage the wrong regions).
pub type ShadowElement = NamespacedElement<MemoryRenderBufferRenderElement<GlesRenderer>>;

/// The 9-slice ring for one window, as render elements. `rect` is the window's
/// geometry in logical points (output-relative), `scale` the output scale, and
/// `window_index` the window's position in the stack (it namespaces this ring's
/// ids, keeping them stable frame to frame). Slices that fail to import are
/// skipped: a missing shadow is cosmetic.
pub fn ring_elements(
    renderer: &mut GlesRenderer,
    buffer: &MemoryRenderBuffer,
    rect: Rectangle<i32, Logical>,
    scale: f64,
    window_index: usize,
) -> Vec<ShadowElement> {
    let r = SPREAD;
    let (x, y) = (rect.loc.x, rect.loc.y + OFFSET_Y);
    let (w, h) = (rect.size.w, rect.size.h);
    if w <= 0 || h <= 0 {
        return Vec::new();
    }
    // (src x, y, w, h) in the profile texture -> (dst x, y, w, h) in points.
    // The four corners take the texture's corner squares; the four edges take a
    // one-pixel strip through the middle and stretch along the window side.
    let slices = [
        (0, 0, r, r, x - r, y - r, r, r),         // top-left
        (r + 1, 0, r, r, x + w, y - r, r, r),     // top-right
        (0, r + 1, r, r, x - r, y + h, r, r),     // bottom-left
        (r + 1, r + 1, r, r, x + w, y + h, r, r), // bottom-right
        (r, 0, 1, r, x, y - r, w, r),             // top
        (r, r + 1, 1, r, x, y + h, w, r),         // bottom
        (0, r, r, 1, x - r, y, r, h),             // left
        (r + 1, r, r, 1, x + w, y, r, h),         // right
    ];
    slices
        .iter()
        .enumerate()
        .filter_map(|(slice, &(sx, sy, sw, sh, dx, dy, dw, dh))| {
            let src: Rectangle<f64, Logical> =
                Rectangle::new((sx as f64, sy as f64).into(), (sw as f64, sh as f64).into());
            let loc: Point<f64, Physical> = (dx as f64 * scale, dy as f64 * scale).into();
            let el = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                loc,
                buffer,
                None,
                Some(src),
                Some((dw, dh).into()),
                Kind::Unspecified,
            )
            .ok()?;
            // Unique per (window, slice) and stable across frames.
            Some(NamespacedElement::new(el, window_index * slices.len() + slice))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_fades_from_the_window_edge_to_nothing_at_the_rim() {
        let (side, rgba) = profile_rgba();
        assert_eq!(side, 2 * SPREAD + 1); // the 9-slice indexes the texture directly
        assert_eq!(rgba.len(), (side * side * 4) as usize);
        let alpha = |x: i32, y: i32| rgba[((y * side + x) * 4 + 3) as usize];

        // Full strength along the centre cross (that edge abuts the window), and
        // nothing at all at the rim, so the ring fades out instead of ending in a
        // visible line.
        assert_eq!(alpha(SPREAD, SPREAD), (EDGE_ALPHA * 255.0).round() as u8);
        assert_eq!(alpha(0, SPREAD), 0);
        assert_eq!(alpha(side - 1, SPREAD), 0);
        assert_eq!(alpha(0, 0), 0);

        // Each edge strip rises monotonically from the rim to the window edge:
        // this is the gradient the stretched slices show.
        for x in 1..=SPREAD {
            assert!(alpha(x, SPREAD) >= alpha(x - 1, SPREAD), "left strip at {x}");
        }
        for y in 1..=SPREAD {
            assert!(alpha(SPREAD, y) >= alpha(SPREAD, y - 1), "top strip at {y}");
        }

        // Corners are radial, so a corner pixel is dimmer than an edge pixel at
        // the same axis distance - that is what rounds the corners off.
        assert!(alpha(SPREAD / 2, SPREAD / 2) < alpha(SPREAD / 2, SPREAD));

        // Pure black: only the alpha channel carries the shadow.
        assert!(rgba.chunks_exact(4).all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0));
    }

}
