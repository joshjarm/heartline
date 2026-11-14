//! Rendering: background shader, arena / Heart / entities, drift-mode (idle) visuals,
//! bloom + CRT composite, and the HUD. Everything is procedural — no art assets.

use macroquad::miniquad::UniformType;
use macroquad::prelude::*;
use std::f32::consts::{PI, TAU};

use crate::analysis::{SectionKind, N_SPEC};
use crate::game::*;
use crate::level::*;

// ---------------------------------------------------------------------------------------------
// Palettes
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg0: Color,
    pub bg1: Color,
    pub grid: Color,
    pub primary: Color,
    pub secondary: Color,
    pub accent: Color,
    pub danger: Color,
}

fn hex(h: u32) -> Color {
    Color::new(
        ((h >> 16) & 0xff) as f32 / 255.0,
        ((h >> 8) & 0xff) as f32 / 255.0,
        (h & 0xff) as f32 / 255.0,
        1.0,
    )
}

pub fn palette(i: u8) -> Palette {
    match i % N_PALETTES {
        // Sunset
        0 => Palette { bg0: hex(0x0d0221), bg1: hex(0x2b0a3d), grid: hex(0xff4fd8), primary: hex(0x19f0ff), secondary: hex(0xff71ce), accent: hex(0xfff36b), danger: hex(0xff3864) },
        // Miami
        1 => Palette { bg0: hex(0x040a26), bg1: hex(0x0b1f4a), grid: hex(0x01cdfe), primary: hex(0x05ffa1), secondary: hex(0xb967ff), accent: hex(0xfffb96), danger: hex(0xff4f79) },
        // Dusk
        2 => Palette { bg0: hex(0x150727), bg1: hex(0x3a0f55), grid: hex(0xb967ff), primary: hex(0x7df9ff), secondary: hex(0xff901f), accent: hex(0xffd319), danger: hex(0xff2975) },
        // Aqua
        3 => Palette { bg0: hex(0x02141f), bg1: hex(0x04364a), grid: hex(0x2de2e6), primary: hex(0xd8fff8), secondary: hex(0x2de2e6), accent: hex(0xff6ac1), danger: hex(0xff3864) },
        // Rose
        _ => Palette { bg0: hex(0x0e0626), bg1: hex(0x2a1060), grid: hex(0xc084fc), primary: hex(0x7df9ff), secondary: hex(0xff9de1), accent: hex(0xc4ffb3), danger: hex(0xff4a5e) },
    }
}

fn mixc(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color::new(a.r + (b.r - a.r) * t, a.g + (b.g - a.g) * t, a.b + (b.b - a.b) * t, a.a + (b.a - a.a) * t)
}

fn alpha(c: Color, a: f32) -> Color {
    Color::new(c.r, c.g, c.b, c.a * a.clamp(0.0, 1.0))
}

fn lerp_pal(a: &Palette, b: &Palette, t: f32) -> Palette {
    Palette {
        bg0: mixc(a.bg0, b.bg0, t),
        bg1: mixc(a.bg1, b.bg1, t),
        grid: mixc(a.grid, b.grid, t),
        primary: mixc(a.primary, b.primary, t),
        secondary: mixc(a.secondary, b.secondary, t),
        accent: mixc(a.accent, b.accent, t),
        danger: mixc(a.danger, b.danger, t),
    }
}

fn v3(c: Color) -> Vec3 {
    vec3(c.r, c.g, c.b)
}

// ---------------------------------------------------------------------------------------------
// Shape helpers
// ---------------------------------------------------------------------------------------------

fn rot2(v: Vec2, a: f32) -> Vec2 {
    let (s, c) = a.sin_cos();
    vec2(v.x * c - v.y * s, v.x * s + v.y * c)
}

fn rquad(center: Vec2, half: Vec2, rot: f32, color: Color) {
    let a = center + rot2(vec2(-half.x, -half.y), rot);
    let b = center + rot2(vec2(half.x, -half.y), rot);
    let c = center + rot2(vec2(half.x, half.y), rot);
    let d = center + rot2(vec2(-half.x, half.y), rot);
    draw_triangle(a, b, c, color);
    draw_triangle(a, c, d, color);
}

fn rquad_lines(center: Vec2, half: Vec2, rot: f32, thick: f32, color: Color) {
    let p = [
        center + rot2(vec2(-half.x, -half.y), rot),
        center + rot2(vec2(half.x, -half.y), rot),
        center + rot2(vec2(half.x, half.y), rot),
        center + rot2(vec2(-half.x, half.y), rot),
    ];
    for i in 0..4 {
        let (a, b) = (p[i], p[(i + 1) % 4]);
        let e = (b - a).normalize_or_zero() * thick * 0.5;
        draw_line(a.x - e.x, a.y - e.y, b.x + e.x, b.y + e.y, thick, color);
    }
}

fn poly_pts(center: Vec2, n: usize, r: f32, rot: f32) -> Vec<Vec2> {
    (0..n)
        .map(|i| center + vec2((rot + i as f32 * TAU / n as f32).cos(), (rot + i as f32 * TAU / n as f32).sin()) * r)
        .collect()
}

fn poly_fill(center: Vec2, n: usize, r: f32, rot: f32, color: Color) {
    let p = poly_pts(center, n, r, rot);
    for i in 0..n {
        draw_triangle(center, p[i], p[(i + 1) % n], color);
    }
}

fn poly_lines(center: Vec2, n: usize, r: f32, rot: f32, thick: f32, color: Color) {
    let p = poly_pts(center, n, r, rot);
    for i in 0..n {
        let (a, b) = (p[i], p[(i + 1) % n]);
        let e = (b - a).normalize_or_zero() * thick * 0.35;
        draw_line(a.x - e.x, a.y - e.y, b.x + e.x, b.y + e.y, thick, color);
    }
}

/// Thick arc from angle a0 to a1 (radians), drawn as a triangle strip.
fn arc(center: Vec2, r: f32, a0: f32, a1: f32, thick: f32, color: Color) {
    let span = a1 - a0;
    if span.abs() < 1e-4 || r <= 0.0 {
        return;
    }
    let segs = ((span.abs() * r / 14.0).ceil() as usize).clamp(3, 96);
    let (ri, ro) = ((r - thick * 0.5).max(0.0), r + thick * 0.5);
    let mut prev_i = center + vec2(a0.cos(), a0.sin()) * ri;
    let mut prev_o = center + vec2(a0.cos(), a0.sin()) * ro;
    for s in 1..=segs {
        let a = a0 + span * s as f32 / segs as f32;
        let d = vec2(a.cos(), a.sin());
        let (ci, co) = (center + d * ri, center + d * ro);
        draw_triangle(prev_i, prev_o, co, color);
        draw_triangle(prev_i, co, ci, color);
        prev_i = ci;
        prev_o = co;
    }
}

fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

pub fn spaced(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

pub fn text_w(s: &str, size: f32) -> f32 {
    measure_text(s, None, size.max(1.0) as u16, 1.0).width
}

pub fn text_left(s: &str, x: f32, y: f32, size: f32, color: Color) {
    draw_text_ex(s, x, y, TextParams { font_size: size.max(1.0) as u16, color, ..Default::default() });
}

pub fn text_center(s: &str, x: f32, y: f32, size: f32, color: Color) {
    let w = text_w(s, size);
    text_left(s, x - w * 0.5, y, size, color);
}

pub fn text_right(s: &str, x: f32, y: f32, size: f32, color: Color) {
    let w = text_w(s, size);
    text_left(s, x - w, y, size, color);
}

/// Text with a soft dark backing so it stays readable over the scene.
pub fn text_shadow(s: &str, x: f32, y: f32, size: f32, color: Color, align: i8) {
    let w = text_w(s, size);
    let x0 = match align {
        0 => x - w * 0.5,
        1 => x - w,
        _ => x,
    };
    let o = (size * 0.07).max(1.0);
    text_left(s, x0 + o, y + o, size, Color::new(0.0, 0.0, 0.05, 0.75 * color.a));
    text_left(s, x0, y, size, color);
}

// ---------------------------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------------------------

pub const VIEW_W: f32 = 1920.0;
pub const VIEW_H: f32 = 1080.0;

#[derive(Clone, Copy)]
pub struct View {
    pub sw: f32,
    pub sh: f32,
    pub scale: f32,
    pub center: Vec2,
}

impl View {
    pub fn to_world(&self, p: Vec2) -> Vec2 {
        (p - vec2(self.sw, self.sh) * 0.5) / self.scale + self.center
    }
    pub fn to_screen(&self, w: Vec2) -> Vec2 {
        (w - self.center) * self.scale + vec2(self.sw, self.sh) * 0.5
    }
}

// ---------------------------------------------------------------------------------------------
// Shaders
// ---------------------------------------------------------------------------------------------

const VERT: &str = r#"#version 100
attribute vec3 position;
attribute vec2 texcoord;
attribute vec4 color0;
varying highp vec2 uv;
varying lowp vec4 color;
uniform mat4 Model;
uniform mat4 Projection;
void main() {
    gl_Position = Projection * Model * vec4(position, 1);
    color = color0 / 255.0;
    uv = texcoord;
}
"#;

const BG_FRAG: &str = r#"#version 100
precision highp float;
varying vec2 uv;
uniform vec2 u_res;
uniform float u_time;
uniform float u_beat;
uniform float u_bass;
uniform float u_idle;
uniform float u_scale;
uniform vec2 u_cam;
uniform vec2 u_arena;
uniform vec3 u_c0;
uniform vec3 u_c1;
uniform vec3 u_grid;
uniform vec3 u_sunA;
uniform vec3 u_sunB;

float hash(vec2 p) { return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453); }

vec3 topdown(vec2 wp) {
    vec3 col = mix(u_c0, u_c1, clamp(uv.y * 0.9 + 0.05, 0.0, 1.0));
    float d = length(wp);
    vec2 dv = wp / max(d, 1.0);
    float rip = sin(d * 0.018 - u_time * 2.2) * (5.0 + 20.0 * u_bass);
    vec2 gp = wp + dv * rip;
    vec2 g = abs(fract(gp / 96.0 + 0.5) - 0.5) * 96.0;
    float px = 1.0 / u_scale;
    float line = 1.0 - smoothstep(0.0, 1.5 * px + 0.8, min(g.x, g.y));
    vec2 a = abs(wp) - u_arena;
    float outside = smoothstep(-4.0, 30.0, max(a.x, a.y));
    float fade = exp(-d * 0.0011);
    col += u_grid * line * (0.07 + 0.22 * u_beat * fade) * (0.25 + 0.75 * outside) * (0.4 + 0.6 * fade);
    // striped sun behind the Heart
    float sr = 255.0 + 14.0 * u_beat;
    float sd = d / sr;
    if (sd < 1.0) {
        float yy = wp.y / sr;
        float band = yy < 0.0 ? 1.0 : step(0.14 + 0.5 * yy, fract(yy * 6.0 - u_time * 0.15));
        vec3 sc = mix(u_sunA, u_sunB, clamp(yy * 0.5 + 0.5, 0.0, 1.0));
        col += sc * band * 0.13 * smoothstep(1.0, 0.97, sd);
    }
    col += u_sunB * 0.09 * exp(-sd * sd * 1.3);
    return col;
}

vec3 horizon(vec2 p) {
    float hz = 0.56;
    vec3 col;
    if (p.y < hz) {
        float t = p.y / hz;
        col = mix(u_c0 * 0.6, mix(u_c1, u_sunB, 0.45), pow(t, 2.2));
        vec2 sp = floor(p * u_res / 3.0);
        float st = step(0.9975, hash(sp)) * (0.5 + 0.5 * sin(u_time * 2.0 + hash(sp + 1.3) * 40.0));
        col += vec3(st) * (1.0 - t);
        vec2 sc = vec2(0.5, hz - 0.12);
        vec2 q = (p - sc) * vec2(u_res.x / u_res.y, 1.0);
        float r = 0.24 + 0.006 * u_beat;
        float dd = length(q);
        float yy = q.y / r;
        float mask = yy < 0.0 ? 1.0 : step(0.12 + 0.55 * yy, fract(yy * 6.0 - u_time * 0.2));
        vec3 suncol = mix(u_sunA, u_sunB, clamp(yy * 0.5 + 0.5, 0.0, 1.0));
        col = mix(col, suncol * 0.82, smoothstep(r, r - 0.004, dd) * mask);
        col += u_sunB * 0.22 * exp(-dd * dd * 16.0);
    } else {
        float y = (p.y - hz) / (1.0 - hz);
        float z = 1.0 / (y + 0.02);
        float x = (p.x - 0.5) * z * 2.4;
        float gx = abs(fract(x) - 0.5);
        float gz = abs(fract(z * 0.9 + u_time * 0.9) - 0.5);
        float wx = min(1.5 * 2.4 * z / u_res.x, 0.5);
        float wz = min(1.5 * 0.9 * z * z / ((1.0 - hz) * u_res.y), 0.5);
        float lx = (1.0 - smoothstep(wx, wx * 2.5 + 0.002, gx)) * smoothstep(0.45, 0.12, wx);
        float lz = (1.0 - smoothstep(wz, wz * 2.5 + 0.002, gz)) * smoothstep(0.30, 0.06, wz);
        float g = max(lx, lz);
        float fade = smoothstep(0.0, 0.22, y);
        col = mix(u_c0 * 0.5, u_c1 * 0.6, y) + u_grid * g * (0.08 + 0.75 * fade) * (0.8 + 0.5 * u_beat);
        col += u_sunB * 0.28 * exp(-y * 9.0);
    }
    return col;
}

void main() {
    vec2 wp = u_cam + (uv - 0.5) * u_res / u_scale;
    vec3 col = topdown(wp);
    if (u_idle > 0.003) {
        col = mix(col, horizon(uv), u_idle);
    }
    gl_FragColor = vec4(col, 1.0);
}
"#;

const BLUR_FRAG: &str = r#"#version 100
precision highp float;
varying vec2 uv;
uniform sampler2D Texture;
uniform vec2 u_dir;
uniform float u_thresh;

vec3 samp(vec2 p) {
    vec3 c = texture2D(Texture, p).rgb;
    if (u_thresh > 0.0) {
        float l = max(c.r, max(c.g, c.b));
        c *= smoothstep(u_thresh, u_thresh + 0.4, l);
    }
    return c;
}

void main() {
    vec3 c = samp(uv) * 0.227027;
    c += (samp(uv + u_dir * 1.3846) + samp(uv - u_dir * 1.3846)) * 0.316216;
    c += (samp(uv + u_dir * 3.2308) + samp(uv - u_dir * 3.2308)) * 0.070270;
    gl_FragColor = vec4(c, 1.0);
}
"#;

const COMP_FRAG: &str = r#"#version 100
precision highp float;
varying vec2 uv;
uniform sampler2D Texture;
uniform sampler2D Bloom1;
uniform sampler2D Bloom2;
uniform vec2 u_res;
uniform float u_time;
uniform float u_aberr;
uniform float u_flash;
uniform vec3 u_flashcol;
uniform float u_bloom;

void main() {
    vec2 c = uv - 0.5;
    float r2 = dot(c, c);
    vec2 off = c * (0.0016 + u_aberr * 0.010) * (0.3 + r2 * 2.4);
    vec3 col;
    col.r = texture2D(Texture, uv + off).r;
    col.g = texture2D(Texture, uv).g;
    col.b = texture2D(Texture, uv - off).b;
    vec3 b1 = texture2D(Bloom1, uv).rgb;
    vec3 b2 = texture2D(Bloom2, uv).rgb;
    col += (b1 * 0.55 + b2 * 0.9) * u_bloom;
    float scan = 0.95 + 0.05 * sin(uv.y * u_res.y * 3.14159);
    col *= scan;
    float vig = smoothstep(1.05, 0.3, sqrt(r2) * 1.3);
    col *= mix(0.5, 1.0, vig);
    col += u_flashcol * u_flash;
    float n = fract(sin(dot(uv * u_res + u_time, vec2(12.9898, 78.233))) * 43758.5453);
    col += (n - 0.5) * 0.022;
    gl_FragColor = vec4(col, 1.0);
}
"#;

fn mat(frag: &str, uniforms: &[(&str, UniformType)], textures: &[&str]) -> Material {
    load_material(
        ShaderSource::Glsl { vertex: VERT, fragment: frag },
        MaterialParams {
            uniforms: uniforms.iter().map(|(n, t)| UniformDesc::new(n, *t)).collect(),
            textures: textures.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        },
    )
    .unwrap_or_else(|e| panic!("shader failed to compile: {e:?}"))
}

// ---------------------------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------------------------

pub struct HudInfo {
    pub title: String,
    pub progress: f32,
    pub time_left: f32,
    pub bpm: f32,
    pub mode: String,
    pub status: String,
    pub track_no: String,
    pub show_combat: bool,
    /// Large centred announcement (e.g. "TRACK CLEAR") and its opacity.
    pub banner: String,
    pub banner_a: f32,
}

impl HudInfo {
    pub fn empty() -> Self {
        HudInfo {
            title: String::new(),
            progress: 0.0,
            time_left: 0.0,
            bpm: 0.0,
            mode: String::new(),
            status: String::new(),
            track_no: String::new(),
            show_combat: false,
            banner: String::new(),
            banner_a: 0.0,
        }
    }
}

pub struct Renderer {
    pw: u32,
    ph: u32,
    pub ss: u32,
    ss_forced: Option<u32>,
    scene: RenderTarget,
    b1a: RenderTarget,
    b1b: RenderTarget,
    b2a: RenderTarget,
    b2b: RenderTarget,
    bg_mat: Material,
    blur_mat: Material,
    comp_mat: Material,
    pub pal: Palette,
    pal_from: Palette,
    pal_to: u8,
    pal_t: f32,
    pub time: f32,
    pub view: View,
}

fn auto_ss(forced: Option<u32>, pw: u32, ph: u32) -> u32 {
    match forced {
        Some(s) => s.clamp(1, 2),
        None => {
            if (pw as u64) * (ph as u64) <= 2600 * 1500 {
                2
            } else {
                1
            }
        }
    }
}

fn make_rt(w: u32, h: u32) -> RenderTarget {
    let rt = render_target(w.max(8), h.max(8));
    rt.texture.set_filter(FilterMode::Linear);
    rt
}

impl Renderer {
    /// `ss_forced`: supersampling factor (1 or 2); `None` picks 2x unless the window is huge.
    pub fn new(ss_forced: Option<u32>) -> Self {
        let dpi = screen_dpi_scale();
        let pw = (screen_width() * dpi).round().max(64.0) as u32;
        let ph = (screen_height() * dpi).round().max(64.0) as u32;
        let ss = auto_ss(ss_forced, pw, ph);
        let bg_mat = mat(
            BG_FRAG,
            &[
                ("u_res", UniformType::Float2),
                ("u_time", UniformType::Float1),
                ("u_beat", UniformType::Float1),
                ("u_bass", UniformType::Float1),
                ("u_idle", UniformType::Float1),
                ("u_scale", UniformType::Float1),
                ("u_cam", UniformType::Float2),
                ("u_arena", UniformType::Float2),
                ("u_c0", UniformType::Float3),
                ("u_c1", UniformType::Float3),
                ("u_grid", UniformType::Float3),
                ("u_sunA", UniformType::Float3),
                ("u_sunB", UniformType::Float3),
            ],
            &[],
        );
        let blur_mat = mat(BLUR_FRAG, &[("u_dir", UniformType::Float2), ("u_thresh", UniformType::Float1)], &[]);
        let comp_mat = mat(
            COMP_FRAG,
            &[
                ("u_res", UniformType::Float2),
                ("u_time", UniformType::Float1),
                ("u_aberr", UniformType::Float1),
                ("u_flash", UniformType::Float1),
                ("u_flashcol", UniformType::Float3),
                ("u_bloom", UniformType::Float1),
            ],
            &["Bloom1", "Bloom2"],
        );
        let pal = palette(0);
        let mut r = Renderer {
            pw,
            ph,
            ss,
            ss_forced,
            scene: make_rt(pw * ss, ph * ss),
            b1a: make_rt(pw / 2, ph / 2),
            b1b: make_rt(pw / 2, ph / 2),
            b2a: make_rt(pw / 4, ph / 4),
            b2b: make_rt(pw / 4, ph / 4),
            bg_mat,
            blur_mat,
            comp_mat,
            pal,
            pal_from: pal,
            pal_to: 0,
            pal_t: 1.0,
            time: 0.0,
            view: View { sw: screen_width(), sh: screen_height(), scale: 1.0, center: Vec2::ZERO },
        };
        r.bind_bloom();
        r
    }

    fn bind_bloom(&mut self) {
        self.comp_mat.set_texture("Bloom1", self.b1b.texture.clone());
        self.comp_mat.set_texture("Bloom2", self.b2b.texture.clone());
    }

    fn resize_if_needed(&mut self) {
        let dpi = screen_dpi_scale();
        let pw = (screen_width() * dpi).round().max(64.0) as u32;
        let ph = (screen_height() * dpi).round().max(64.0) as u32;
        if pw != self.pw || ph != self.ph {
            self.pw = pw;
            self.ph = ph;
            self.ss = auto_ss(self.ss_forced, pw, ph);
            self.scene = make_rt(pw * self.ss, ph * self.ss);
            self.b1a = make_rt(pw / 2, ph / 2);
            self.b1b = make_rt(pw / 2, ph / 2);
            self.b2a = make_rt(pw / 4, ph / 4);
            self.b2b = make_rt(pw / 4, ph / 4);
            self.bind_bloom();
        }
    }

    pub fn set_palette(&mut self, idx: u8) {
        if idx != self.pal_to {
            self.pal_from = self.pal;
            self.pal_to = idx;
            self.pal_t = 0.0;
        }
    }

    /// Compute the view for this frame (call before reading the mouse in world space).
    pub fn update_view(&mut self, world: &World) -> View {
        let (sw, sh) = (screen_width(), screen_height());
        let scale = (sw / VIEW_W).min(sh / VIEW_H);
        let sh_amp = world.shake * world.shake * 16.0;
        let t = self.time * 47.0;
        let shake = vec2((t * 1.3).sin() + (t * 2.9).cos(), (t * 1.7).cos() + (t * 3.1).sin()) * sh_amp * 0.5;
        self.view = View { sw, sh, scale, center: world.player.pos * 0.10 + shake };
        self.view
    }

    fn pass(&self, target: &RenderTarget, src: &Texture2D, dir: Vec2, thresh: f32) {
        set_camera(&Camera2D {
            target: vec2(0.5, 0.5),
            zoom: vec2(2.0, 2.0),
            render_target: Some(target.clone()),
            ..Default::default()
        });
        self.blur_mat.set_uniform("u_dir", dir);
        self.blur_mat.set_uniform("u_thresh", thresh);
        gl_use_material(&self.blur_mat);
        draw_texture_ex(src, 0.0, 0.0, WHITE, DrawTextureParams { dest_size: Some(vec2(1.0, 1.0)), ..Default::default() });
        gl_use_default_material();
    }

    /// Draw the whole scene (background, world, drift visuals) and post-process it to the screen.
    pub fn draw_scene(&mut self, world: &World, dt: f32, idle: f32, progress: f32) {
        self.resize_if_needed();
        self.time += dt;
        if self.pal_t < 1.0 {
            self.pal_t = (self.pal_t + dt * 1.4).min(1.0);
            self.pal = lerp_pal(&self.pal_from, &palette(self.pal_to), ease(self.pal_t));
        }
        let pal = self.pal;
        let view = self.view;
        let (rw, rh) = ((self.pw * self.ss) as f32, (self.ph * self.ss) as f32);
        let px_scale = view.scale * rw / view.sw;
        let idle_beat = (-((self.time * 100.0 / 60.0).fract()) * 5.0).exp();
        let beat = world.beat_pulse * (1.0 - idle) + idle_beat * idle * 0.6;

        // ---- background ----
        set_camera(&Camera2D {
            target: vec2(0.5, 0.5),
            zoom: vec2(2.0, 2.0),
            render_target: Some(self.scene.clone()),
            ..Default::default()
        });
        clear_background(BLACK);
        let m = &self.bg_mat;
        m.set_uniform("u_res", vec2(rw, rh));
        m.set_uniform("u_time", self.time);
        m.set_uniform("u_beat", beat);
        m.set_uniform("u_bass", world.feat.low.min(1.2) * (1.0 - idle));
        m.set_uniform("u_idle", ease(idle));
        m.set_uniform("u_scale", px_scale);
        m.set_uniform("u_cam", view.center);
        m.set_uniform("u_arena", vec2(HALF_W, HALF_H));
        m.set_uniform("u_c0", v3(pal.bg0));
        m.set_uniform("u_c1", v3(pal.bg1));
        m.set_uniform("u_grid", v3(pal.grid));
        m.set_uniform("u_sunA", v3(pal.accent));
        m.set_uniform("u_sunB", v3(pal.secondary));
        gl_use_material(m);
        draw_rectangle(0.0, 0.0, 1.0, 1.0, WHITE);
        gl_use_default_material();

        // ---- world ----
        set_camera(&Camera2D {
            target: view.center,
            zoom: vec2(2.0 * view.scale / view.sw, 2.0 * view.scale / view.sh),
            render_target: Some(self.scene.clone()),
            ..Default::default()
        });
        let lv = 1.0 - ease(idle);
        let tscale = px_scale / screen_dpi_scale();
        if lv > 0.003 {
            self.draw_arena(world, lv);
            self.draw_hazards(world, lv);
        }
        self.draw_idle(world, idle);
        self.draw_pickups(world, tscale);
        if lv > 0.003 {
            self.draw_enemies(world, lv);
            self.draw_bullets(world, lv);
        }
        self.draw_shots(world);
        if lv > 0.003 {
            self.draw_heart(world, lv, progress);
        }
        self.draw_player(world);
        self.draw_particles(world, tscale);

        // ---- bloom ----
        let (hw, hh) = ((self.pw / 2) as f32, (self.ph / 2) as f32);
        let (qw, qh) = ((self.pw / 4) as f32, (self.ph / 4) as f32);
        self.pass(&self.b1a, &self.scene.texture, vec2(1.0 / hw, 0.0), 0.55);
        self.pass(&self.b1b, &self.b1a.texture, vec2(0.0, 1.0 / hh), 0.0);
        self.pass(&self.b2a, &self.b1b.texture, vec2(1.6 / qw, 0.0), 0.0);
        self.pass(&self.b2b, &self.b2a.texture, vec2(0.0, 1.6 / qh), 0.0);

        // ---- composite to the screen ----
        set_default_camera();
        clear_background(BLACK);
        let c = &self.comp_mat;
        c.set_uniform("u_res", vec2(self.pw as f32, self.ph as f32));
        c.set_uniform("u_time", self.time);
        c.set_uniform("u_aberr", (world.beat_pulse * 0.25 + world.shake * 0.9 + world.flash * 0.6).min(1.5) * (1.0 - idle * 0.7));
        c.set_uniform("u_flash", world.flash * 0.5);
        c.set_uniform("u_flashcol", v3(mixc(pal.secondary, WHITE, 0.5)));
        c.set_uniform("u_bloom", (1.0 + 0.25 * beat) * (1.0 - 0.5 * ease(idle)));
        gl_use_material(c);
        draw_texture_ex(
            &self.scene.texture,
            0.0,
            0.0,
            WHITE,
            DrawTextureParams { dest_size: Some(vec2(view.sw, view.sh)), ..Default::default() },
        );
        gl_use_default_material();
    }

    // -----------------------------------------------------------------------------------------

    fn draw_arena(&self, w: &World, lv: f32) {
        let pal = &self.pal;
        let half = TILE * 0.5 - 4.0;
        let mid = (ROWS / 2) as i32;
        for r in 0..ROWS {
            for c in 0..COLS {
                let tile = &w.tiles[r * COLS + c];
                let p = tile_center(c, r);
                // Floor doubles as a mirrored spectrum analyser.
                let band = c * (N_SPEC - 1) / (COLS - 1);
                let v = w.feat.spec[band];
                let dr = (r as i32 - mid).abs() as f32;
                let lit = (v * 5.2 - dr).clamp(0.0, 1.0);
                let base = mixc(pal.primary, pal.secondary, dr / 4.0);
                rquad(p, vec2(half, half), 0.0, alpha(base, (0.035 + 0.17 * lit) * lv));
                rquad_lines(p, vec2(half, half), 0.0, 1.5, alpha(base, (0.10 + 0.30 * lit + 0.08 * w.beat_pulse) * lv));
                if tile.armed {
                    let k = (1.0 - (tile.strike_t - w.t) / tile.lead).clamp(0.0, 1.0);
                    rquad_lines(p, vec2(half, half), 0.0, 3.0, alpha(pal.danger, (0.35 + 0.6 * k) * lv));
                    let h2 = half * k;
                    rquad(p, vec2(h2, h2), 0.0, alpha(pal.danger, (0.12 + 0.22 * k) * lv));
                }
                if tile.flash > 0.0 {
                    rquad(p, vec2(half + 3.0, half + 3.0), 0.0, alpha(mixc(pal.danger, WHITE, tile.flash), tile.flash * 0.85 * lv));
                }
            }
        }
        // Arena frame
        let k = 0.55 + 0.45 * w.bar_pulse;
        rquad_lines(Vec2::ZERO, vec2(HALF_W + 6.0, HALF_H + 6.0), 0.0, 3.0, alpha(pal.primary, k * lv));
        let cl = 46.0;
        for (sx, sy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let corner = vec2(sx * (HALF_W + 18.0), sy * (HALF_H + 18.0));
            draw_line(corner.x, corner.y, corner.x - sx * cl, corner.y, 4.0, alpha(pal.secondary, lv));
            draw_line(corner.x, corner.y, corner.x, corner.y - sy * cl, 4.0, alpha(pal.secondary, lv));
        }
    }

    fn draw_hazards(&self, w: &World, lv: f32) {
        let pal = &self.pal;
        let t = w.t;
        // Lasers
        for l in &w.lasers {
            let (span, _other) = if l.horizontal { (HALF_W + 40.0, HALF_H) } else { (HALF_H + 40.0, HALF_W) };
            let line = |pos: f32, thick: f32, col: Color| {
                if l.horizontal {
                    draw_line(-span, pos, span, pos, thick, col);
                } else {
                    draw_line(pos, -span, pos, span, thick, col);
                }
            };
            if t < l.t0 {
                let k = (1.0 - (l.t0 - t) / l.lead).clamp(0.0, 1.0);
                let blink = 0.5 + 0.5 * (t * 28.0).sin();
                line(l.from, 2.0 + 3.0 * k, alpha(pal.danger, (0.25 + 0.5 * k) * (0.6 + 0.4 * blink) * lv));
                // ghost of where it will travel
                line(l.to, 1.5, alpha(pal.danger, 0.18 * lv));
                let a = if l.horizontal { vec2(-span, l.from) } else { vec2(l.from, -span) };
                let b = if l.horizontal { vec2(-span, l.to) } else { vec2(l.to, -span) };
                draw_line(a.x, a.y, b.x, b.y, 1.5, alpha(pal.danger, 0.18 * lv));
                // converging brackets
                let wdt = 60.0 * (1.0 - k) + 14.0;
                line(l.from - wdt, 1.5, alpha(pal.danger, 0.4 * k * lv));
                line(l.from + wdt, 1.5, alpha(pal.danger, 0.4 * k * lv));
            } else if l.active(t) {
                let pos = l.pos_at(t);
                let wob = 1.0 + 0.2 * (t * 60.0).sin();
                line(pos, 34.0 * wob, alpha(pal.danger, 0.22 * lv));
                line(pos, 18.0 * wob, alpha(pal.danger, 0.7 * lv));
                line(pos, 7.0, alpha(WHITE, lv));
            } else {
                let k = (1.0 - (t - l.t0 - l.dur) / 0.3).clamp(0.0, 1.0);
                line(l.to, 10.0 * k, alpha(pal.danger, 0.5 * k * lv));
            }
        }
        // Shockwaves
        for wv in &w.waves {
            if t < wv.t0 {
                let k = ((t - (wv.t0 - wv.lead)) / wv.lead).clamp(0.0, 1.0);
                let rr = HEART_R + 26.0 + 300.0 * (1.0 - k);
                arc(Vec2::ZERO, rr, 0.0, TAU, 3.0, alpha(pal.danger, 0.7 * k * lv));
                // show where the safe gaps will be
                let n = wv.gaps.max(1);
                for g in 0..n {
                    let c = wv.gap + g as f32 * TAU / n as f32;
                    let d = vec2(c.cos(), c.sin());
                    draw_line(d.x * 110.0, d.y * 110.0, d.x * 900.0, d.y * 900.0, 3.0, alpha(pal.primary, 0.28 * k * lv));
                }
            } else {
                let rr = wv.radius(t);
                let n = wv.gaps.max(1);
                let half = 0.36;
                for g in 0..n {
                    let c0 = wv.gap + g as f32 * TAU / n as f32 + half;
                    let c1 = wv.gap + (g + 1) as f32 * TAU / n as f32 - half;
                    arc(Vec2::ZERO, rr, c0, c1, 26.0, alpha(pal.danger, 0.3 * lv));
                    arc(Vec2::ZERO, rr, c0, c1, 12.0, alpha(pal.danger, 0.9 * lv));
                    arc(Vec2::ZERO, rr, c0, c1, 4.0, alpha(WHITE, lv));
                }
            }
        }
        // Spawn portals
        for p in &w.portals {
            let k = ((t - p.t0) / (p.t1 - p.t0).max(0.05)).clamp(0.0, 1.0);
            let s = 64.0 * (1.0 - k) + 16.0;
            let col = if p.kind == EnemyKind::Elite { pal.danger } else { pal.secondary };
            rquad_lines(p.pos, vec2(s, s), k * PI, 3.0, alpha(col, (0.3 + 0.7 * k) * lv));
            rquad_lines(p.pos, vec2(s * 0.5, s * 0.5), -k * PI, 2.0, alpha(WHITE, 0.6 * k * lv));
            if p.kind == EnemyKind::Darter {
                let d = (w.player.pos - p.pos).normalize_or_zero();
                let e = p.pos + d * 1700.0;
                draw_line(p.pos.x, p.pos.y, e.x, e.y, 2.0, alpha(pal.accent, 0.35 * k * lv));
            }
        }
    }

    fn draw_pickups(&self, w: &World, tscale: f32) {
        let pal = &self.pal;
        for s in &w.shards {
            let fade = (s.life / 1.5).min(1.0) * (s.age * 6.0).min(1.0);
            let spin = self.time * 3.0 + s.pos.x * 0.01;
            if s.heal {
                let c = alpha(pal.primary, fade);
                rquad(s.pos, vec2(11.0, 3.5), 0.0, c);
                rquad(s.pos, vec2(3.5, 11.0), 0.0, c);
                poly_lines(s.pos, 4, 18.0 + 2.0 * (self.time * 5.0).sin(), PI * 0.25, 2.0, alpha(WHITE, fade * 0.8));
            } else {
                poly_fill(s.pos, 4, 7.0, spin, alpha(pal.accent, fade));
                poly_fill(s.pos, 4, 3.0, spin, alpha(WHITE, fade));
            }
        }
        for o in &w.orbs {
            let fade = (o.life / 2.0).min(1.0) * (o.age * 3.0).min(1.0);
            let pulse = 1.0 + 0.08 * (self.time * 5.0 + o.pos.x).sin();
            poly_fill(o.pos, 6, 26.0 * pulse, self.time * 0.8, alpha(pal.bg0, 0.85 * fade));
            poly_lines(o.pos, 6, 26.0 * pulse, self.time * 0.8, 3.5, alpha(pal.accent, fade));
            poly_lines(o.pos, 6, 38.0 * pulse, -self.time * 0.5, 1.5, alpha(pal.primary, fade * 0.7));
            arc(o.pos, 48.0, -PI * 0.5, -PI * 0.5 + TAU * (o.life / 14.0).clamp(0.0, 1.0), 2.0, alpha(WHITE, 0.5 * fade));
            wtext(o.up.name(), o.pos + vec2(0.0, -58.0), 28.0, alpha(pal.accent, fade), tscale);
            wtext(o.up.desc(), o.pos + vec2(0.0, 72.0), 18.0, alpha(WHITE, fade * 0.9), tscale);
            let lvl = w.player.up(o.up);
            if lvl > 0 {
                wtext(&format!("lv {} > {}", lvl, lvl + 1), o.pos + vec2(0.0, 7.0), 16.0, alpha(WHITE, fade * 0.8), tscale);
            } else {
                wtext("new", o.pos + vec2(0.0, 7.0), 16.0, alpha(WHITE, fade * 0.8), tscale);
            }
        }
    }

    fn draw_enemies(&self, w: &World, lv: f32) {
        let pal = &self.pal;
        for e in &w.enemies {
            let appear = (e.age * 6.0).min(1.0);
            let a = lv * appear;
            let (col, sides, rot) = match e.kind {
                EnemyKind::Chaser => (pal.secondary, 3, e.rot),
                EnemyKind::Mini => (pal.secondary, 3, e.rot),
                EnemyKind::Darter => (pal.accent, 4, e.rot),
                EnemyKind::Orbiter => (pal.primary, 4, e.rot),
                EnemyKind::Splitter => (pal.secondary, 6, e.age * 0.6),
                EnemyKind::Elite => (pal.danger, 5, e.age * 0.4),
            };
            let fill = mixc(mixc(pal.bg0, col, 0.28), WHITE, e.flash);
            let r = e.r * (1.0 + 0.15 * e.flash) * (1.15 - 0.15 * appear);
            if e.kind == EnemyKind::Darter {
                // long kite
                let d = vec2(rot.cos(), rot.sin());
                let n = vec2(-d.y, d.x);
                let (tip, tail) = (e.pos + d * r * 1.7, e.pos - d * r * 1.3);
                let (l, rr) = (e.pos + n * r * 0.6, e.pos - n * r * 0.6);
                draw_triangle(tip, l, rr, alpha(fill, a));
                draw_triangle(tail, l, rr, alpha(fill, a));
                for (p, q) in [(tip, l), (l, tail), (tail, rr), (rr, tip)] {
                    draw_line(p.x, p.y, q.x, q.y, 3.0, alpha(col, a));
                }
                continue;
            }
            poly_fill(e.pos, sides, r * 1.2, rot, alpha(fill, a));
            poly_lines(e.pos, sides, r * 1.2, rot, if e.kind == EnemyKind::Mini { 2.5 } else { 3.5 }, alpha(col, a));
            match e.kind {
                EnemyKind::Orbiter => poly_fill(e.pos, 4, r * 0.4, -rot, alpha(col, a)),
                EnemyKind::Splitter => poly_lines(e.pos, 3, r * 0.55, -rot * 2.0, 2.5, alpha(col, a)),
                EnemyKind::Elite => {
                    poly_lines(e.pos, 5, r * 0.6, -rot * 1.5, 2.5, alpha(WHITE, a * 0.8));
                    let frac = (e.hp / e.max_hp).clamp(0.0, 1.0);
                    arc(e.pos, r * 1.6, -PI * 0.5, -PI * 0.5 + TAU * frac, 3.0, alpha(pal.danger, a));
                }
                EnemyKind::Chaser => {
                    let d = vec2(rot.cos(), rot.sin());
                    let eye = e.pos + d * r * 0.25;
                    draw_circle(eye.x, eye.y, 3.2, alpha(WHITE, a));
                }
                _ => {}
            }
        }
    }

    fn draw_bullets(&self, w: &World, lv: f32) {
        let pal = &self.pal;
        for b in &w.bullets {
            let a = b.fade.clamp(0.0, 1.0) * lv * (b.age * 10.0 + 0.3).min(1.0);
            if b.soft {
                poly_fill(b.pos, 4, b.r * 1.25, b.age * 4.0, alpha(mixc(pal.bg0, WHITE, 0.25), a));
                poly_lines(b.pos, 4, b.r * 1.25, b.age * 4.0, 2.5, alpha(WHITE, a));
            } else {
                draw_poly(b.pos.x, b.pos.y, 10, b.r * 1.35, 0.0, alpha(pal.danger, a * 0.55));
                draw_poly(b.pos.x, b.pos.y, 10, b.r * 0.95, 0.0, alpha(pal.danger, a));
                draw_poly(b.pos.x, b.pos.y, 8, b.r * 0.45, 0.0, alpha(WHITE, a));
            }
        }
    }

    fn draw_shots(&self, w: &World) {
        let pal = &self.pal;
        for s in &w.shots {
            let d = s.vel.normalize_or_zero();
            let tail = s.pos - d * 26.0;
            draw_line(tail.x, tail.y, s.pos.x, s.pos.y, s.r * 1.6, alpha(pal.primary, 0.75));
            let mid = s.pos - d * 10.0;
            draw_line(mid.x, mid.y, s.pos.x, s.pos.y, s.r * 0.9, WHITE);
        }
    }

    fn draw_heart(&self, w: &World, lv: f32, progress: f32) {
        let pal = &self.pal;
        let h = &w.heart;
        let low = w.feat.low.min(1.2);
        let broken = h.broken;
        let a = if broken { 0.4 * lv } else { lv };

        // charge telegraph glow
        if h.charge > 0.0 {
            draw_poly(0.0, 0.0, 32, HEART_R + 30.0 * h.charge, 0.0, alpha(pal.danger, 0.25 * h.charge * lv));
        }

        // Rings
        let rr = h.rot;
        for k in 0..3 {
            let a0 = rr + k as f32 * TAU / 3.0;
            arc(Vec2::ZERO, 96.0 + 6.0 * h.punch, a0, a0 + 1.4, 5.0, alpha(pal.primary, 0.9 * a));
        }
        for k in 0..4 {
            let a0 = -rr * 0.7 + k as f32 * TAU / 4.0;
            arc(Vec2::ZERO, 114.0 + 10.0 * low, a0, a0 + 0.8, 3.0, alpha(pal.secondary, 0.8 * a));
        }
        // Song-progress ring (the clock) and integrity ring
        arc(Vec2::ZERO, 132.0, 0.0, TAU, 1.5, alpha(WHITE, 0.12 * lv));
        arc(Vec2::ZERO, 132.0, -PI * 0.5, -PI * 0.5 + TAU * progress.clamp(0.0, 1.0), 3.0, alpha(pal.accent, 0.9 * lv));
        if !broken {
            let frac = (h.hp / h.max_hp.max(1.0)).clamp(0.0, 1.0);
            arc(Vec2::ZERO, 144.0, PI * 0.5 - PI * frac, PI * 0.5 + PI * frac, 4.0, alpha(mixc(pal.danger, WHITE, h.flash), 0.85 * lv));
        }

        // Cube cluster
        let spread = 19.0 * (1.0 + 0.30 * h.punch + 0.22 * low + 1.3 * ease(h.reform) + if broken { 0.9 } else { 0.0 });
        let crot = PI * 0.25 + h.rot * 0.25 + ease(h.reform) * 1.5;
        let size = 8.0 * (1.0 + 0.12 * h.punch);
        for j in -2i32..=2 {
            for i in -2i32..=2 {
                let m = i.abs() + j.abs();
                if m > 3 {
                    continue;
                }
                let wob = (self.time * 2.0 + (i * 3 + j * 5) as f32).sin() * 1.5 * (1.0 + 2.0 * low);
                let p = rot2(vec2(i as f32, j as f32) * (spread + wob * 0.3), crot);
                let shade = match m {
                    0 => mixc(WHITE, pal.primary, 0.1),
                    1 => mixc(pal.primary, WHITE, 0.35),
                    2 => pal.primary,
                    _ => mixc(pal.primary, pal.bg1, 0.45),
                };
                let col = mixc(shade, WHITE, h.flash * 0.8);
                if broken {
                    rquad_lines(p, vec2(size, size), crot, 2.0, alpha(col, a));
                } else {
                    rquad(p, vec2(size, size), crot, alpha(col, a));
                    // top-face highlight gives the cubes a little depth
                    rquad(p + rot2(vec2(-1.6, -1.6), crot), vec2(size * 0.55, size * 0.55), crot, alpha(mixc(col, WHITE, 0.45), a));
                }
            }
        }
        if !broken {
            let core = 10.0 + 9.0 * h.punch + 5.0 * low;
            rquad(Vec2::ZERO, vec2(core, core), crot + PI * 0.25, alpha(WHITE, (0.5 + 0.5 * h.punch) * lv));
        }
    }

    fn draw_player(&self, w: &World) {
        let pal = &self.pal;
        let p = &w.player;
        if w.dead {
            return;
        }
        let blink = if p.invuln > 0.0 && p.dash_t <= 0.0 { 0.45 + 0.55 * ((self.time * 30.0).sin() * 0.5 + 0.5) } else { 1.0 };
        // drones
        for k in 0..p.drones() {
            let dp = p.drone_pos(k);
            rquad(dp, vec2(5.0, 5.0), self.time * 3.0, alpha(pal.accent, 0.95));
            rquad_lines(dp, vec2(9.0, 9.0), -self.time * 2.0, 1.5, alpha(pal.accent, 0.6));
        }
        if p.shield {
            arc(p.pos, 21.0, 0.0, TAU, 2.5, alpha(pal.primary, 0.8));
        }
        // dash cooldown
        if p.dash_cd > 0.0 {
            let frac = 1.0 - (p.dash_cd / p.dash_cd_max.max(0.01)).clamp(0.0, 1.0);
            arc(p.pos, 17.0, -PI * 0.5, -PI * 0.5 + TAU * frac, 1.5, alpha(WHITE, 0.5));
        } else {
            arc(p.pos, 17.0, 0.0, TAU, 1.5, alpha(pal.primary, 0.75));
        }
        let r = PLAYER_R * (1.0 + 0.15 * w.beat_pulse);
        draw_poly(p.pos.x, p.pos.y, 18, r, 0.0, alpha(WHITE, blink));
        draw_poly(p.pos.x, p.pos.y, 12, 3.5, 0.0, alpha(pal.bg0, blink));
        // aim tick
        let d = p.aim_dir;
        let n = vec2(-d.y, d.x);
        let tip = p.pos + d * (27.0 + 5.0 * p.muzzle);
        let base = p.pos + d * 18.0;
        draw_triangle(tip, base + n * 5.0, base - n * 5.0, alpha(pal.primary, blink));
    }

    fn draw_particles(&self, w: &World, tscale: f32) {
        let pal = &self.pal;
        let slot = |c: u8| -> Color {
            match c {
                C_PRIMARY => pal.primary,
                C_SECONDARY => pal.secondary,
                C_ACCENT => pal.accent,
                C_DANGER => pal.danger,
                _ => WHITE,
            }
        };
        for p in &w.parts {
            let k = (p.life / p.max).clamp(0.0, 1.0);
            let s = p.size * (0.4 + 0.6 * k);
            rquad(p.pos, vec2(s * 0.5, s * 0.5), p.rot + p.life * 4.0, alpha(slot(p.col), k * 0.9));
        }
        for t in &w.texts {
            let k = (t.life / t.max).clamp(0.0, 1.0);
            let a = (k * 3.0).min(1.0);
            wtext(&t.text, t.pos, t.size, alpha(slot(t.col), a), tscale);
        }
    }

    /// Drift mode: synthwave blocks, ribbons, polygons and line bursts orbiting the player.
    /// As `idle` falls to 0 the blocks fly out to the arena tiles and everything fades.
    fn draw_idle(&self, w: &World, idle: f32) {
        if idle <= 0.002 {
            return;
        }
        let pal = &self.pal;
        let t = self.time;
        let pc = w.player.pos;
        let a = ease((idle * 1.6).min(1.0));
        let e = ease(idle);
        let ib = (-((t * 100.0 / 60.0).fract()) * 5.0).exp();

        // sine ribbons
        for k in 0..3 {
            let col = [pal.secondary, pal.primary, pal.accent][k];
            let y0 = pc.y + (k as f32 - 1.0) * 150.0;
            let amp = 46.0 * (1.0 + 0.3 * (t * 0.7 + k as f32).sin());
            let mut prev: Option<Vec2> = None;
            let segs = 72;
            for s in 0..=segs {
                let f = s as f32 / segs as f32;
                let x = pc.x - 900.0 + 1800.0 * f;
                let y = y0 + (x * 0.011 + t * (1.4 + k as f32 * 0.45) + k as f32 * 2.0).sin() * amp
                    + (x * 0.023 - t * 0.9).sin() * amp * 0.35;
                let p = vec2(x, y);
                if let Some(q) = prev {
                    let edge = (f * (1.0 - f) * 4.0).min(1.0);
                    draw_line(q.x, q.y, p.x, p.y, 3.0, alpha(col, 0.55 * a * edge));
                }
                prev = Some(p);
            }
        }

        // expanding concentric polygons
        for k in 0..5 {
            let rr = ((t * 70.0 + k as f32 * 100.0) % 500.0) + 50.0;
            let fade = (1.0 - rr / 550.0).max(0.0);
            let sides = [3usize, 4, 6, 4, 3][k];
            poly_lines(pc, sides, rr, t * 0.3 * if k % 2 == 0 { 1.0 } else { -1.0 }, 2.5, alpha(mixc(pal.primary, pal.secondary, k as f32 / 4.0), 0.5 * fade * a));
        }

        // rotating radial dashes
        for k in 0..16 {
            let ang = t * 0.35 + k as f32 * TAU / 16.0;
            let d = vec2(ang.cos(), ang.sin());
            let r0 = 360.0 + 24.0 * (t * 1.3 + k as f32).sin();
            let (p0, p1) = (pc + d * r0, pc + d * (r0 + 46.0 + 20.0 * ib));
            draw_line(p0.x, p0.y, p1.x, p1.y, 3.0, alpha(pal.accent, 0.5 * a));
        }

        // orbiting blocks -> arena tiles
        let rings = [(8usize, 120.0f32, 0.9f32), (10, 205.0, -0.6), (12, 295.0, 0.42)];
        let mut n = 0usize;
        for (ri, &(count, radius, speed)) in rings.iter().enumerate() {
            for k in 0..count {
                let ang = t * speed + k as f32 * TAU / count as f32;
                let bob = 1.0 + 0.07 * (t * 2.0 + k as f32).sin() + 0.06 * ib;
                let orbit = pc + vec2(ang.cos(), ang.sin()) * radius * bob;
                // each block has a home tile it flies to when the level fades in
                let ti = (n * 37 + 11) % (COLS * ROWS);
                let home = tile_center(ti % COLS, ti / COLS);
                let pos = home + (orbit - home) * e;
                let bs = 13.0 + 5.0 * ((n % 3) as f32);
                let size = (TILE * 0.5 - 4.0) + (bs - (TILE * 0.5 - 4.0)) * e;
                let rot = (t * (1.2 + ri as f32 * 0.5) + k as f32) * e;
                let col = [pal.primary, pal.secondary, pal.accent][(k + ri) % 3];
                rquad(pos, vec2(size, size), rot, alpha(col, 0.16 * a));
                rquad_lines(pos, vec2(size, size), rot, 2.5, alpha(col, 0.9 * a));
                n += 1;
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // HUD (screen space)
    // -----------------------------------------------------------------------------------------

    pub fn draw_hud(&self, w: &World, info: &HudInfo, idle: f32) {
        let pal = &self.pal;
        let (sw, sh) = (screen_width(), screen_height());
        let u = sh / 1080.0;
        let m = 34.0 * u;
        let hud_a = if info.show_combat { 1.0 } else { 0.0 };

        if info.show_combat {
            // Score + multiplier
            text_shadow(&format!("{:08}", w.score), m, m + 34.0 * u, 52.0 * u, WHITE, -1);
            let mult = w.mult();
            let mc = if mult >= 4.0 { pal.accent } else if mult >= 2.0 { pal.primary } else { alpha(WHITE, 0.7) };
            text_shadow(&format!("x{:.1}", mult), m, m + 70.0 * u, 34.0 * u, mc, -1);
            let cf = (w.combo % 8.0) / 8.0;
            draw_rectangle(m + 92.0 * u, m + 56.0 * u, 150.0 * u, 6.0 * u, alpha(WHITE, 0.15));
            draw_rectangle(m + 92.0 * u, m + 56.0 * u, 150.0 * u * cf, 6.0 * u, mc);

            // HP pips
            let y = sh - m - 26.0 * u;
            for i in 0..w.player.max_hp {
                let x = m + i as f32 * 34.0 * u;
                let full = i < w.player.hp;
                if full {
                    draw_rectangle(x, y, 24.0 * u, 24.0 * u, pal.primary);
                    draw_rectangle(x + 4.0 * u, y + 4.0 * u, 9.0 * u, 9.0 * u, alpha(WHITE, 0.8));
                } else {
                    draw_rectangle_lines(x, y, 24.0 * u, 24.0 * u, 3.0 * u, alpha(pal.primary, 0.45));
                }
            }
            if w.player.shield {
                let x = m + w.player.max_hp as f32 * 34.0 * u + 6.0 * u;
                draw_rectangle_lines(x, y, 24.0 * u, 24.0 * u, 4.0 * u, pal.accent);
            }
            // XP bar
            let xf = (w.player.xp as f32 / w.player.xp_next.max(1) as f32).clamp(0.0, 1.0);
            draw_rectangle(m, y - 16.0 * u, 260.0 * u, 6.0 * u, alpha(WHITE, 0.15));
            draw_rectangle(m, y - 16.0 * u, 260.0 * u * xf, 6.0 * u, pal.accent);
            text_shadow(&format!("LV {}", w.player.level + 1), m + 270.0 * u, y - 8.0 * u, 20.0 * u, alpha(pal.accent, 0.9), -1);

            // Upgrades
            let mut uy = sh - m;
            for up in UpgradeId::ALL.iter().rev() {
                let lv = w.player.up(*up);
                if lv > 0 {
                    text_shadow(&format!("{} {}", up.name(), roman(lv)), sw - m, uy, 20.0 * u, alpha(pal.accent, 0.85), 1);
                    uy -= 22.0 * u;
                }
            }

            // Heart integrity
            let bw = 520.0 * u;
            let bx = sw * 0.5 - bw * 0.5;
            let by = m + 44.0 * u;
            let frac = (w.heart.hp / w.heart.max_hp.max(1.0)).clamp(0.0, 1.0);
            draw_rectangle(bx, by, bw, 8.0 * u, alpha(WHITE, 0.14));
            if w.heart.broken {
                text_shadow("HEART BROKEN", sw * 0.5, by + 30.0 * u, 20.0 * u, pal.accent, 0);
            } else {
                draw_rectangle(bx, by, bw * frac, 8.0 * u, mixc(pal.danger, WHITE, w.heart.flash));
                text_shadow("HEART", bx - 12.0 * u, by + 9.0 * u, 18.0 * u, alpha(pal.danger, 0.9), 1);
            }

            // Section label
            let label = match w.section_kind {
                SectionKind::Calm => "CALM",
                SectionKind::Normal => "GROOVE",
                SectionKind::Intense => "SURGE",
                SectionKind::Drop => "DROP",
            };
            text_shadow(label, sw - m, m + 24.0 * u, 28.0 * u, pal.secondary, 1);
            if info.bpm > 1.0 {
                text_shadow(&format!("{:.0} BPM", info.bpm), sw - m, m + 50.0 * u, 20.0 * u, alpha(WHITE, 0.6), 1);
            }
            if w.section_banner > 0.0 && idle < 0.5 {
                let k = (w.section_banner / 2.4).clamp(0.0, 1.0);
                let a = (k * 4.0).min(1.0) * ((1.0 - k) * 6.0).min(1.0);
                let s = format!("/ /  {}  / /", spaced(label));
                text_shadow(&s, sw * 0.5, sh * 0.2, 44.0 * u, alpha(pal.secondary, a), 0);
            }
        }

        // Title + progress (shown in drift mode too)
        if !info.title.is_empty() {
            let ta = 0.55 + 0.45 * hud_a;
            text_shadow(&info.title, sw * 0.5, m + 16.0 * u, 24.0 * u, alpha(WHITE, 0.85 * ta), 0);
            if info.progress > 0.0 || info.show_combat {
                let bw = 520.0 * u;
                let bx = sw * 0.5 - bw * 0.5;
                draw_rectangle(bx, m + 26.0 * u, bw, 3.0 * u, alpha(WHITE, 0.14));
                draw_rectangle(bx, m + 26.0 * u, bw * info.progress.clamp(0.0, 1.0), 3.0 * u, pal.accent);
                if info.time_left > 0.0 {
                    let tl = info.time_left.max(0.0) as u32;
                    text_shadow(&format!("{}:{:02}", tl / 60, tl % 60), bx + bw + 12.0 * u, m + 32.0 * u, 20.0 * u, alpha(WHITE, 0.7), -1);
                }
                if !info.track_no.is_empty() {
                    text_shadow(&info.track_no, bx - 12.0 * u, m + 32.0 * u, 20.0 * u, alpha(WHITE, 0.7), 1);
                }
            }
        }
        if !info.banner.is_empty() && info.banner_a > 0.0 {
            let s = spaced(&info.banner);
            let bw = text_w(&s, 64.0 * u) + 80.0 * u;
            draw_rectangle(sw * 0.5 - bw * 0.5, sh * 0.13, bw, 84.0 * u, Color::new(0.0, 0.0, 0.06, 0.55 * info.banner_a));
            text_shadow(&s, sw * 0.5, sh * 0.13 + 60.0 * u, 64.0 * u, alpha(pal.accent, info.banner_a), 0);
        }
        if !info.mode.is_empty() {
            text_shadow(&info.mode, sw * 0.5, sh - m - 30.0 * u, 20.0 * u, alpha(pal.primary, 0.85), 0);
        }
        if !info.status.is_empty() {
            let pulse = 0.65 + 0.35 * (self.time * 3.0).sin();
            text_shadow(&info.status, sw * 0.5, sh - m, 24.0 * u, alpha(WHITE, pulse), 0);
        }
    }
}

fn roman(n: u8) -> &'static str {
    match n {
        1 => "I",
        2 => "II",
        3 => "III",
        4 => "IV",
        5 => "V",
        6 => "VI",
        _ => "",
    }
}

/// Centered text in world space, rasterised at device resolution.
fn wtext(s: &str, pos: Vec2, size: f32, color: Color, tscale: f32) {
    let fs = (size * tscale).clamp(6.0, 200.0);
    let font_size = fs.round().max(1.0) as u16;
    let font_scale = size / font_size as f32;
    let dim = measure_text(s, None, font_size, font_scale);
    draw_text_ex(
        s,
        pos.x - dim.width * 0.5,
        pos.y,
        TextParams { font_size, font_scale, color, ..Default::default() },
    );
}
