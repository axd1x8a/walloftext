struct Uniforms {
    cw: f32,
    ch: f32,
    vx: f32,
    vy: f32,
    dpr: f32,
    step: f32,
    canvas_w: f32,
    canvas_h: f32,
    alpha: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
}
@group(0) @binding(0) var<uniform> u: Uniforms;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0)       color: vec3<f32>,
}

@vertex
fn vs_main(
    @location(0) corner: vec2<f32>,
    @location(1) grid_pos: vec2<f32>,
    @location(2) color: vec3<f32>,
) -> VsOut {
    let world_pos = grid_pos * vec2<f32>(u.cw, u.ch) - vec2<f32>(u.vx, u.vy);
    let draw_size = vec2<f32>(u.cw * u.step, u.ch * u.step);

    let dev = (world_pos + corner * draw_size) * u.dpr;
    let ndc = dev / vec2<f32>(u.canvas_w, u.canvas_h) * 2.0 - 1.0;

    var out: VsOut;
    out.pos = vec4<f32>(ndc.x, -ndc.y, 0.0, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, u.alpha);
}
