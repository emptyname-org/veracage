//! Draw a PNG icon with a direct OpenGL call inside an eframe (glow) window,
//! bypassing egui's own textured-quad path.
//!
//! egui_glow uploads textures as sRGB and its fragment shader re-encodes samples
//! to gamma. Sampling a transparent-edged icon that way blends the rounded
//! corners in the wrong space and fringes them light. A plain RGBA8 texture
//! drawn with straight-alpha blending renders the edge correctly - the same
//! thing the compositor does for this icon via smithay's renderer.

use eframe::glow::{self, HasContext};

/// Vertex + fragment shaders, modern (GL 3 / ES 3) and legacy (GL 2 / ES 2).
const VS_MODERN: &str = r#"
in vec2 pos; in vec2 uv; out vec2 v_uv;
void main() { v_uv = uv; gl_Position = vec4(pos, 0.0, 1.0); }
"#;
const FS_MODERN: &str = r#"
in vec2 v_uv; out vec4 frag; uniform sampler2D tex;
void main() { frag = texture(tex, v_uv); }
"#;
const VS_LEGACY: &str = r#"
attribute vec2 pos; attribute vec2 uv; varying vec2 v_uv;
void main() { v_uv = uv; gl_Position = vec4(pos, 0.0, 1.0); }
"#;
const FS_LEGACY: &str = r#"
varying vec2 v_uv; uniform sampler2D tex;
void main() { gl_FragColor = texture2D(tex, v_uv); }
"#;

/// A GLSL version header appropriate for the context, and whether it is the
/// modern (in/out) dialect. Desktop GL >= 3.0 uses `#version 130` (the first
/// with in/out + a user-declared fragment `out`; 150 would demand GL 3.2);
/// older desktop GL uses `#version 120`, which has no `precision` qualifier
/// (so FS_LEGACY omits it for the desktop path).
fn shader_header(v: &glow::Version) -> (String, bool) {
    if v.is_embedded {
        if v.major >= 3 {
            ("#version 300 es\nprecision mediump float;\n".to_string(), true)
        } else {
            ("#version 100\nprecision mediump float;\n".to_string(), false)
        }
    } else if v.major >= 3 {
        ("#version 130\n".to_string(), true)
    } else {
        ("#version 120\n".to_string(), false)
    }
}

pub struct GlIcon {
    program: glow::Program,
    vbo: glow::Buffer,
    vao: Option<glow::VertexArray>,
    texture: glow::Texture,
    tex_loc: Option<glow::UniformLocation>,
    pos_loc: u32,
    uv_loc: u32,
}

impl GlIcon {
    /// Build from straight-alpha RGBA8 pixels. None on any GL setup error, in
    /// which case the caller simply draws no logo (there is no second egui-Image
    /// path - that would reintroduce the sRGB fringe this exists to avoid).
    pub fn new(gl: &glow::Context, w: i32, h: i32, rgba: &[u8]) -> Option<Self> {
        // Bound and checked-multiply the dimensions before trusting them as the
        // texture size: an out-of-range or overflowing w*h*4 must not pass the
        // length check and then drive an out-of-bounds tex_image_2d read.
        const MAX_SIDE: i32 = 4096;
        if !(1..=MAX_SIDE).contains(&w) || !(1..=MAX_SIDE).contains(&h) {
            return None;
        }
        let expected = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
        if rgba.len() != expected {
            return None;
        }
        unsafe {
            let (header, modern) = shader_header(gl.version());
            let vs = format!("{header}{}", if modern { VS_MODERN } else { VS_LEGACY });
            let fs = format!("{header}{}", if modern { FS_MODERN } else { FS_LEGACY });
            let program = link_program(gl, &vs, &fs)?;

            // Quad as a TRIANGLE_STRIP over NDC -1..1; V flipped so the image top
            // maps to the top of the rect. (x, y, u, v) interleaved.
            let verts: [f32; 16] = [
                -1.0, -1.0, 0.0, 1.0, //
                1.0, -1.0, 1.0, 1.0, //
                -1.0, 1.0, 0.0, 0.0, //
                1.0, 1.0, 1.0, 0.0,
            ];
            let vbo = gl.create_buffer().ok()?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            let bytes =
                std::slice::from_raw_parts(verts.as_ptr() as *const u8, std::mem::size_of_val(&verts));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);

            let texture = gl.create_texture().ok()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            // Plain RGBA8 (NOT sRGB): sampled as-is, no gamma round-trip.
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                w,
                h,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                Some(rgba),
            );

            let pos_loc = gl.get_attrib_location(program, "pos")?;
            let uv_loc = gl.get_attrib_location(program, "uv")?;
            let tex_loc = gl.get_uniform_location(program, "tex");

            // A VAO is required on core profiles; create one when available and
            // record the attribute layout into it.
            let vao = gl.create_vertex_array().ok();
            if let Some(vao) = vao {
                gl.bind_vertex_array(Some(vao));
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
                gl.enable_vertex_attrib_array(pos_loc);
                gl.vertex_attrib_pointer_f32(pos_loc, 2, glow::FLOAT, false, 16, 0);
                gl.enable_vertex_attrib_array(uv_loc);
                gl.vertex_attrib_pointer_f32(uv_loc, 2, glow::FLOAT, false, 16, 8);
                gl.bind_vertex_array(None);
            }

            Some(GlIcon { program, vbo, vao, texture, tex_loc, pos_loc, uv_loc })
        }
    }

    /// Draw the icon filling the given viewport rect (GL pixels, bottom-left
    /// origin: left, from_bottom, width, height).
    pub fn paint(&self, gl: &glow::Context, left: i32, from_bottom: i32, width: i32, height: i32) {
        unsafe {
            gl.viewport(left, from_bottom, width, height);
            gl.use_program(Some(self.program));
            if let Some(vao) = self.vao {
                gl.bind_vertex_array(Some(vao));
            }
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            gl.enable_vertex_attrib_array(self.pos_loc);
            gl.vertex_attrib_pointer_f32(self.pos_loc, 2, glow::FLOAT, false, 16, 0);
            gl.enable_vertex_attrib_array(self.uv_loc);
            gl.vertex_attrib_pointer_f32(self.uv_loc, 2, glow::FLOAT, false, 16, 8);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
            gl.uniform_1_i32(self.tex_loc.as_ref(), 0);
            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            if self.vao.is_some() {
                gl.bind_vertex_array(None);
            }
        }
    }
}

/// Compile + link a program; None (and logs) on any shader error.
unsafe fn link_program(gl: &glow::Context, vs: &str, fs: &str) -> Option<glow::Program> {
    let compile = |ty: u32, src: &str| -> Option<glow::Shader> {
        let s = gl.create_shader(ty).ok()?;
        gl.shader_source(s, src);
        gl.compile_shader(s);
        if gl.get_shader_compile_status(s) {
            Some(s)
        } else {
            eprintln!("veracage: icon shader compile failed: {}", gl.get_shader_info_log(s));
            gl.delete_shader(s);
            None
        }
    };
    let v = compile(glow::VERTEX_SHADER, vs)?;
    let f = compile(glow::FRAGMENT_SHADER, fs)?;
    let program = gl.create_program().ok()?;
    gl.attach_shader(program, v);
    gl.attach_shader(program, f);
    gl.link_program(program);
    gl.delete_shader(v);
    gl.delete_shader(f);
    if gl.get_program_link_status(program) {
        Some(program)
    } else {
        eprintln!("veracage: icon program link failed: {}", gl.get_program_info_log(program));
        gl.delete_program(program);
        None
    }
}
