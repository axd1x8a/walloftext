use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use rand::{
    RngExt,
    distr::{Alphanumeric, Distribution},
};
use tokio::sync::mpsc;
use tracing::info;
use walloftext_shared::{BOARD_HALF, CHUNK_H, CHUNK_W, ChunkCoords, ChunkLocalCoords, Rgb};

use crate::state::{
    AppState, MAX_HOT_ACTIONS, SEGMENT_DIR, SNAPSHOT_PATH, Segment, SegmentEvent, StoredChunkCell,
    UserAccount, WorldShard, WorldSnapshot,
};

pub const SEGMENT_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

fn new_segment(prev_id: u64) -> Segment {
    Segment {
        segment_id: prev_id + 1,
        prev_segment_id: if prev_id == 0 { None } else { Some(prev_id) },
        new_users: Default::default(),
        new_regions: Default::default(),
        actions: Vec::with_capacity(MAX_HOT_ACTIONS),
    }
}

fn has_pending(segment: &Segment) -> bool {
    !segment.actions.is_empty() || !segment.new_users.is_empty() || !segment.new_regions.is_empty()
}

pub fn start_segment_worker(
    mut rx: mpsc::UnboundedReceiver<SegmentEvent>,
    last_segment_id: u64,
    flushed_id: Arc<AtomicU64>,
    force_flush: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        std::fs::create_dir_all(SEGMENT_DIR).expect("cannot create segments dir");

        let mut current = new_segment(last_segment_id);
        let mut last_flush = std::time::Instant::now();

        'outer: loop {
            loop {
                match rx.try_recv() {
                    Ok(event) => match event {
                        SegmentEvent::Action(action) => {
                            current.actions.push(action);
                            if current.actions.len() >= MAX_HOT_ACTIONS {
                                finalize_segment(&current);
                                flushed_id.store(current.segment_id, Ordering::Relaxed);
                                current = new_segment(current.segment_id);
                                last_flush = std::time::Instant::now();
                            }
                        }
                        SegmentEvent::UserUpsert(u) => {
                            current.new_users.insert(u.user_id, u);
                        }
                        SegmentEvent::RegionUpsert(r) => {
                            current.new_regions.insert(r.id, r);
                        }
                        SegmentEvent::RegionRemove(id) => {
                            current.new_regions.remove(&id);
                        }
                    },
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => break 'outer,
                }
            }

            let due = last_flush.elapsed() >= SEGMENT_FLUSH_INTERVAL
                || force_flush.swap(false, Ordering::Relaxed);
            if has_pending(&current) && due {
                finalize_segment(&current);
                flushed_id.store(current.segment_id, Ordering::Relaxed);
                current = new_segment(current.segment_id);
                last_flush = std::time::Instant::now();
            }

            std::thread::sleep(std::time::Duration::from_millis(200));
        }

        if has_pending(&current) {
            finalize_segment(&current);
            flushed_id.store(current.segment_id, Ordering::Relaxed);
        }
    });
}

pub fn finalize_segment(segment: &Segment) {
    let path = format!("{}/{:016x}.seg", SEGMENT_DIR, segment.segment_id);
    let tmp = format!("{}.tmp", path);
    let raw = bitcode::encode(segment);
    match zstd::encode_all(raw.as_slice(), 3) {
        Ok(compressed) => {
            if std::fs::write(&tmp, compressed).is_ok()
                && let Err(e) = std::fs::rename(&tmp, &path)
            {
                tracing::error!("failed to rename segment {}: {}", path, e);
            }
        }
        Err(e) => tracing::error!("segment compression failed: {}", e),
    }
    info!(
        "segment {} finalized ({} actions)",
        segment.segment_id,
        segment.actions.len()
    );
}

pub fn collect_seg_paths_after(base_id: u64) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(SEGMENT_DIR)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some("seg")
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| u64::from_str_radix(s, 16).ok())
                    .map(|id| id > base_id)
                    .unwrap_or(false)
        })
        .collect();
    paths.sort();
    paths
}

pub fn read_segment_file(path: &std::path::Path) -> anyhow::Result<Segment> {
    let compressed = std::fs::read(path)?;
    let raw = zstd::decode_all(compressed.as_slice())?;
    bitcode::decode(&raw).map_err(|e| anyhow::anyhow!("decode segment {:?}: {}", path, e))
}

pub fn read_snapshot_file(path: &std::path::Path) -> anyhow::Result<WorldSnapshot> {
    let compressed = std::fs::read(path)?;
    let raw = zstd::decode_all(compressed.as_slice())?;
    bitcode::decode(&raw).map_err(|e| anyhow::anyhow!("decode snapshot: {}", e))
}

impl AppState {
    pub async fn hydrate(&self) -> anyhow::Result<u64> {
        std::fs::create_dir_all(SEGMENT_DIR)?;

        let snap_exists = std::path::Path::new(SNAPSHOT_PATH).exists();

        let base_segment_id = if snap_exists {
            let snap = tokio::task::spawn_blocking(|| {
                read_snapshot_file(std::path::Path::new(SNAPSHOT_PATH))
            })
            .await??;
            let based_on = snap.based_on_segment_id;
            self.apply_snapshot(snap).await;
            info!("Snapshot loaded (based on segment {}).", based_on);
            based_on
        } else {
            0
        };

        let seg_paths = collect_seg_paths_after(base_segment_id);

        if !snap_exists && seg_paths.is_empty() {
            return self.init_fresh_board().await;
        }

        if seg_paths.is_empty() {
            return Ok(base_segment_id);
        }

        info!(
            "Replaying {} segment file(s) after segment {}...",
            seg_paths.len(),
            base_segment_id
        );

        let state = self.clone();
        let last_id = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
            let mut last_segment_id = base_segment_id;
            let mut cells_delta: i64 = 0;

            for path in &seg_paths {
                let segment = read_segment_file(path)?;

                last_segment_id = segment.segment_id;

                {
                    let mut users_g = state.inner.users.blocking_write();
                    let mut name_g = state.inner.name_to_id.blocking_write();
                    let mut sess_g = state.inner.sessions.blocking_write();
                    for (uid, user) in &segment.new_users {
                        if users_g.len() <= *uid as usize {
                            users_g.resize(*uid as usize + 1, None);
                        }
                        name_g.insert(user.username.clone(), *uid);
                        if !user.session_token.is_empty() {
                            sess_g.insert(user.session_token.clone(), *uid);
                        }
                        users_g[*uid as usize] = Some(user.clone());
                    }
                    if let Some(&max_uid) = segment.new_users.keys().max() {
                        let cur = state.inner.next_uid.load(Ordering::SeqCst);
                        if max_uid + 1 > cur {
                            state.inner.next_uid.store(max_uid + 1, Ordering::SeqCst);
                        }
                    }
                }

                {
                    let mut reg_g = state.inner.regions.blocking_write();
                    for (rid, region) in &segment.new_regions {
                        reg_g.insert(*rid, region.clone());
                    }
                    if let Some(&max_rid) = segment.new_regions.keys().max() {
                        let cur = state.inner.next_rid.load(Ordering::SeqCst);
                        if max_rid + 1 > cur {
                            state.inner.next_rid.store(max_rid + 1, Ordering::SeqCst);
                        }
                    }
                }

                {
                    let mut hot = state.inner.hot_buffer.lock().unwrap();
                    for action in &segment.actions {
                        for change in &action.changes {
                            let (chunk, local) = change.pos.to_chunk();
                            let idx = chunk.shard_idx();
                            let arr_idx = local.to_index();
                            let mut shard = state.inner.shards[idx].write().unwrap();
                            match &change.new {
                                Some(new_cell) => {
                                    let prev = shard.put_cell(chunk, arr_idx, new_cell.clone());
                                    if prev.is_none() {
                                        cells_delta += 1;
                                    }
                                    if new_cell.author_id != 0 {
                                        let mut counts = state.inner.authors_count.blocking_write();
                                        if counts.len() <= new_cell.author_id as usize {
                                            counts.resize(new_cell.author_id as usize + 1, 0);
                                        }
                                        counts[new_cell.author_id as usize] += 1;
                                    }
                                }
                                None => {
                                    let prev = shard.remove_cell(chunk, arr_idx);
                                    if prev.is_some() {
                                        cells_delta -= 1;
                                    }
                                }
                            }
                            drop(shard);
                        }

                        hot.push_back(action.clone());
                        if hot.len() > MAX_HOT_ACTIONS {
                            hot.pop_front();
                        }
                    }
                }

                if let Some(last_action) = segment.actions.last() {
                    let cur = state.inner.next_action_id.load(Ordering::Relaxed);
                    if last_action.action_id + 1 > cur {
                        state
                            .inner
                            .next_action_id
                            .store(last_action.action_id + 1, Ordering::Relaxed);
                    }
                }
            }

            if cells_delta > 0 {
                state
                    .inner
                    .total_cells
                    .fetch_add(cells_delta as u32, Ordering::Relaxed);
            } else if cells_delta < 0 {
                state
                    .inner
                    .total_cells
                    .fetch_sub((-cells_delta) as u32, Ordering::Relaxed);
            }

            info!(
                "Segment replay complete. Last segment: {}.",
                last_segment_id
            );
            Ok(last_segment_id)
        })
        .await??;

        Ok(last_id)
    }

    async fn apply_snapshot(&self, snap: WorldSnapshot) {
        let WorldSnapshot {
            based_on_segment_id: _,
            next_rid,
            next_uid,
            total_cells,
            users,
            regions,
            authors_count,
            chunks,
        } = snap;

        self.inner.next_rid.store(next_rid, Ordering::SeqCst);
        self.inner.next_uid.store(next_uid, Ordering::SeqCst);
        self.inner.total_cells.store(total_cells, Ordering::Relaxed);

        {
            let mut users_g = self.inner.users.write().await;
            let mut name_g = self.inner.name_to_id.write().await;
            let mut sess_g = self.inner.sessions.write().await;
            users_g.resize(next_uid as usize + 1, None);
            for u in users {
                name_g.insert(u.username.clone(), u.user_id);
                if !u.session_token.is_empty() {
                    sess_g.insert(u.session_token.clone(), u.user_id);
                }
                users_g[u.user_id as usize] = Some(u.clone());
            }
        }

        {
            let mut reg_g = self.inner.regions.write().await;
            for r in regions {
                reg_g.insert(r.id, r);
            }
        }

        *self.inner.authors_count.write().await = authors_count;

        let state = self.clone();
        let _ = tokio::task::spawn_blocking(move || {
            for chunk in chunks {
                let idx = chunk.coords.shard_idx();
                let mut array = WorldShard::new_array();
                for cell in &chunk.cells {
                    let arr_idx = cell.local.to_index();
                    array[arr_idx] = Some(cell.clone());
                }
                state.inner.shards[idx]
                    .write()
                    .unwrap()
                    .insert_chunk(chunk.coords, array);
            }
        })
        .await;
    }

    pub fn start_snapshot_worker(&self) {
        let state = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                if let Err(e) = state.write_snapshot().await {
                    tracing::error!("snapshot worker error: {}", e);
                }
            }
        });
    }

    pub async fn write_snapshot(&self) -> anyhow::Result<()> {
        let based_on = self.inner.last_flushed_segment_id.load(Ordering::Relaxed);
        let next_rid = self.inner.next_rid.load(Ordering::SeqCst);
        let next_uid = self.inner.next_uid.load(Ordering::SeqCst);
        let total_cells = self.inner.total_cells.load(Ordering::Relaxed);

        let users: Vec<_> = self
            .inner
            .users
            .read()
            .await
            .iter()
            .filter_map(|u| u.clone())
            .collect();
        let regions: Vec<_> = self.inner.regions.read().await.values().cloned().collect();
        let authors_count = self.inner.authors_count.read().await.clone();
        let chunks = self.snapshot_all_shards();

        let snap = WorldSnapshot {
            based_on_segment_id: based_on,
            next_rid,
            next_uid,
            total_cells,
            users,
            regions,
            authors_count,
            chunks,
        };

        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let raw = bitcode::encode(&snap);
            let compressed = zstd::encode_all(raw.as_slice(), 3)?;
            let tmp = format!("{}.tmp", SNAPSHOT_PATH);
            std::fs::write(&tmp, &compressed)?;
            std::fs::rename(&tmp, SNAPSHOT_PATH)?;
            info!(
                "Snapshot written (based on segment {}, {} cells).",
                snap.based_on_segment_id, snap.total_cells
            );
            Ok(())
        })
        .await?
    }

    async fn init_fresh_board(&self) -> anyhow::Result<u64> {
        info!("No existing data, initializing fresh board...");
        let mut rng = rand::rng();
        let mut total = 0u32;

        for cx in -BOARD_HALF / CHUNK_W..BOARD_HALF / CHUNK_W {
            for cy in -BOARD_HALF / CHUNK_H..BOARD_HALF / CHUNK_H {
                let chunk = ChunkCoords {
                    x: cx as i8,
                    y: cy as i8,
                };
                let mut array = WorldShard::new_array();
                for ly in 0u8..CHUNK_H as u8 {
                    for lx in 0u8..CHUNK_W as u8 {
                        let local = ChunkLocalCoords { x: lx, y: ly };
                        array[local.to_index()] = Some(StoredChunkCell {
                            local,
                            ch: Alphanumeric.sample(&mut rng) as char,
                            color: Rgb(
                                rng.random_range(100..=200),
                                rng.random_range(100..=200),
                                rng.random_range(100..=200),
                            ),
                            author_id: 0,
                            ts: 0,
                        });
                        total += 1;
                    }
                }
                self.inner.shards[chunk.shard_idx()]
                    .write()
                    .unwrap()
                    .insert_chunk(chunk, array);
            }
        }

        self.inner.total_cells.store(total, Ordering::Relaxed);
        {
            let mut counts = self.inner.authors_count.write().await;
            counts.resize(1, 0);
            counts[0] = total;
        }

        let system_user = UserAccount {
            user_id: 0,
            username: "system".to_string(),
            original_anon_name: "system".to_string(),
            password_hash: None,
            session_token: String::new(),
            is_anonymous: false,
            protected_cells: 0,
        };
        {
            let mut users_vec = vec![None; 2];
            users_vec[0] = Some(system_user);
            *self.inner.users.write().await = users_vec;
            self.inner
                .name_to_id
                .write()
                .await
                .insert("system".to_string(), 0);
        }

        self.write_snapshot().await?;
        info!("Fresh board initialized: {} cells.", total);
        Ok(0)
    }
}
