use crate::FoldHashMap;

use bitflagset::BitSet;
use walloftext_shared::{CHUNK_H_USIZE, CHUNK_W_USIZE, ChunkCoords, ClientMsg, Rgb, WorldCoords};

use crate::dom::debug_log;
use wasm_bindgen::JsCast;

use crate::state::{CanvasCell, read_net, with_board};

pub const MAX_FLUSH: u64 = 64;
pub const CHUNK_AREA: usize = CHUNK_W_USIZE * CHUNK_H_USIZE;

const OCCUPIED_WORDS: usize = CHUNK_AREA / 64;

pub struct ChunkData {
    pub chars: Box<[char]>,
    pub colors: Box<[Rgb]>,
    pub authors: Box<[u32]>,
    pub timestamps: Box<[i64]>,
    pub occupied: BitSet<[u64; OCCUPIED_WORDS], usize>,
}

impl ChunkData {
    pub fn new_empty() -> Self {
        Self {
            chars: vec![' '; CHUNK_AREA].into_boxed_slice(),
            colors: vec![Rgb(0, 0, 0); CHUNK_AREA].into_boxed_slice(),
            authors: vec![0u32; CHUNK_AREA].into_boxed_slice(),
            timestamps: vec![0i64; CHUNK_AREA].into_boxed_slice(),
            occupied: BitSet::default(),
        }
    }

    pub fn is_occupied(&self, idx: usize) -> bool {
        self.occupied.contains(&idx)
    }

    pub fn set_occupied(&mut self, idx: usize, val: bool) {
        self.occupied.set(idx, val);
    }
}

pub struct LodChunkData {
    pub colors: Box<[Rgb]>,
    pub occupied: BitSet<[u64; OCCUPIED_WORDS], usize>,
}

impl LodChunkData {
    pub fn new_empty() -> Self {
        Self {
            colors: vec![Rgb(0, 0, 0); CHUNK_AREA].into_boxed_slice(),
            occupied: BitSet::default(),
        }
    }

    pub fn is_occupied(&self, idx: usize) -> bool {
        self.occupied.contains(&idx)
    }

    pub fn set_occupied(&mut self, idx: usize, val: bool) {
        self.occupied.set(idx, val);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FetchStatus {
    None,
    Pending,
    InFlight,
    Loaded,
}

pub struct ChunkNode {
    pub full_data: Option<ChunkData>,
    pub full_status: FetchStatus,

    pub lod_data: Option<LodChunkData>,
    pub lod_status: FetchStatus,
    pub lod_step: u8,

    pub last_seen: f64,
}

impl ChunkNode {
    pub fn lod_color_at(&self, idx: usize) -> Option<Rgb> {
        self.full_data
            .as_ref()
            .and_then(|fd| fd.is_occupied(idx).then(|| fd.colors[idx]))
            .or_else(|| {
                self.lod_data
                    .as_ref()
                    .and_then(|l| l.is_occupied(idx).then(|| l.colors[idx]))
            })
    }
}

impl Default for ChunkNode {
    fn default() -> Self {
        Self {
            full_data: None,
            full_status: FetchStatus::None,
            lod_data: None,
            lod_status: FetchStatus::None,
            lod_step: 0,
            last_seen: 0.0,
        }
    }
}

pub struct ChunkManager {
    pub chunks: FoldHashMap<ChunkCoords, ChunkNode>,
    pub flush_timer: i32,
}

impl ChunkManager {
    pub fn new() -> Self {
        Self {
            chunks: FoldHashMap::default(),
            flush_timer: -1,
        }
    }

    pub fn get_node_mut(&mut self, chunk: ChunkCoords) -> &mut ChunkNode {
        self.chunks.entry(chunk).or_default()
    }

    pub fn coords(pos: WorldCoords) -> (ChunkCoords, usize) {
        let (chunk, local) = pos.to_chunk();
        (chunk, local.to_index())
    }

    pub fn get_cell(&self, pos: WorldCoords) -> Option<CanvasCell> {
        let (chunk, idx) = Self::coords(pos);
        let node = self.chunks.get(&chunk)?;
        let chunk_data = node.full_data.as_ref()?;
        if !chunk_data.is_occupied(idx) {
            return None;
        }
        Some(CanvasCell {
            ch: chunk_data.chars[idx],
            color: chunk_data.colors[idx],
            author_id: chunk_data.authors[idx],
            ts: chunk_data.timestamps[idx],
        })
    }

    pub fn set_cell(&mut self, pos: WorldCoords, cell: Option<CanvasCell>) {
        let (chunk, idx) = Self::coords(pos);
        let node = self.get_node_mut(chunk);

        match cell {
            None => {
                if let Some(chunk_data) = node.full_data.as_mut() {
                    chunk_data.set_occupied(idx, false);
                    chunk_data.chars[idx] = ' ';
                }
            }
            Some(c) => {
                let chunk_data = node.full_data.get_or_insert_with(ChunkData::new_empty);
                chunk_data.chars[idx] = c.ch;
                chunk_data.colors[idx] = c.color;
                chunk_data.authors[idx] = c.author_id;
                chunk_data.timestamps[idx] = c.ts;
                chunk_data.set_occupied(idx, true);
                node.full_status = FetchStatus::Loaded;
            }
        }
    }

    pub fn on_chunk_received(&mut self, chunk: ChunkCoords, data: ChunkData) {
        let node = self.get_node_mut(chunk);
        node.full_status = FetchStatus::Loaded;
        node.full_data = Some(data);
    }

    pub fn on_lod_received(&mut self, chunk: ChunkCoords, step: u8, data: LodChunkData) {
        let node = self.get_node_mut(chunk);
        if node.lod_status == FetchStatus::InFlight && node.lod_step >= step {
            node.lod_status = FetchStatus::Loaded;
            node.lod_step = step;
            node.lod_data = Some(data);
        }
    }

    pub fn on_disconnect(&mut self) {
        debug_log!("chunks on_disconnect: requeuing in-flight requests");
        for node in self.chunks.values_mut() {
            if node.full_status == FetchStatus::InFlight {
                node.full_status = FetchStatus::Pending;
            }
            if node.lod_status == FetchStatus::InFlight {
                node.lod_status = FetchStatus::Pending;
            }
        }
    }

    pub fn schedule_flush_debounced(&mut self, delay_ms: i32) {
        let win = crate::dom::window();
        if self.flush_timer != -1 {
            win.clear_timeout_with_handle(self.flush_timer);
        }
        let cb = wasm_bindgen::closure::Closure::once(|| {
            with_board(|b| b.chunk_manager.flush_timer = -1);
            refresh_chunks(MAX_FLUSH);
        });
        self.flush_timer = win
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                delay_ms,
            )
            .unwrap_or(-1);
        cb.forget();
    }
}

pub fn queue_visible_chunks() {
    let is_conn = read_net(|n| n.is_conn);
    if !is_conn {
        return;
    }

    let (cols, rows, step, is_glyph) = crate::state::visible_chunk_range();

    with_board(|b| {
        for cx in cols {
            for cy in rows.clone() {
                let chunk = ChunkCoords {
                    x: cx as i8,
                    y: cy as i8,
                };
                let node = b.chunk_manager.get_node_mut(chunk);

                if is_glyph {
                    if node.full_status == FetchStatus::None {
                        node.full_status = FetchStatus::Pending;
                    }
                } else if node.lod_status == FetchStatus::None
                    || (node.lod_status == FetchStatus::Loaded && node.lod_step > step)
                {
                    node.lod_status = FetchStatus::Pending;
                    node.lod_step = step;
                }
            }
        }
    });
}

pub fn flush_chunks(max: usize) {
    let (ccx, ccy) = crate::state::viewport_chunk_center();

    let dist = |chunk: ChunkCoords| -> u64 {
        let dx = chunk.x as f64 - ccx;
        let dy = chunk.y as f64 - ccy;
        ((dx * dx + dy * dy) * 1000.0) as u64
    };

    let mut candidates: Vec<(ChunkCoords, u8, u64)> = Vec::with_capacity(max * 2);

    with_board(|b| {
        for (&chunk, node) in &b.chunk_manager.chunks {
            if node.full_status == FetchStatus::Pending {
                candidates.push((chunk, 1, dist(chunk)));
            } else if node.lod_status == FetchStatus::Pending && node.lod_step != 1 {
                candidates.push((chunk, node.lod_step, dist(chunk)));
            }
        }
    });

    if candidates.is_empty() {
        return;
    }

    if candidates.len() > max {
        candidates.select_nth_unstable_by_key(max, |&(_, _, d)| d);
        candidates.truncate(max);
    }

    let mut full_chunks: Vec<ChunkCoords> = Vec::new();
    let mut lod_entries: Vec<(ChunkCoords, u8)> = Vec::new();

    with_board(|b| {
        for (chunk, step, _) in &candidates {
            let node = b.chunk_manager.get_node_mut(*chunk);
            if *step == 1 {
                node.full_status = FetchStatus::InFlight;
                full_chunks.push(*chunk);
            } else {
                node.lod_status = FetchStatus::InFlight;
                lod_entries.push((*chunk, *step));
            }
        }
    });

    if !full_chunks.is_empty() {
        read_net(|n| {
            n.send(&ClientMsg::FetchChunkBatch {
                chunks: full_chunks,
            })
        });
    }
    if !lod_entries.is_empty() {
        read_net(|n| {
            n.send(&ClientMsg::FetchLodBatch {
                entries: lod_entries,
            })
        });
    }
}

pub fn refresh_chunks(max: u64) {
    queue_visible_chunks();
    flush_chunks(max as usize);
}

pub fn evict_unseen(
    chunks: &mut crate::FoldHashMap<ChunkCoords, ChunkNode>,
    cutoff: f64,
    col_range: std::ops::RangeInclusive<i64>,
    row_range: std::ops::RangeInclusive<i64>,
) {
    chunks.retain(|c, node| {
        let in_vp = col_range.contains(&(c.x as i64)) && row_range.contains(&(c.y as i64));
        in_vp || node.last_seen >= cutoff
    });
}
