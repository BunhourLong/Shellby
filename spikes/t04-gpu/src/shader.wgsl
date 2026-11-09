// T04 spike: bg pass + glyph pass. One instance per cell, quad expanded from vertex_index
// (triangle strip, 4 vertices). Instance data comes in through instance-step vertex buffers.

struct Uniforms {
    cell: vec2f,     // cell size, px
    origin: vec2f,   // grid origin, px
    atlas: vec2f,    // atlas size, texels (unused: textureLoad takes texel coords)
    viewport: vec2f, // render target size, px
}

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;

fn corner(vi: u32) -> vec2f {
    return vec2f(f32(vi & 1u), f32(vi >> 1u));
}

fn to_clip(p: vec2f) -> vec4f {
    return vec4f(p.x / u.viewport.x * 2.0 - 1.0, 1.0 - p.y / u.viewport.y * 2.0, 0.0, 1.0);
}

struct BgOut {
    @builtin(position) pos: vec4f,
    @location(0) color: vec4f,
}

@vertex
fn bg_vs(@builtin(vertex_index) vi: u32, @location(0) col_row: vec2<u32>, @location(1) color: vec4f) -> BgOut {
    var o: BgOut;
    o.pos = to_clip(u.origin + (vec2f(col_row) + corner(vi)) * u.cell);
    o.color = color;
    return o;
}

@fragment
fn bg_fs(i: BgOut) -> @location(0) vec4f {
    return i.color;
}

struct GlyphOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) fg: vec4f,
}

@vertex
fn glyph_vs(
    @builtin(vertex_index) vi: u32,
    @location(0) col_row: vec2<u32>,
    @location(1) uv_min: vec2<u32>,
    @location(2) uv_size: vec2<u32>,
    @location(3) offset: vec2<i32>,
    @location(4) fg: vec4f,
    @location(5) flags: u32,
) -> GlyphOut {
    let c = corner(vi);
    let size = vec2f(uv_size);
    var o: GlyphOut;
    o.pos = to_clip(u.origin + vec2f(col_row) * u.cell + vec2f(offset) + c * size);
    o.uv = vec2f(uv_min) + c * size;
    o.fg = fg;
    return o;
}

@fragment
fn glyph_fs(i: GlyphOut) -> @location(0) vec4f {
    let a = textureLoad(atlas_tex, vec2<i32>(floor(i.uv)), 0).r;
    return vec4f(i.fg.rgb, i.fg.a * a);
}
