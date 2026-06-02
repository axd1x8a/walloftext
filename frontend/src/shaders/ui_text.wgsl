struct Uniforms {
    canvas_w: f32,
    canvas_h: f32,
    dpr: f32,
    ui_scale: f32,
    atlas_w: f32,
    atlas_h: f32,
    _pad0: f32,
    _pad1: f32,
}
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) @interpolate(flat) atlas_xy: vec2<f32>,
    @location(2) @interpolate(flat) glyph_wh: vec2<f32>,
    @location(3) local_px: vec2<f32>,
    @location(4) @interpolate(flat) min_uv: vec2<f32>,
    @location(5) @interpolate(flat) max_uv: vec2<f32>,
}

@vertex
fn vs_main(
    @location(0) corner: vec2<f32>,
    @location(1) screen_xy: vec2<f32>,
    @location(2) color: vec3<f32>,
    @location(3) atlas_xy: vec2<f32>,
    @location(4) glyph_wh: vec2<f32>,
    @location(5) cell_off: vec2<f32>,
    @location(6) extra_scale: f32,
) -> VsOut {
    let final_scale = u.ui_scale * extra_scale;
    let dev = screen_xy + (cell_off + corner * glyph_wh) * final_scale;
    let ndc = dev / vec2<f32>(u.canvas_w, u.canvas_h) * 2.0 - 1.0;

    var out: VsOut;
    out.pos = vec4<f32>(ndc.x, -ndc.y, 0.0, 1.0);
    out.color = color;
    out.atlas_xy = atlas_xy;
    out.glyph_wh = glyph_wh;

    out.local_px = corner * glyph_wh;

    let atlas_size = vec2<f32>(u.atlas_w, u.atlas_h);
    out.min_uv = (atlas_xy + vec2<f32>(0.5)) / atlas_size;
    out.max_uv = (atlas_xy + glyph_wh - vec2<f32>(0.5)) / atlas_size;

    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let virtual_px = in.atlas_xy + in.local_px;
    let uv = virtual_px / vec2<f32>(u.atlas_w, u.atlas_h);
    let clamped_uv = clamp(uv, in.min_uv, in.max_uv);

    var alpha: f32;
    if u.ui_scale * u.dpr > 1.0 {
        let ix = clamp(i32(in.atlas_xy.x) + i32(in.local_px.x),
            i32(in.atlas_xy.x), i32(in.atlas_xy.x) + i32(in.glyph_wh.x) - 1);
        let iy = clamp(i32(in.atlas_xy.y) + i32(in.local_px.y),
            i32(in.atlas_xy.y), i32(in.atlas_xy.y) + i32(in.glyph_wh.y) - 1);
        alpha = textureLoad(atlas, vec2<i32>(ix, iy), 0).r;
        if alpha < 0.5 { discard; }
    } else {
        let lod = log2(1.0 / (u.ui_scale * u.dpr));
        let raw = textureSampleLevel(atlas, atlas_sampler, clamped_uv, max(0.0, lod)).r;
        if raw <= 0.05 { discard; }
        alpha = raw;
    }

    return vec4<f32>(in.color, alpha);
}
