// One instanced quad pipeline for backgrounds, glyphs, decorations, the cursor and the UI.
// The quad corner comes from vertex_index; per-instance data is a 20-byte vertex buffer.

struct Uniforms {
    origin: vec2f,
    screen: vec2f,
}

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(1) @binding(0) var mask_tex: texture_2d<f32>;
@group(1) @binding(1) var color_tex: texture_2d<f32>;

const CORNERS = array<vec2f, 6>(
    vec2f(0.0, 0.0),
    vec2f(1.0, 0.0),
    vec2f(0.0, 1.0),
    vec2f(0.0, 1.0),
    vec2f(1.0, 0.0),
    vec2f(1.0, 1.0),
);

struct Out {
    @builtin(position) pos: vec4f,
    @location(0) tex: vec2f,
    @location(1) color: vec4f,
    @location(2) @interpolate(flat) flags: u32,
}

@vertex
fn vs(
    @builtin(vertex_index) vi: u32,
    @location(0) xy: vec2<i32>,
    @location(1) uv: vec4<u32>,
    @location(2) color: vec4f,
    @location(3) flags: u32,
) -> Out {
    let corner = CORNERS[vi];
    let size = vec2f(f32(uv.z), f32(uv.w));
    let pos = u.origin + vec2f(xy) + corner * size;

    var o: Out;
    o.pos = vec4f(
        pos.x / u.screen.x * 2.0 - 1.0,
        1.0 - pos.y / u.screen.y * 2.0,
        0.0,
        1.0,
    );
    o.tex = vec2f(f32(uv.x), f32(uv.y)) + corner * size;
    o.color = color;
    o.flags = flags;
    return o;
}

@fragment
fn fs(i: Out) -> @location(0) vec4f {
    let c = i.color;
    let premul = vec4f(c.rgb * c.a, c.a);
    if i.flags == 2u {
        return premul;
    }
    let coord = vec2<i32>(floor(i.tex));
    if i.flags == 1u {
        return textureLoad(color_tex, coord, 0);
    }
    let coverage = textureLoad(mask_tex, coord, 0).r;
    return premul * coverage;
}
