//! Drop shadows under the sandbox windows, drawn by scenefx's box-shadow shader.
//!
//! The shader below is scenefx's `render/fx_renderer/shaders/box_shadow.frag`
//! (MIT, see LICENSES/scenefx-MIT.txt), which implements Evan Wallace's fast
//! rounded-rectangle shadow. Two adaptations, both mechanical: `main` reads
//! smithay's pixel-shader interface (the `v_coords` varying and `size` uniform)
//! where scenefx read `gl_FragCoord` plus its own `position` uniform, and the
//! rounded-corner clipping is dropped along with the `corner_alpha` helper it
//! called, since we have no rounded corners to clip against. The maths - erf,
//! the gaussian, and both rounded-box functions - is theirs, untouched.
//!
//! Each window gets one `PixelShaderElement` covering its rectangle grown by the
//! blur radius, composited UNDER the windows (see the element order in winit.rs).

use std::collections::HashMap;

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{
    GlesPixelProgram, GlesRenderer, Uniform, UniformName, UniformType, element::PixelShaderElement,
};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::utils::{Logical, Rectangle};

/// How far the shadow reaches beyond the window edge, in logical points. It is
/// also the shader's blur sigma, so the visible falloff is about this wide.
const SPREAD: f32 = 18.0;
/// Corner rounding the shadow is shaped with. Client windows here are square, so
/// a small radius only softens the corners of the shadow itself.
const CORNER_RADIUS: f32 = 8.0;
/// Shadow opacity directly under the window edge.
const OPACITY: f32 = 0.45;

const SHADER: &str = r#"
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif

varying vec2 v_coords;

uniform vec2 size;
uniform float alpha;
uniform float blur_sigma;
uniform float corner_radius;
uniform vec4 shadow_color;

float gaussian(float x, float sigma) {
    const float pi = 3.141592653589793;
    return exp(-(x * x) / (2.0 * sigma * sigma)) / (sqrt(2.0 * pi) * sigma);
}

vec2 erf(vec2 x) {
    vec2 s = sign(x), a = abs(x);
    x = 1.0 + (0.278393 + (0.230389 + 0.078108 * (a * a)) * a) * a;
    x *= x;
    return s - s / (x * x);
}

float roundedBoxShadowX(float x, float y, float sigma, float corner, vec2 halfSize) {
    float delta = min(halfSize.y - corner - abs(y), 0.0);
    float curved = halfSize.x - corner + sqrt(max(0.0, corner * corner - delta * delta));
    vec2 integral = 0.5 + 0.5 * erf((x + vec2(-curved, curved)) * (sqrt(0.5) / sigma));
    return integral.y - integral.x;
}

float roundedBoxShadow(vec2 lower, vec2 upper, vec2 point, float sigma, float corner_radius) {
    vec2 center = (lower + upper) * 0.5;
    vec2 halfSize = (upper - lower) * 0.5;
    point -= center;

    float low = point.y - halfSize.y;
    float high = point.y + halfSize.y;
    float start = clamp(-3.0 * sigma, low, high);
    float end = clamp(3.0 * sigma, low, high);

    float step = (end - start) / 4.0;
    float y = start + step * 0.5;
    float value = 0.0;
    for (int i = 0; i < 4; i++) {
        value += roundedBoxShadowX(point.x, point.y - y, sigma, corner_radius, halfSize) * gaussian(y, sigma) * step;
        y += step;
    }

    return value;
}

void main() {
    // The window sits inset by the blur radius inside this element, so the
    // shadow box is the element shrunk by that much on every side.
    vec2 point = v_coords * size;
    float shadow = roundedBoxShadow(vec2(blur_sigma), size - blur_sigma, point,
                                    blur_sigma * 0.5, corner_radius);
    float a = shadow_color.a * shadow * alpha;
    gl_FragColor = vec4(shadow_color.rgb * a, a);   // premultiplied
}
"#;

/// Per-window shadow elements plus the compiled shader they share.
#[derive(Default)]
pub struct Shadows {
    program: Option<GlesPixelProgram>,
    /// One element per window, kept across frames so the damage tracker sees a
    /// stable element id (a fresh id every frame would re-damage every shadow).
    /// Keyed by the window's surface, so a closed window's entry is dropped.
    elements: HashMap<ObjectId, PixelShaderElement>,
}

impl Shadows {
    /// The shadow for ONE window, cached across frames so the damage tracker sees
    /// a stable element id. Compiles the shader on first use (needs a current GL
    /// context, so call it from the render path); if that fails there are simply
    /// no shadows, rather than no compositor.
    ///
    /// The caller draws this directly beneath that window, not beneath all of
    /// them: a shadow has to fall on the windows behind it, which is how scene
    /// graph compositors (scenefx, sway) place theirs.
    pub fn element(
        &mut self,
        renderer: &mut GlesRenderer,
        id: &ObjectId,
        geo: Rectangle<i32, Logical>,
        scale: f64,
    ) -> Option<PixelShaderElement> {
        if self.program.is_none() {
            self.program = renderer
                .compile_custom_pixel_shader(
                    SHADER,
                    &[
                        UniformName::new("blur_sigma", UniformType::_1f),
                        UniformName::new("corner_radius", UniformType::_1f),
                        UniformName::new("shadow_color", UniformType::_4f),
                    ],
                )
                .map_err(|e| tracing::warn!("shadow shader did not compile: {e}"))
                .ok();
        }
        let program = self.program.clone()?;
        let spread = SPREAD.round() as i32;
        // The element covers the window plus the blur on every side.
        let area = Rectangle::new(
            (geo.loc.x - spread, geo.loc.y - spread).into(),
            (geo.size.w + 2 * spread, geo.size.h + 2 * spread).into(),
        );
        let element = self.elements.entry(id.clone()).or_insert_with(|| {
            PixelShaderElement::new(
                program,
                area,
                None, // nothing here is opaque: it is a soft shadow
                1.0,
                vec![
                    // Sigma and radius are in the shader's own pixel space, so
                    // they scale with the output.
                    Uniform::new("blur_sigma", SPREAD * scale as f32),
                    Uniform::new("corner_radius", CORNER_RADIUS * scale as f32),
                    Uniform::new("shadow_color", [0.0, 0.0, 0.0, OPACITY]),
                ],
                Kind::Unspecified,
            )
        });
        element.resize(area, None);
        Some(element.clone())
    }

    /// Forget the shadows of windows that are gone, so the map cannot grow
    /// forever. Called with the ids still on screen.
    pub fn retain(&mut self, live: &[ObjectId]) {
        self.elements.retain(|id, _| live.contains(id));
    }
}
