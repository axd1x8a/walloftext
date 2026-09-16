use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{RwLock as AsyncRwLock, broadcast, mpsc};
use walloftext_shared::{
    AuthorStat, CHUNK_AREA, CHUNK_H, CHUNK_W, CellUpdateEntry, ChunkCell, ChunkCoords,
    ChunkLocalCoords, FontAtlasFile, MAX_PROTECTED_CELLS, NUM_SHARDS, RegionRecord, Rgb,
    WorldCoords, WorldRect,
};

use crate::error::AppError;

pub const SEGMENT_DIR: &str = "./data/segments";
pub const SNAPSHOT_PATH: &str = "./data/walloftext.snap";
pub const MAX_HOT_ACTIONS: usize = 16_384;

pub const CELL_BUCKET_MAX: u64 = 200;
pub const CELL_REFILL_PER_SEC: u64 = 40;

#[derive(Debug, Clone, bitcode::Encode, bitcode::Decode)]
pub struct StoredChunkCell {
    pub local: ChunkLocalCoords,
    pub ch: char,
    pub color: walloftext_shared::Rgb,
    pub author_id: u32,
    pub ts: i64,
}

#[derive(Debug, Clone, bitcode::Encode, bitcode::Decode)]
pub struct StoredRegion {
    pub id: u32,
    pub bounds: WorldRect,
    pub label: String,
    pub owner_id: u32,
}

#[derive(Debug, Clone, bitcode::Encode, bitcode::Decode)]
pub struct UserAccount {
    pub user_id: u32,
    pub username: String,
    pub original_anon_name: String,
    pub password_hash: Option<String>,
    pub session_token: String,
    pub is_anonymous: bool,
    pub protected_cells: u32,
}

#[derive(Debug, Clone, bitcode::Encode, bitcode::Decode)]
pub struct StoredChunk {
    pub coords: ChunkCoords,
    pub cells: Vec<StoredChunkCell>,
}

#[derive(Debug, Clone, bitcode::Encode, bitcode::Decode)]
pub struct WorldSnapshot {
    pub based_on_segment_id: u64,
    pub next_rid: u32,
    pub next_uid: u32,
    pub total_cells: u32,
    pub users: Vec<UserAccount>,
    pub regions: Vec<StoredRegion>,
    pub authors_count: Vec<u32>,
    pub chunks: Vec<StoredChunk>,
}

#[derive(Debug, Clone, bitcode::Encode, bitcode::Decode)]
pub struct CellChange {
    pub pos: WorldCoords,
    pub old: Option<StoredChunkCell>,
    pub new: Option<StoredChunkCell>,
}

impl CellChange {
    pub fn to_update_entry(&self, action: &Action) -> CellUpdateEntry {
        match &self.new {
            Some(c) => CellUpdateEntry {
                pos: self.pos,
                ch: Some(c.ch),
                color: c.color,
                ts: c.ts,
                author_id: c.author_id,
            },
            None => CellUpdateEntry {
                pos: self.pos,
                ch: None,
                color: Rgb(0, 0, 0),
                ts: action.ts,
                author_id: action.author_id,
            },
        }
    }
}

#[derive(Debug, Clone, bitcode::Encode, bitcode::Decode)]
pub struct Action {
    pub action_id: u64,
    pub author_id: u32,
    pub ts: i64,
    pub changes: Vec<CellChange>,
}

#[derive(Debug, Clone, bitcode::Encode, bitcode::Decode)]
pub struct Segment {
    pub segment_id: u64,
    pub prev_segment_id: Option<u64>,
    pub new_users: HashMap<u32, UserAccount>,
    pub new_regions: HashMap<u32, StoredRegion>,
    pub actions: Vec<Action>,
}

pub enum SegmentEvent {
    Action(Action),
    UserUpsert(UserAccount),
    RegionUpsert(StoredRegion),
    RegionRemove(u32),
}

pub struct WorldShard {
    chunks: HashMap<ChunkCoords, Box<[Option<StoredChunkCell>; CHUNK_AREA]>>,
}

impl WorldShard {
    pub fn new_array() -> Box<[Option<StoredChunkCell>; CHUNK_AREA]> {
        vec![None; CHUNK_AREA]
            .into_boxed_slice()
            .try_into()
            .unwrap()
    }

    pub fn put_cell(
        &mut self,
        chunk: ChunkCoords,
        arr_idx: usize,
        cell: StoredChunkCell,
    ) -> Option<StoredChunkCell> {
        let arr = self.chunks.entry(chunk).or_insert_with(Self::new_array);
        arr[arr_idx].replace(cell)
    }

    pub fn remove_cell(&mut self, chunk: ChunkCoords, arr_idx: usize) -> Option<StoredChunkCell> {
        self.chunks
            .get_mut(&chunk)
            .and_then(|arr| arr[arr_idx].take())
    }

    pub fn get_cell(&self, chunk: ChunkCoords, arr_idx: usize) -> Option<StoredChunkCell> {
        self.chunks.get(&chunk).and_then(|arr| arr[arr_idx].clone())
    }

    pub fn get_chunk(&self, chunk: ChunkCoords) -> Vec<StoredChunkCell> {
        self.chunks
            .get(&chunk)
            .map(|arr| arr.iter().filter_map(|opt| opt.clone()).collect())
            .unwrap_or_default()
    }

    pub fn insert_chunk(
        &mut self,
        chunk: ChunkCoords,
        data: Box<[Option<StoredChunkCell>; CHUNK_AREA]>,
    ) {
        self.chunks.insert(chunk, data);
    }

    pub fn snapshot(&self) -> Vec<StoredChunk> {
        self.chunks
            .iter()
            .filter_map(|(&chunk, arr)| {
                let cells: Vec<StoredChunkCell> =
                    arr.iter().filter_map(|opt| opt.clone()).collect();
                if cells.is_empty() {
                    None
                } else {
                    Some(StoredChunk {
                        coords: chunk,
                        cells,
                    })
                }
            })
            .collect()
    }
}

pub struct ConnInfo {
    pub direct_tx: mpsc::UnboundedSender<Vec<u8>>,
    #[allow(dead_code)]
    pub kick_tx: tokio::sync::oneshot::Sender<()>,
    pub viewport: Option<WorldRect>,
    pub cursor: Option<WorldCoords>,
    pub color: walloftext_shared::Rgb,
    pub user_id: u32,
    pub name: String,
}

pub struct AppStateInner {
    pub shards: Vec<RwLock<WorldShard>>,

    pub users: AsyncRwLock<Vec<Option<UserAccount>>>,
    pub authors_count: AsyncRwLock<Vec<u32>>,
    pub name_to_id: AsyncRwLock<HashMap<String, u32>>,
    pub sessions: AsyncRwLock<HashMap<String, u32>>,
    pub regions: AsyncRwLock<HashMap<u32, StoredRegion>>,

    pub connected: AtomicU16,
    pub next_rid: AtomicU32,
    pub next_uid: AtomicU32,
    pub next_action_id: AtomicU64,
    pub next_conn_id: AtomicU64,
    pub total_cells: AtomicU32,

    pub connections: Mutex<HashMap<u64, ConnInfo>>,

    pub login_throttle: Mutex<HashMap<String, (u32, u64)>>,
    pub tx: broadcast::Sender<Vec<u8>>,
    pub segment_tx: mpsc::UnboundedSender<SegmentEvent>,
    pub last_flushed_segment_id: Arc<AtomicU64>,

    pub cell_update_buffer: Mutex<Vec<(CellUpdateEntry, String)>>,
    pub hot_buffer: Mutex<VecDeque<Action>>,
    pub cell_rate: Mutex<HashMap<u32, (u64, std::time::Instant)>>,

    pub font_atlas: Arc<FontAtlasFile>,
    pub index_html: Arc<String>,

    pub admin_token: String,
}

#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<AppStateInner>,
}

impl AppState {
    pub fn epoch_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    pub fn new(
        segment_tx: mpsc::UnboundedSender<SegmentEvent>,
        font_atlas: Arc<FontAtlasFile>,
        index_html: Arc<String>,
        admin_token: String,
    ) -> Self {
        let shards = (0..NUM_SHARDS)
            .map(|_| {
                RwLock::new(WorldShard {
                    chunks: HashMap::new(),
                })
            })
            .collect();

        let inner = AppStateInner {
            shards,
            users: AsyncRwLock::new(Vec::new()),
            authors_count: AsyncRwLock::new(Vec::new()),
            name_to_id: AsyncRwLock::new(HashMap::new()),
            sessions: AsyncRwLock::new(HashMap::new()),
            regions: AsyncRwLock::new(HashMap::new()),
            connected: AtomicU16::new(0),
            next_rid: AtomicU32::new(1),
            next_uid: AtomicU32::new(1),
            next_action_id: AtomicU64::new(0),
            next_conn_id: AtomicU64::new(0),
            total_cells: AtomicU32::new(0),
            connections: Mutex::new(HashMap::new()),
            login_throttle: Mutex::new(HashMap::new()),
            tx: broadcast::channel::<Vec<u8>>(1024).0,
            segment_tx,
            cell_update_buffer: Mutex::new(Vec::new()),
            hot_buffer: Mutex::new(VecDeque::new()),
            cell_rate: Mutex::new(HashMap::new()),
            last_flushed_segment_id: Arc::new(AtomicU64::new(0)),
            font_atlas,
            index_html,
            admin_token,
        };

        Self {
            inner: Arc::new(inner),
        }
    }

    pub async fn apply_action_batch(
        &self,
        author_id: u32,
        updates: Vec<(WorldCoords, Option<StoredChunkCell>)>,
    ) -> Action {
        let ts = Self::epoch_secs() as i64;
        let mut changes = Vec::with_capacity(updates.len());
        let mut total_delta: i64 = 0;
        let mut new_authors: HashSet<u32> = HashSet::new();

        for (pos, mut new_cell) in updates {
            let (chunk, local) = pos.to_chunk();
            let idx = chunk.shard_idx();
            let arr_idx = local.to_index();

            if let Some(ref mut c) = new_cell {
                c.local = local;
            }

            let old_cell = {
                let mut shard = self.inner.shards[idx].write().unwrap();
                match new_cell.clone() {
                    Some(c) => shard.put_cell(chunk, arr_idx, c),
                    None => shard.remove_cell(chunk, arr_idx),
                }
            };

            match &new_cell {
                Some(c) => {
                    if old_cell.is_none() {
                        total_delta += 1;
                    }
                    if c.author_id != 0 {
                        new_authors.insert(c.author_id);
                    }
                }
                None => {
                    if old_cell.is_some() {
                        total_delta -= 1;
                    }
                }
            }

            changes.push(CellChange {
                pos,
                old: old_cell,
                new: new_cell,
            });
        }

        match total_delta.cmp(&0) {
            std::cmp::Ordering::Greater => {
                self.inner
                    .total_cells
                    .fetch_add(total_delta as u32, Ordering::Relaxed);
            }
            std::cmp::Ordering::Less => {
                self.inner
                    .total_cells
                    .fetch_sub((-total_delta) as u32, Ordering::Relaxed);
            }
            std::cmp::Ordering::Equal => {}
        }

        if !new_authors.is_empty() {
            let mut counts = self.inner.authors_count.write().await;
            for aid in &new_authors {
                if counts.len() <= *aid as usize {
                    counts.resize(*aid as usize + 1, 0);
                }
                counts[*aid as usize] += 1;
            }
        }

        let action_id = self.inner.next_action_id.fetch_add(1, Ordering::Relaxed);
        let action = Action {
            action_id,
            author_id,
            ts,
            changes,
        };

        {
            let mut hot = self.inner.hot_buffer.lock().unwrap();
            hot.push_back(action.clone());
            if hot.len() > MAX_HOT_ACTIONS {
                hot.pop_front();
            }
        }

        let _ = self
            .inner
            .segment_tx
            .send(SegmentEvent::Action(action.clone()));

        action
    }

    pub fn current_cell(&self, pos: WorldCoords) -> Option<StoredChunkCell> {
        let (chunk, local) = pos.to_chunk();
        self.inner.shards[chunk.shard_idx()]
            .read()
            .unwrap()
            .get_cell(chunk, local.to_index())
    }

    pub async fn username_of(&self, uid: u32) -> String {
        let users = self.inner.users.read().await;
        if let Some(Some(u)) = users.get(uid as usize) {
            u.username.clone()
        } else {
            format!("anonymous#{}", uid)
        }
    }

    pub async fn get_session(&self, token: &str) -> Option<u32> {
        self.inner.sessions.read().await.get(token).copied()
    }

    pub async fn get_user_by_name(&self, name: &str) -> Option<UserAccount> {
        let uid = self.inner.name_to_id.read().await.get(name).copied()?;
        self.get_user_by_id(uid).await
    }

    pub async fn get_user_by_id(&self, uid: u32) -> Option<UserAccount> {
        self.inner
            .users
            .read()
            .await
            .get(uid as usize)
            .cloned()
            .flatten()
    }

    pub async fn find_region_at(&self, pos: WorldCoords) -> Option<StoredRegion> {
        self.inner
            .regions
            .read()
            .await
            .values()
            .find(|r| r.bounds.contains(pos))
            .cloned()
    }

    pub async fn all_regions(&self) -> Vec<StoredRegion> {
        self.inner.regions.read().await.values().cloned().collect()
    }

    pub async fn region_to_record(&self, r: &StoredRegion) -> RegionRecord {
        RegionRecord {
            id: r.id,
            bounds: r.bounds,
            label: r.label.clone(),
            owner_id: r.owner_id,
            owner: self.username_of(r.owner_id).await,
        }
    }

    pub fn check_cell_rate(&self, user_id: u32, cost: u64) -> Result<(), AppError> {
        if user_id == 0 || cost == 0 {
            return Ok(());
        }
        let now = std::time::Instant::now();
        let mut map = self.inner.cell_rate.lock().unwrap();
        let (tokens, last) = map.entry(user_id).or_insert((CELL_BUCKET_MAX, now));
        let elapsed = now.duration_since(*last).as_secs_f64();
        let refilled = (*tokens as f64 + elapsed * CELL_REFILL_PER_SEC as f64) as u64;
        *tokens = refilled.min(CELL_BUCKET_MAX);
        *last = now;
        if *tokens >= cost {
            *tokens -= cost;
            Ok(())
        } else {
            Err(AppError::BadInput("rate limited: slow down".into()))
        }
    }

    pub async fn add_region(&self, mut r: StoredRegion) -> Result<u32, AppError> {
        let area = r.bounds.area();
        if area == 0 {
            return Err(AppError::BadInput("empty region".into()));
        }
        if area > MAX_PROTECTED_CELLS && r.owner_id != 0 {
            return Err(AppError::Quota(format!(
                "too large ({} cells, max {})",
                area, MAX_PROTECTED_CELLS
            )));
        }
        let used = self
            .get_user_by_id(r.owner_id)
            .await
            .map(|u| u.protected_cells as u64)
            .unwrap_or(0);
        if used + area > MAX_PROTECTED_CELLS && r.owner_id != 0 {
            return Err(AppError::Quota(format!(
                "quota: {}/{} cells used",
                used, MAX_PROTECTED_CELLS
            )));
        }

        let id = self.inner.next_rid.fetch_add(1, Ordering::SeqCst);
        r.id = id;
        self.inner.regions.write().await.insert(id, r.clone());
        self.adjust_cells(r.owner_id, area as i64).await;
        let _ = self.inner.segment_tx.send(SegmentEvent::RegionUpsert(r));
        Ok(id)
    }

    pub async fn remove_region(&self, id: u32, requester_uid: u32) -> Result<bool, AppError> {
        let mut regions = self.inner.regions.write().await;
        if let Some(r) = regions.get(&id)
            && r.owner_id == requester_uid
        {
            let area = r.bounds.area();
            regions.remove(&id);
            drop(regions);
            self.adjust_cells(requester_uid, -(area as i64)).await;
            let _ = self.inner.segment_tx.send(SegmentEvent::RegionRemove(id));
            return Ok(true);
        }
        Ok(false)
    }

    pub async fn adjust_cells(&self, uid: u32, delta: i64) {
        let mut users = self.inner.users.write().await;
        if let Some(Some(u)) = users.get_mut(uid as usize) {
            u.protected_cells = (u.protected_cells as i64 + delta).max(0) as u32;
            let _ = self
                .inner
                .segment_tx
                .send(SegmentEvent::UserUpsert(u.clone()));
        }
    }

    pub async fn get_chunk_with_authors(
        &self,
        chunk: ChunkCoords,
    ) -> (Vec<ChunkCell>, HashMap<u32, String>) {
        let cells = self.inner.shards[chunk.shard_idx()]
            .read()
            .unwrap()
            .get_chunk(chunk);

        let mut unique_authors: HashSet<u32> = HashSet::new();
        let chunk_cells: Vec<ChunkCell> = cells
            .into_iter()
            .map(|c| {
                unique_authors.insert(c.author_id);
                ChunkCell {
                    local: c.local,
                    ch: c.ch,
                    color: c.color,
                    author_id: c.author_id,
                    ts: c.ts,
                }
            })
            .collect();

        let mut authors_map: HashMap<u32, String> = HashMap::with_capacity(unique_authors.len());
        for uid in unique_authors {
            authors_map.insert(uid, self.username_of(uid).await);
        }
        (chunk_cells, authors_map)
    }

    pub async fn get_lod_colors(
        &self,
        chunk: ChunkCoords,
        target_step: u8,
    ) -> Vec<walloftext_shared::LodCell> {
        let cells = self.inner.shards[chunk.shard_idx()]
            .read()
            .unwrap()
            .get_chunk(chunk);

        let target_step = target_step as i64;
        let cx = chunk.x as i64;
        let cy = chunk.y as i64;

        let mut seen: HashSet<(i64, i64)> = HashSet::new();
        let mut result = Vec::new();

        for c in cells {
            let wx = cx * CHUNK_W + c.local.x as i64;
            let wy = cy * CHUNK_H + c.local.y as i64;

            let wx_snap = wx.div_euclid(target_step) * target_step;
            let wy_snap = wy.div_euclid(target_step) * target_step;

            if !seen.insert((wx_snap, wy_snap)) {
                continue;
            }

            let lx_delta = wx_snap - cx * CHUNK_W;
            let ly_delta = wy_snap - cy * CHUNK_H;
            if !(0..CHUNK_W).contains(&lx_delta) || !(0..CHUNK_H).contains(&ly_delta) {
                continue;
            }

            result.push(walloftext_shared::LodCell {
                local: ChunkLocalCoords {
                    x: lx_delta as u8,
                    y: ly_delta as u8,
                },
                color: c.color,
            });
        }
        result
    }

    pub fn snapshot_all_shards(&self) -> Vec<StoredChunk> {
        let mut result = Vec::new();
        for shard in &self.inner.shards {
            result.extend(shard.read().unwrap().snapshot());
        }
        result
    }

    pub async fn top_authors(&self, n: usize) -> Vec<AuthorStat> {
        let counts = self.inner.authors_count.read().await;
        let users = self.inner.users.read().await;

        let mut v: Vec<(u32, u32)> = counts
            .iter()
            .enumerate()
            .filter(|&(_, &count)| count > 0)
            .map(|(uid, &count)| (uid as u32, count))
            .collect();

        v.sort_by_key(|a| std::cmp::Reverse(a.1));
        v.truncate(n);

        v.into_iter()
            .map(|(uid, count)| {
                let name = if let Some(Some(u)) = users.get(uid as usize) {
                    u.username.clone()
                } else {
                    format!("anonymous#{}", uid)
                };
                AuthorStat { name, count }
            })
            .collect()
    }
}
