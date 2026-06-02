use std::{
    io::Read,
    net::TcpStream,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use image::{AnimationDecoder, Rgb as ImgRgb, buffer::ConvertBuffer};
use rayon::prelude::*;
use tungstenite::{Message, WebSocket, connect, stream::MaybeTlsStream};
use walloftext_shared::{CellWrite, ClientMsg, Rgb, ServerMsg, WorldCoords};

#[derive(Parser)]
#[command(name = "walloftext-video", about = "Play video on a WallOfText board")]
struct Args {
    #[arg(long, default_value = "ws://localhost:3000/ws")]
    url: String,

    #[arg(long, short = 'x', default_value_t = 0)]
    x: i16,

    #[arg(long, short = 'y', default_value_t = 0)]
    y: i16,

    #[arg(long)]
    width: u32,

    #[arg(long)]
    height: u32,

    #[arg(long, default_value_t = 10.0)]
    fps: f64,

    #[arg(long)]
    username: Option<String>,

    #[arg(long)]
    password: Option<String>,

    file: Option<PathBuf>,
}

fn send_msg(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>, msg: &ClientMsg) -> Result<()> {
    let bytes = bitcode::encode(msg).into();
    ws.send(Message::Binary(bytes))?;
    Ok(())
}

fn recv_msg(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>) -> Result<ServerMsg> {
    loop {
        let msg = ws.read()?;
        if let Message::Binary(bytes) = msg {
            let decompressed = zstd::decode_all::<&[u8]>(bytes.as_ref())
                .context("failed to zstd-decompress server message")?;
            let server_msg: ServerMsg = bitcode::decode(&decompressed)
                .context("failed to bitcode-decode server message")?;
            return Ok(server_msg);
        }
    }
}

fn connect_and_auth(
    url: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    eprintln!("Connecting to {url} ...");
    let (mut ws, _) = connect(url).context("WebSocket connect failed")?;

    loop {
        if let ServerMsg::Welcome { anon_name, .. } = recv_msg(&mut ws)? {
            eprintln!("Connected (anonymous name: {anon_name})");
            break;
        }
    }

    if let (Some(u), Some(p)) = (username, password) {
        eprintln!("Logging in as {u} ...");
        send_msg(
            &mut ws,
            &ClientMsg::Login {
                username: u.to_owned(),
                password: p.to_owned(),
            },
        )?;
        loop {
            match recv_msg(&mut ws)? {
                ServerMsg::AuthOk { username, .. } => {
                    eprintln!("Logged in as {username}");
                    break;
                }
                ServerMsg::AuthFail { reason } => bail!("Login failed: {reason}"),
                _ => {}
            }
        }
    }

    Ok(ws)
}

fn pixel_to_cell(r: u8, g: u8, b: u8) -> Option<char> {
    let y = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    match y as u32 {
        0..=19 => None,
        20..=89 => Some('░'),
        90..=139 => Some('▒'),
        140..=199 => Some('▓'),
        _ => Some('█'),
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn new() -> Self {
        Self {
            tokens: 200.0,
            last: Instant::now(),
        }
    }

    fn take(&mut self, n: usize) {
        const CAP: f64 = 200.0;
        const REFILL: f64 = 40.0;

        let n = n as f64;
        loop {
            let now = Instant::now();
            let elapsed = now.duration_since(self.last).as_secs_f64();
            self.tokens = (self.tokens + elapsed * REFILL).min(CAP);
            self.last = now;

            if self.tokens >= n {
                self.tokens -= n;
                return;
            }

            let wait = (n - self.tokens) / REFILL;
            thread::sleep(Duration::from_secs_f64(wait));
        }
    }
}

struct Session {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    bucket: Option<Bucket>,
    last_ping: Instant,
}

impl Session {
    fn new(ws: WebSocket<MaybeTlsStream<TcpStream>>, bucket: Option<Bucket>) -> Self {
        Self {
            ws,
            bucket,
            last_ping: Instant::now(),
        }
    }

    fn send_batch(&mut self, batch: Vec<CellWrite>) -> Result<()> {
        if self.last_ping.elapsed() > Duration::from_secs(5) {
            self.ws.send(Message::Ping(vec![].into()))?;
            self.last_ping = Instant::now();
        }
        if let Some(b) = self.bucket.as_mut() {
            b.take(1);
        }
        let bytes = bitcode::encode(&ClientMsg::SetCellBatch {
            cells: batch,
            commit: false,
        })
        .into();
        self.ws.send(Message::Binary(bytes))?;
        self.drain()
    }

    fn drain(&mut self) -> Result<()> {
        ws_set_nonblocking(&self.ws, true);
        loop {
            match self.ws.read() {
                Ok(Message::Close(_)) => anyhow::bail!("server closed connection"),
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    break;
                }
                Err(e) => {
                    ws_set_nonblocking(&self.ws, false);
                    return Err(e.into());
                }
            }
        }
        ws_set_nonblocking(&self.ws, false);
        Ok(())
    }
}

fn ws_set_nonblocking(ws: &WebSocket<MaybeTlsStream<TcpStream>>, v: bool) {
    match ws.get_ref() {
        MaybeTlsStream::Plain(tcp) => {
            let _ = tcp.set_nonblocking(v);
        }
        MaybeTlsStream::NativeTls(tls) => {
            let _ = tls.get_ref().set_nonblocking(v);
        }
        _ => {}
    }
}

fn clear_area(session: &mut Session, x0: i16, y0: i16, width: u32, height: u32) -> Result<()> {
    eprintln!("Clearing {}x{} render area ...", width, height);
    let cells: Vec<CellWrite> = (0..height as i16)
        .flat_map(|iy| {
            (0..width as i16).map(move |ix| CellWrite {
                pos: WorldCoords {
                    x: x0.saturating_add(ix),
                    y: y0.saturating_add(iy),
                },
                ch: None,
                color: Rgb(0, 0, 0),
            })
        })
        .collect();

    let total = cells.len();
    for (n, batch) in cells.chunks(100).enumerate() {
        session.send_batch(batch.to_vec())?;
        eprint!("\r  cleared {}/{total} cells    ", (n + 1) * batch.len());
    }
    eprintln!();
    Ok(())
}

type PrevCell = Option<(char, [u8; 3])>;

fn send_frame(
    session: &mut Session,
    prev: &mut [PrevCell],
    pixels: &[[u8; 3]],
    x0: i16,
    y0: i16,
    width: u32,
) -> Result<(usize, usize)> {
    let mut changed: Vec<CellWrite> = Vec::new();

    for (i, &[r, g, b]) in pixels.iter().enumerate() {
        let ch = pixel_to_cell(r, g, b);
        let new_state = ch.map(|c| (c, [r, g, b]));

        if new_state != prev[i] {
            prev[i] = new_state;
            let ix = (i as u32 % width) as i16;
            let iy = (i as u32 / width) as i16;
            changed.push(CellWrite {
                pos: WorldCoords {
                    x: x0.saturating_add(ix),
                    y: y0.saturating_add(iy),
                },
                ch,
                color: Rgb(r, g, b),
            });
        }
    }

    let cells = changed.len();

    session.send_batch(changed)?;
    Ok((cells, cells))
}

#[allow(clippy::too_many_arguments)]
fn play_gif(
    session: &mut Session,
    prev: &mut [PrevCell],
    path: &PathBuf,
    x0: i16,
    y0: i16,
    width: u32,
    height: u32,
    default_fps: f64,
) -> Result<()> {
    let file = std::io::BufReader::new(std::fs::File::open(path).context("failed to open GIF")?);
    let decoder = image::codecs::gif::GifDecoder::new(file).context("failed to decode GIF")?;
    eprintln!("Decoding and processing GIF frames ...");
    let default_delay = Duration::from_secs_f64(1.0 / default_fps);

    let (tx, rx) = std::sync::mpsc::sync_channel(rayon::current_num_threads() * 2);
    std::thread::spawn(move || {
        for item in decoder.into_frames().enumerate() {
            if tx.send(item).is_err() {
                break;
            }
        }
    });

    let mut processed: Vec<(usize, Duration, Vec<[u8; 3]>)> = rx
        .into_iter()
        .par_bridge()
        .map(|(i, frame_result)| -> Result<_> {
            let frame = frame_result.context("failed to decode GIF frame")?;
            let delay = {
                let (numer, denom) = frame.delay().numer_denom_ms();
                if denom == 0 || numer == 0 {
                    default_delay
                } else {
                    Duration::from_micros(numer as u64 * 1000 / denom as u64)
                }
            };
            let img: image::ImageBuffer<image::Rgb<u8>, Vec<u8>> = frame.buffer().convert();
            let scaled =
                image::imageops::resize(&img, width, height, image::imageops::FilterType::Triangle);
            let pixels: Vec<[u8; 3]> = scaled
                .pixels()
                .map(|ImgRgb([r, g, b])| [*r, *g, *b])
                .collect();
            Ok((i, delay, pixels))
        })
        .collect::<Result<_>>()?;

    processed.sort_unstable_by_key(|(i, _, _)| *i);

    eprintln!("Ready ({} frames)", processed.len());
    eprintln!("Press Enter to start playback ...");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;

    let total = processed.len();
    for loop_n in 1u64.. {
        prev.fill(None);
        for (i, (_, frame_delay, pixels)) in processed.iter().enumerate() {
            let deadline = Instant::now() + *frame_delay;

            let (cells, batches) = send_frame(session, prev, pixels, x0, y0, width)?;
            eprint!(
                "\rloop {loop_n}  frame {}/{total}  {cells} cells  {batches} batches    ",
                i + 1
            );

            let remaining = deadline.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                thread::sleep(remaining);
            }
        }
    }

    Ok(())
}

fn play_stdin(
    session: &mut Session,
    prev: &mut [PrevCell],
    x0: i16,
    y0: i16,
    width: u32,
    height: u32,
    fps: f64,
) -> Result<()> {
    let frame_dur = Duration::from_secs_f64(1.0 / fps);
    let frame_bytes = (width * height * 3) as usize;
    let mut buf = vec![0u8; frame_bytes];
    let mut stdin = std::io::stdin().lock();
    eprintln!("Reading raw RGB24 from stdin ({width}x{height}, {fps} fps) ...");

    for frame_n in 1u64.. {
        let deadline = Instant::now() + frame_dur;

        stdin.read_exact(&mut buf).context("stdin ended")?;

        let pixels: Vec<[u8; 3]> = buf
            .as_chunks::<3>()
            .0
            .iter()
            .map(|c| [c[0], c[1], c[2]])
            .collect();
        let (cells, batches) = send_frame(session, prev, &pixels, x0, y0, width)?;
        eprint!("\rframe {frame_n}  {cells} cells  {batches} batches    ");

        let now = Instant::now();
        if deadline > now {
            thread::sleep(deadline - now);
        }
    }

    Ok(())
}

fn run_session(args: &Args) -> Result<()> {
    let ws = connect_and_auth(
        &args.url,
        args.username.as_deref(),
        args.password.as_deref(),
    )?;

    eprintln!(
        "Playing {}x{} at ({},{}) via {}",
        args.width, args.height, args.x, args.y, args.url
    );

    let bucket: Option<Bucket> = if args.username.is_some() {
        None
    } else {
        Some(Bucket::new())
    };
    let mut session = Session::new(ws, bucket);
    let mut prev: Vec<PrevCell> = vec![None; (args.width * args.height) as usize];

    clear_area(&mut session, args.x, args.y, args.width, args.height)?;

    match &args.file {
        Some(path) => play_gif(
            &mut session,
            &mut prev,
            path,
            args.x,
            args.y,
            args.width,
            args.height,
            args.fps,
        ),
        None => play_stdin(
            &mut session,
            &mut prev,
            args.x,
            args.y,
            args.width,
            args.height,
            args.fps,
        ),
    }
}

fn main() {
    let args = Args::parse();
    eprintln!("Starting player at {}x{} ...", args.width, args.height);

    while let Err(e) = run_session(&args) {
        eprintln!("\nError: {}. Reconnecting in 1s...", e);
        thread::sleep(Duration::from_secs(1));
    }
}
