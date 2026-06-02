use std::collections::HashMap;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use walloftext_shared::{CHUNK_H, CHUNK_W, ChunkCoords, FontAtlasFile, Rgb};

use crate::state::AppState;

const OUT_W: u32 = 1200;
const OUT_H: u32 = 630;
const DPR: f64 = 1.5;
const BG: [u8; 4] = [0x0C, 0x0C, 0x0C, 0xff];
const LOD_GLYPH_THRESHOLD: f64 = 5.0;

fn parse_at(s: &str) -> Option<(i32, i32, u32)> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() < 2 {
        return None;
    }
    let x = parts[0].parse::<i32>().ok()?;
    let y = parts[1].parse::<i32>().ok()?;
    let z = parts
        .get(2)
        .map(|s| s.trim_end_matches('%'))
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(100)
        .clamp(3, 800);
    Some((x, y, z))
}

#[inline]
fn atlas_bit(atlas: &FontAtlasFile, ax: i32, ay: i32) -> bool {
    if ax < 0 || ay < 0 || ax >= atlas.atlas_w as i32 || ay >= atlas.atlas_h as i32 {
        return false;
    }
    let packed_stride = atlas.atlas_w as usize / 8;
    let idx = ay as usize * packed_stride + (ax as usize / 8);
    ((atlas.pixels_1bit[idx] >> (ax as usize % 8)) & 1) != 0
}

pub fn build_coverage_map(
    atlas: &FontAtlasFile,
    glyph_idx: usize,
    cell_px_w: i32,
    cell_px_h: i32,
) -> Vec<u8> {
    let cw = atlas.cell_w as f32;
    let ch = atlas.cell_h as f32;
    let ax0 = atlas.atlas_x[glyph_idx] as i32;
    let ay0 = atlas.atlas_y[glyph_idx] as i32;
    let gw = atlas.w[glyph_idx] as i32;
    let gh = atlas.h[glyph_idx] as i32;
    let off_x = atlas.cell_off_x[glyph_idx] as i32;
    let off_y = atlas.cell_off_y[glyph_idx] as i32;

    let sx = cw / cell_px_w as f32;
    let sy = ch / cell_px_h as f32;

    let x_weights: Vec<Vec<(i32, f32)>> = (0..cell_px_w)
        .map(|dx| {
            let lo = dx as f32 * sx;
            let hi = (dx + 1) as f32 * sx;
            (lo.floor() as i32..hi.ceil() as i32)
                .filter_map(|a| {
                    let gx = a - off_x;
                    if gx < 0 || gx >= gw {
                        return None;
                    }
                    let w = (hi.min((a + 1) as f32) - lo.max(a as f32)) / sx;
                    if w > 0.0 { Some((gx, w)) } else { None }
                })
                .collect()
        })
        .collect();

    let y_weights: Vec<Vec<(i32, f32)>> = (0..cell_px_h)
        .map(|dy| {
            let lo = dy as f32 * sy;
            let hi = (dy + 1) as f32 * sy;
            (lo.floor() as i32..hi.ceil() as i32)
                .filter_map(|a| {
                    let gy = a - off_y;
                    if gy < 0 || gy >= gh {
                        return None;
                    }
                    let w = (hi.min((a + 1) as f32) - lo.max(a as f32)) / sy;
                    if w > 0.0 { Some((gy, w)) } else { None }
                })
                .collect()
        })
        .collect();

    let mut map = vec![0u8; (cell_px_w * cell_px_h) as usize];
    for (dy, yw) in y_weights.iter().enumerate() {
        for (dx, xw) in x_weights.iter().enumerate() {
            let mut cov = 0.0f32;
            for &(gy, wy) in yw {
                for &(gx, wx) in xw {
                    if atlas_bit(atlas, ax0 + gx, ay0 + gy) {
                        cov += wx * wy;
                    }
                }
            }
            map[dy * cell_px_w as usize + dx] = (cov * 255.0 + 0.5) as u8;
        }
    }
    map
}

#[inline(always)]
fn div255(x: u32) -> u8 {
    ((x + (x >> 8) + 1) >> 8) as u8
}

fn make_blend_table(r: u8, g: u8, b: u8) -> Box<[[u8; 3]; 256]> {
    let mut t = Box::new([[0u8; 3]; 256]);
    for cov in 0u32..=255 {
        let inv = 255 - cov;
        t[cov as usize] = [
            div255(r as u32 * cov + BG[0] as u32 * inv),
            div255(g as u32 * cov + BG[1] as u32 * inv),
            div255(b as u32 * cov + BG[2] as u32 * inv),
        ];
    }
    t
}

pub fn render_preview(
    atlas: &FontAtlasFile,
    cells: &HashMap<(i32, i32), (char, Rgb)>,
    cx: i32,
    cy: i32,
    zoom: f64,
) -> Vec<u8> {
    let total = OUT_W as usize * OUT_H as usize * 4;
    let mut buf = vec![0u8; total];
    for px in buf.as_chunks_mut::<4>().0 {
        px.copy_from_slice(&BG);
    }

    let cw = 8.0 * zoom;
    let ch = 16.0 * zoom;
    let cw_dev = cw * DPR;
    let ch_dev = ch * DPR;

    let vx = (cx as f64 * cw_dev + cw_dev / 2.0) - OUT_W as f64 / 2.0;
    let vy = (cy as f64 * ch_dev + ch_dev / 2.0) - OUT_H as f64 / 2.0;

    let glyph_mode = cw_dev >= LOD_GLYPH_THRESHOLD;

    let lod_step = if glyph_mode {
        1i32
    } else {
        ((1.0 / cw_dev).max(1.0) as u32).next_power_of_two() as i32
    };

    struct DrawCmd {
        py: i32,
        px: i32,
        next_py: i32,
        next_px: i32,
        ch: char,
        r: u8,
        g: u8,
        b: u8,
    }

    let mut draws: Vec<DrawCmd> = Vec::new();

    for (&(wx, wy), &(ch_c, Rgb(r, g, b))) in cells {
        if lod_step > 1 && (wx.rem_euclid(lod_step) != 0 || wy.rem_euclid(lod_step) != 0) {
            continue;
        }

        let px = (wx as f64 * cw_dev - vx).round() as i32;
        let py = (wy as f64 * ch_dev - vy).round() as i32;
        let next_px = ((wx + lod_step) as f64 * cw_dev - vx).round() as i32;
        let next_py = ((wy + lod_step) as f64 * ch_dev - vy).round() as i32;

        if px >= OUT_W as i32 || next_px <= 0 || py >= OUT_H as i32 || next_py <= 0 {
            continue;
        }
        draws.push(DrawCmd {
            py,
            px,
            next_py,
            next_px,
            ch: ch_c,
            r,
            g,
            b,
        });
    }

    if glyph_mode {
        draws.sort_unstable_by_key(|d| d.py);
    }

    let mut cov_cache: HashMap<(char, i32, i32), Vec<u8>> = HashMap::new();
    #[allow(clippy::type_complexity)]
    let mut blend_cache: HashMap<(u8, u8, u8), Box<[[u8; 3]; 256]>> = HashMap::new();

    for cmd in &draws {
        let DrawCmd {
            py,
            px,
            next_py,
            next_px,
            ch: ch_c,
            r,
            g,
            b,
        } = *cmd;

        if glyph_mode {
            let Ok(glyph_idx) = atlas.codepoints.binary_search(&(ch_c as u32)) else {
                continue;
            };

            let cell_px_w = (next_px - px).max(1);
            let cell_px_h = (next_py - py).max(1);

            let cov_map = cov_cache
                .entry((ch_c, cell_px_w, cell_px_h))
                .or_insert_with(|| build_coverage_map(atlas, glyph_idx, cell_px_w, cell_px_h));

            let blend = blend_cache
                .entry((r, g, b))
                .or_insert_with(|| make_blend_table(r, g, b));

            let start_dy = (-py).max(0);
            let end_dy = (OUT_H as i32 - py).min(cell_px_h);
            let start_dx = (-px).max(0);
            let end_dx = (OUT_W as i32 - px).min(cell_px_w);

            for dy in start_dy..end_dy {
                let row_base = (py + dy) as usize * OUT_W as usize;
                let map_row = dy as usize * cell_px_w as usize;

                for dx in start_dx..end_dx {
                    let cov_byte = cov_map[map_row + dx as usize];
                    if cov_byte == 0 {
                        continue;
                    }
                    let [br, bg_out, bb] = blend[cov_byte as usize];
                    let i = (row_base + (px + dx) as usize) * 4;
                    buf[i] = br;
                    buf[i + 1] = bg_out;
                    buf[i + 2] = bb;
                    buf[i + 3] = 0xff;
                }
            }
        } else {
            let x0 = px.max(0) as usize;
            let x1 = next_px.min(OUT_W as i32) as usize;
            let y0 = py.max(0) as usize;
            let y1 = next_py.min(OUT_H as i32) as usize;

            for sy in y0..y1 {
                let row = sy * OUT_W as usize;
                for sx in x0..x1 {
                    let i = (row + sx) * 4;
                    buf[i] = r;
                    buf[i + 1] = g;
                    buf[i + 2] = b;
                    buf[i + 3] = 0xff;
                }
            }
        }
    }

    buf
}

fn encode_png(rgba: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    use image::{ImageBuffer, Rgba};
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(OUT_W, OUT_H, rgba).ok_or_else(|| anyhow::anyhow!("bad buffer"))?;
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
    Ok(png)
}

pub async fn og_preview_handler(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let at = match params.get("at") {
        Some(v) => v.clone(),
        None => return (StatusCode::BAD_REQUEST, "missing `at` param").into_response(),
    };

    let (cx, cy, zoom_pct) = parse_at(&at).unwrap_or((0, 0, 100));
    let zoom = (zoom_pct as f64 / 100.0 * 0.5).clamp(0.03, 8.0);

    let cw = 8.0 * zoom;
    let ch = 16.0 * zoom;
    let cw_dev = cw * DPR;
    let ch_dev = ch * DPR;

    let half_cx_cells = (OUT_W as f64 / 2.0 / cw_dev).ceil() as i32 + 1;
    let half_cy_cells = (OUT_H as f64 / 2.0 / ch_dev).ceil() as i32 + 1;

    let x0 = cx - half_cx_cells;
    let y0 = cy - half_cy_cells;
    let x1 = cx + half_cx_cells;
    let y1 = cy + half_cy_cells;

    let chunk_x0 = x0.div_euclid(CHUNK_W as i32) as i8;
    let chunk_x1 = x1.div_euclid(CHUNK_W as i32) as i8;
    let chunk_y0 = y0.div_euclid(CHUNK_H as i32) as i8;
    let chunk_y1 = y1.div_euclid(CHUNK_H as i32) as i8;

    let mut cells: HashMap<(i32, i32), (char, Rgb)> = HashMap::new();
    for cy_chunk in chunk_y0..=chunk_y1 {
        for cx_chunk in chunk_x0..=chunk_x1 {
            let chunk = ChunkCoords {
                x: cx_chunk,
                y: cy_chunk,
            };
            let shard = state.inner.shards[chunk.shard_idx()].read().unwrap();
            for cell in shard.get_chunk(chunk) {
                let wx = cx_chunk as i32 * CHUNK_W as i32 + cell.local.x as i32;
                let wy = cy_chunk as i32 * CHUNK_H as i32 + cell.local.y as i32;
                cells.insert((wx, wy), (cell.ch, cell.color));
            }
        }
    }

    let rgba = render_preview(&state.inner.font_atlas, &cells, cx, cy, zoom);
    match encode_png(rgba) {
        Ok(png) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "public, max-age=30"),
            ],
            png,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn base_url(headers: &HeaderMap) -> String {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("https");
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:3000");
    format!("{proto}://{host}")
}

fn inject_og_tags(html: &str, at: &str, base: &str) -> String {
    let (x, y, _) = parse_at(at).unwrap_or((0, 0, 100));
    let img_url = format!("{base}/og/preview?at={at}");
    let tags = format!(
        r#"<meta property="og:title" content="WallOfText @ ({x}, {y})">
<meta property="og:description" content="Collaborative text canvas">
<meta property="og:image" content="{img_url}">
<meta property="og:image:width" content="1200">
<meta property="og:image:height" content="630">
<meta property="og:type" content="website">
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:image" content="{img_url}">"#
    );
    html.replacen("</head>", &format!("{tags}\n</head>"), 1)
}

pub async fn index_html_handler(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let html = if let Some(at) = params.get("at") {
        let base = base_url(&headers);
        inject_og_tags(&state.inner.index_html, at, &base)
    } else {
        state.inner.index_html.as_ref().clone()
    };
    Html(html).into_response()
}
