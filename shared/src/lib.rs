use bitcode::{Decode, Encode};
use std::collections::HashMap;

pub const BOARD_HALF: i64 = 2048;
pub const BOARD_SIZE: i64 = BOARD_HALF * 2;

#[inline]
pub fn clamp_coord(n: i64) -> i16 {
    n.clamp(-BOARD_HALF, BOARD_HALF - 1) as i16
}

pub const CHUNK_W: i64 = 64;
pub const CHUNK_H: i64 = 16;
pub const CHUNK_W_USIZE: usize = CHUNK_W as usize;
pub const CHUNK_H_USIZE: usize = CHUNK_H as usize;
pub const CHUNK_AREA: usize = (CHUNK_W * CHUNK_H) as usize;

pub const MAX_PROTECTED_CELLS: u64 = 256;

pub const NUM_SHARDS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct WorldCoords {
    pub x: i16,
    pub y: i16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct ChunkCoords {
    pub x: i8,
    pub y: i8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct ChunkLocalCoords {
    pub x: u8,
    pub y: u8,
}

impl WorldCoords {
    #[inline]
    pub fn from_i64(x: i64, y: i64) -> Self {
        Self {
            x: x.clamp(-BOARD_HALF, BOARD_HALF - 1) as i16,
            y: y.clamp(-BOARD_HALF, BOARD_HALF - 1) as i16,
        }
    }

    #[inline]
    pub fn clamped(self) -> Self {
        Self::from_i64(self.x as i64, self.y as i64)
    }

    #[inline]
    pub fn to_chunk(self) -> (ChunkCoords, ChunkLocalCoords) {
        let x = self.x as i64;
        let y = self.y as i64;
        (
            ChunkCoords {
                x: x.div_euclid(CHUNK_W) as i8,
                y: y.div_euclid(CHUNK_H) as i8,
            },
            ChunkLocalCoords {
                x: x.rem_euclid(CHUNK_W) as u8,
                y: y.rem_euclid(CHUNK_H) as u8,
            },
        )
    }

    #[inline]
    pub fn offset(self, dx: i64, dy: i64) -> Self {
        Self::from_i64(self.x as i64 + dx, self.y as i64 + dy)
    }
}

impl ChunkCoords {
    #[inline]
    pub fn from_world(w: WorldCoords) -> Self {
        w.to_chunk().0
    }

    #[inline]
    pub fn shard_idx(self) -> usize {
        let cx = self.x as i64;
        let cy = self.y as i64;
        let mut h = cx ^ (cy << 13);
        h = h ^ (h >> 17);
        (h.unsigned_abs() as usize) % NUM_SHARDS
    }
}

impl ChunkLocalCoords {
    #[inline]
    pub fn to_index(self) -> usize {
        self.y as usize * CHUNK_W_USIZE + self.x as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Encode, Decode)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn from_hex(s: &str) -> Self {
        let s = s.trim_start_matches('#');
        let r = u8::from_str_radix(&s[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&s[2..4], 16).unwrap_or(255);
        let b = u8::from_str_radix(&s[4..6], 16).unwrap_or(65);
        Self(r, g, b)
    }
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }
    pub fn to_floats(self) -> (f32, f32, f32) {
        (
            self.0 as f32 / 255.0,
            self.1 as f32 / 255.0,
            self.2 as f32 / 255.0,
        )
    }
}

#[derive(Debug, Clone, Encode, Decode)]
pub struct ChunkCell {
    pub local: ChunkLocalCoords,
    pub ch: char,
    pub color: Rgb,
    pub ts: i64,
    pub author_id: u32,
}

#[derive(Debug, Clone, Copy, Encode, Decode)]
pub struct LodCell {
    pub local: ChunkLocalCoords,
    pub color: Rgb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub struct WorldRect {
    pub a: WorldCoords,
    pub b: WorldCoords,
}

impl WorldRect {
    pub fn new(a: WorldCoords, b: WorldCoords) -> Self {
        Self { a, b }
    }

    pub fn from_corners(p: WorldCoords, q: WorldCoords) -> Self {
        Self {
            a: WorldCoords {
                x: p.x.min(q.x),
                y: p.y.min(q.y),
            },
            b: WorldCoords {
                x: p.x.max(q.x),
                y: p.y.max(q.y),
            },
        }
    }

    #[inline]
    pub fn width(&self) -> i64 {
        (self.b.x as i64 - self.a.x as i64 + 1).max(0)
    }
    #[inline]
    pub fn height(&self) -> i64 {
        (self.b.y as i64 - self.a.y as i64 + 1).max(0)
    }
    #[inline]
    pub fn area(&self) -> u64 {
        self.width() as u64 * self.height() as u64
    }

    #[inline]
    pub fn contains(&self, wc: WorldCoords) -> bool {
        wc.x >= self.a.x && wc.x <= self.b.x && wc.y >= self.a.y && wc.y <= self.b.y
    }

    pub fn visible_chunk_columns(&self) -> std::ops::RangeInclusive<i64> {
        let gx0 = (self.a.x as i64 / CHUNK_W - 1).max(-BOARD_HALF / CHUNK_W);
        let gx1 = (self.b.x as i64 / CHUNK_W + 1).min(BOARD_HALF / CHUNK_W);
        gx0..=gx1
    }

    pub fn visible_chunk_rows(&self) -> std::ops::RangeInclusive<i64> {
        let gy0 = (self.a.y as i64 / CHUNK_H - 1).max(-BOARD_HALF / CHUNK_H);
        let gy1 = (self.b.y as i64 / CHUNK_H + 1).min(BOARD_HALF / CHUNK_H);
        gy0..=gy1
    }
}

#[derive(Debug, Clone, Encode, Decode)]
pub struct RegionRecord {
    pub id: u32,
    pub bounds: WorldRect,
    pub label: String,
    pub owner_id: u32,
    pub owner: String,
}

#[derive(Debug, Clone, Encode, Decode)]
pub struct AuthorStat {
    pub name: String,
    pub count: u32,
}

#[derive(Debug, Clone, Encode, Decode)]
pub struct CellWrite {
    pub pos: WorldCoords,
    pub ch: Option<char>,
    pub color: Rgb,
}

#[derive(Debug, Clone, Encode, Decode)]
pub struct RestoreCoord {
    pub pos: WorldCoords,
}

#[derive(Debug, Clone, Encode, Decode)]
pub struct CellUpdateEntry {
    pub pos: WorldCoords,
    pub ch: Option<char>,
    pub color: Rgb,
    pub ts: i64,
    pub author_id: u32,
}

#[derive(Debug, Clone, Encode, Decode)]
pub struct OnlineUser {
    pub user_id: u32,
    pub name: String,
    pub color: Rgb,
    pub pos: Option<WorldCoords>,
}

#[derive(Debug, Clone, Encode, Decode)]
pub enum ServerMsg {
    Welcome {
        author_id: u32,
        author: String,
        anon_name: String,
        session_token: String,
    },
    CellUpdate {
        entry: CellUpdateEntry,
        author: String,
    },
    CellUpdateBatch {
        updates: Vec<CellUpdateEntry>,
        authors: Vec<(u32, String)>,
    },
    ChunkDataBatch {
        chunks: Vec<(ChunkCoords, Vec<ChunkCell>)>,
        authors: HashMap<u32, String>,
    },
    LodDataBatch {
        entries: Vec<(ChunkCoords, u8, Vec<LodCell>)>,
    },
    Stats {
        total_cells: u32,
        online: u16,
        top_authors: Vec<AuthorStat>,
    },
    RegionData(Vec<RegionRecord>),
    RegionAdded(RegionRecord),
    RegionRemoved {
        id: u32,
    },
    Denied {
        pos: WorldCoords,
        reason: String,
    },
    DeniedBatch {
        positions: Vec<WorldCoords>,
        reason: String,
    },
    Error(String),
    AuthOk {
        username: String,
        protected_cells: u32,
    },
    AuthFail {
        reason: String,
    },
    CursorUpdate {
        user_id: u32,
        name: String,
        pos: WorldCoords,
        color: Rgb,
    },
    CursorLeft {
        user_id: u32,
    },
    OnlineList {
        users: Vec<OnlineUser>,
    },
    Kicked,
}

#[derive(Debug, Clone, Encode, Decode)]
pub struct FontAtlasFile {
    pub atlas_w: u16,
    pub atlas_h: u16,
    pub cell_w: u8,
    pub cell_h: u8,

    pub codepoints: Vec<u32>,
    pub atlas_x: Vec<u16>,
    pub atlas_y: Vec<u16>,
    pub w: Vec<u8>,
    pub h: Vec<u8>,
    pub cell_off_x: Vec<i8>,
    pub cell_off_y: Vec<i8>,

    pub pixels_1bit: Vec<u8>,
}

#[derive(Debug, Clone, Encode, Decode)]
pub enum ClientMsg {
    SetCellBatch { cells: Vec<CellWrite>, commit: bool },
    RestoreCellBatch { cells: Vec<RestoreCoord> },
    FetchChunkBatch { chunks: Vec<ChunkCoords> },
    FetchLodBatch { entries: Vec<(ChunkCoords, u8)> },
    FetchStats,
    FetchRegions,
    AddRegion { bounds: WorldRect, label: String },
    RemoveRegion { id: u32 },
    Login { username: String, password: String },
    Logout,
    UpdateViewport(WorldRect),
    UpdateCursor { pos: WorldCoords, color: Rgb },
}
