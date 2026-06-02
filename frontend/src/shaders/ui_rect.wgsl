struct Uniforms {
    canvas_w: f32,
    canvas_h: f32,
    dpr: f32,
    blink_on: f32,
}
@group(0) @binding(0) var<uniform> u: Uniforms;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local_px: vec2<f32>,
    @location(1) @interpolate(flat) screen_wh: vec2<f32>,
    @location(2) @interpolate(flat) fill_rgba: vec4<f32>,
    @location(3) @interpolate(flat) border_rgba: vec4<f32>,
    @location(4) @interpolate(flat) border_px: f32,
    @location(5) @interpolate(flat) dash_px: f32,
    @location(6) @interpolate(flat) flags: f32,
}

@vertex
fn vs_main(
    @location(0) corner: vec2<f32>,
    @location(1) screen_xy: vec2<f32>,
    @location(2) screen_wh: vec2<f32>,
    @location(3) fill_rgba: vec4<f32>,
    @location(4) border_rgba: vec4<f32>,
    @location(5) border_px: f32,
    @location(6) dash_px: f32,
    @location(7) flags: f32,
) -> VsOut {
    let dev = screen_xy + corner * screen_wh;
    let ndc = dev / vec2<f32>(u.canvas_w, u.canvas_h) * 2.0 - 1.0;

    var out: VsOut;
    out.pos = vec4<f32>(ndc.x, -ndc.y, 0.0, 1.0);
    out.local_px = corner * screen_wh;
    out.screen_wh = screen_wh;
    out.fill_rgba = fill_rgba;
    out.border_rgba = border_rgba;
    out.border_px = border_px;
    out.dash_px = dash_px;
    out.flags = flags;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let px = in.local_px;
    let wh = in.screen_wh;

    let dist2 = min(px, wh - px);
    let dist_edge = min(dist2.x, dist2.y);

    var final_color = vec4<f32>(0.0);

    var fill_alpha = in.fill_rgba.a;
    if in.flags >= 0.5 {
        fill_alpha *= u.blink_on;
    }
    final_color = vec4<f32>(in.fill_rgba.rgb, fill_alpha);

    if in.border_px > 0.0 && in.border_rgba.a > 0.0 && dist_edge < in.border_px {
        var draw_border = true;

        if in.dash_px > 0.0 {
            var perim: f32;

            if dist2.y <= dist2.x {
                if px.y < wh.y * 0.5 {
                    perim = px.x; // Top
                } else {
                    perim = wh.x + wh.y + (wh.x - px.x); // Bottom
                }
            } else {
                if px.x > wh.x * 0.5 {
                    perim = wh.x + px.y; // Right
                } else {
                    perim = 2.0 * wh.x + wh.y + (wh.y - px.y); // Left
                }
            }

            if fract((perim / in.dash_px) * 0.5) >= 0.5 {
                draw_border = false;
            }
        }

        if draw_border {
            final_color = in.border_rgba;
        }
    }

    if final_color.a <= 0.0 {
        discard;
    }

    return final_color;
}
