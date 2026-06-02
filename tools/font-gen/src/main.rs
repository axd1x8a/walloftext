use std::io::Write as _;

use anyhow::{Context, Result};
use clap::Parser;
use walloftext_shared::FontAtlasFile;

#[derive(Parser)]
#[command(about = "Generate a bitcode+zstd bitmap atlas from a TTF/OTF font")]
struct Args {
    #[arg(short, long)]
    font: String,

    #[arg(long, default_value_t = 16.0)]
    font_size: f32,

    #[arg(long, default_value_t = 19)]
    level: i32,

    #[arg(short, long, default_value = "static/unifont.wtfont")]
    output: String,

    #[arg(long)]
    dump_png: Option<String>,

    #[arg(long, default_value_t = 4)]
    png_scale: u32,
}

struct ShelfPacker {
    atlas_w: u32,
    x: u32,
    y: u32,
    shelf_h: u32,
}

impl ShelfPacker {
    fn new(atlas_w: u32) -> Self {
        Self {
            atlas_w,
            x: 0,
            y: 0,
            shelf_h: 0,
        }
    }

    fn pack(&mut self, w: u32, h: u32) -> (u32, u32) {
        if self.x + w > self.atlas_w {
            self.y += self.shelf_h;
            self.x = 0;
            self.shelf_h = 0;
        }
        let px = self.x;
        let py = self.y;
        self.x += w;
        self.shelf_h = self.shelf_h.max(h);
        (px, py)
    }

    fn total_height(&self) -> u32 {
        self.y + self.shelf_h
    }
}

struct RawGlyph {
    ch: char,
    bitmap: Vec<u8>,
    w: u32,
    h: u32,
    cell_off_x: i16,
    cell_off_y: i16,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let font_data =
        std::fs::read(&args.font).with_context(|| format!("cannot read font: {}", args.font))?;
    let font = fontdue::Font::from_bytes(font_data.as_slice(), fontdue::FontSettings::default())
        .map_err(|e| anyhow::anyhow!("failed to load font: {}", e))?;

    let line_m = font
        .horizontal_line_metrics(args.font_size)
        .ok_or_else(|| anyhow::anyhow!("font has no horizontal line metrics"))?;
    let cell_h = (line_m.ascent - line_m.descent).round() as u32;
    let baseline = line_m.ascent.round() as i32;
    let cell_w = font.metrics(' ', args.font_size).advance_width.round() as u32;

    let mut raw: Vec<RawGlyph> = Vec::with_capacity(font.chars().len());
    for (&ch, _) in font.chars() {
        let (m, bitmap) = font.rasterize(ch, args.font_size);
        if m.width == 0 || m.height == 0 {
            continue;
        }
        let w = m.width as u32;
        let h = m.height as u32;

        let cell_off_x = ((cell_w as i32 - w as i32) / 2) as i16;

        let raw_top = baseline - (m.ymin + m.height as i32);
        let cell_off_y = raw_top.clamp(0, (cell_h as i32 - h as i32).max(0)) as i16;

        raw.push(RawGlyph {
            ch,
            bitmap,
            w,
            h,
            cell_off_x,
            cell_off_y,
        });
    }

    eprintln!(
        "cell={}x{}px  glyphs={}  font_size={}px  level={}",
        cell_w,
        cell_h,
        raw.len(),
        args.font_size,
        args.level
    );

    raw.sort_unstable_by(|a, b| b.h.cmp(&a.h).then(b.w.cmp(&a.w)));

    let total_area: u64 = raw.iter().map(|g| g.w as u64 * g.h as u64).sum();
    let target_side = ((total_area as f64 / 0.75).sqrt()) as u32;
    let atlas_w = target_side.max(256).next_power_of_two().min(8192);

    let mut packer = ShelfPacker::new(atlas_w);
    let positions: Vec<(u32, u32)> = raw.iter().map(|g| packer.pack(g.w, g.h)).collect();
    let atlas_h = packer.total_height().max(1).next_power_of_two();

    eprintln!("atlas={}x{}  glyphs={}", atlas_w, atlas_h, raw.len());

    let packed_stride = atlas_w as usize / 8;
    let mut pixels_1bit = vec![0u8; packed_stride * atlas_h as usize];

    let mut pixels_gray: Option<Vec<u8>> = args
        .dump_png
        .is_some()
        .then(|| vec![0u8; atlas_w as usize * atlas_h as usize]);

    let n = raw.len();
    let mut entries: Vec<(u32, u16, u16, u8, u8, i8, i8)> = Vec::with_capacity(n);

    for (g, &(ax, ay)) in raw.iter().zip(positions.iter()) {
        let ax = ax as usize;
        let ay = ay as usize;
        let gw = g.w as usize;

        for row in 0..g.h as usize {
            let src = &g.bitmap[row * gw..][..gw];
            let dst_y = ay + row;

            for (col, &px) in src.iter().enumerate() {
                if px > 127 {
                    let x = ax + col;
                    pixels_1bit[dst_y * packed_stride + x / 8] |= 1 << (x & 7);
                }
            }

            if let Some(ref mut gray) = pixels_gray {
                let dst = dst_y * atlas_w as usize + ax;
                gray[dst..dst + gw].copy_from_slice(src);
            }
        }

        entries.push((
            g.ch as u32,
            ax as u16,
            ay as u16,
            g.w as u8,
            g.h as u8,
            g.cell_off_x as i8,
            g.cell_off_y as i8,
        ));
    }

    entries.sort_unstable_by_key(|e| e.0);

    let mut codepoints = Vec::with_capacity(n);
    let mut atlas_x_vec = Vec::with_capacity(n);
    let mut atlas_y_vec = Vec::with_capacity(n);
    let mut w_vec = Vec::with_capacity(n);
    let mut h_vec = Vec::with_capacity(n);
    let mut off_x_vec = Vec::with_capacity(n);
    let mut off_y_vec = Vec::with_capacity(n);

    for (cp, ax, ay, gw, gh, ox, oy) in entries {
        codepoints.push(cp);
        atlas_x_vec.push(ax);
        atlas_y_vec.push(ay);
        w_vec.push(gw);
        h_vec.push(gh);
        off_x_vec.push(ox);
        off_y_vec.push(oy);
    }

    let atlas = FontAtlasFile {
        atlas_w: atlas_w as u16,
        atlas_h: atlas_h as u16,
        cell_w: cell_w as u8,
        cell_h: cell_h as u8,
        codepoints,
        atlas_x: atlas_x_vec,
        atlas_y: atlas_y_vec,
        w: w_vec,
        h: h_vec,
        cell_off_x: off_x_vec,
        cell_off_y: off_y_vec,
        pixels_1bit,
    };

    let encoded = bitcode::encode(&atlas);
    let compressed = zstd::encode_all(encoded.as_slice(), args.level)?;

    if let Some(p) = std::path::Path::new(&args.output).parent()
        && !p.as_os_str().is_empty()
    {
        std::fs::create_dir_all(p)?;
    }
    std::fs::File::create(&args.output)?.write_all(&compressed)?;

    eprintln!(
        "written {} | {}B encoded -> {}B compressed ({:.1}x)",
        args.output,
        encoded.len(),
        compressed.len(),
        encoded.len() as f64 / compressed.len() as f64,
    );

    if let Some(png_path) = &args.dump_png {
        let gray = pixels_gray.unwrap();
        write_png(png_path, &gray, atlas_w, atlas_h, args.png_scale)?;
        eprintln!(
            "dumped atlas bitmap -> {}  ({}x{}px at {}x scale)",
            png_path,
            atlas_w * args.png_scale,
            atlas_h * args.png_scale,
            args.png_scale,
        );
    }

    Ok(())
}

fn write_png(path: &str, pixels: &[u8], w: u32, h: u32, scale: u32) -> Result<()> {
    use std::io::BufWriter;
    let s = scale.max(1) as usize;
    let out_w = w as usize * s;
    let out_h = h as usize * s;
    let mut scaled = vec![0u8; out_w * out_h];
    for sy in 0..h as usize {
        for sx in 0..w as usize {
            let v = pixels[sy * w as usize + sx];
            for dy in 0..s {
                scaled[(sy * s + dy) * out_w + sx * s..][..s].fill(v);
            }
        }
    }
    let file = std::fs::File::create(path).with_context(|| format!("cannot create PNG: {path}"))?;
    let mut enc = png::Encoder::new(BufWriter::new(file), out_w as u32, out_h as u32);
    enc.set_color(png::ColorType::Grayscale);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&scaled)?;
    Ok(())
}
