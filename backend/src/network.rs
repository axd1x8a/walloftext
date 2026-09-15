use axum::{
    Json, Router,
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tower_http::services::ServeDir;
use walloftext_shared::{
    CellUpdateEntry, CellWrite, ClientMsg, OnlineUser, RestoreCoord, Rgb, ServerMsg, WorldCoords,
};

use crate::state::{AppState, ConnInfo, StoredChunkCell};

pub fn encode(msg: &ServerMsg) -> Vec<u8> {
    let raw = bitcode::encode(msg);
    zstd::encode_all(raw.as_slice(), 1).unwrap_or(raw)
}

impl AppState {
    pub fn broadcast(&self, msg: &ServerMsg) {
        let _ = self.inner.tx.send(encode(msg));
    }

    pub fn queue_cell_update(&self, entry: CellUpdateEntry, author: String) {
        self.inner
            .cell_update_buffer
            .lock()
            .unwrap()
            .push((entry, author));
    }

    pub fn start_cell_update_worker(&self) {
        let state = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(40));
            loop {
                interval.tick().await;
                let entries: Vec<(CellUpdateEntry, String)> = {
                    let mut buf = state.inner.cell_update_buffer.lock().unwrap();
                    if buf.is_empty() {
                        continue;
                    }
                    std::mem::take(&mut *buf)
                };
                let conns = state.inner.connections.lock().unwrap();
                for conn in conns.values() {
                    let relevant: Vec<_> = entries
                        .iter()
                        .filter(|(e, _)| conn.viewport.is_none_or(|r| r.contains(e.pos)))
                        .collect();
                    if relevant.is_empty() {
                        continue;
                    }
                    let msg = match relevant.as_slice() {
                        [(entry, author)] => ServerMsg::CellUpdate {
                            entry: (*entry).clone(),
                            author: author.clone(),
                        },
                        _ => {
                            let mut authors: Vec<(u32, String)> = Vec::with_capacity(2);
                            let updates = relevant
                                .iter()
                                .map(|(entry, author)| {
                                    if !authors.iter().any(|(id, _)| *id == entry.author_id) {
                                        authors.push((entry.author_id, author.clone()));
                                    }
                                    (*entry).clone()
                                })
                                .collect();
                            ServerMsg::CellUpdateBatch { updates, authors }
                        }
                    };
                    let _ = conn.direct_tx.send(encode(&msg));
                }
            }
        });
    }
}

async fn check_region_access(
    state: &AppState,
    user_id: u32,
    pos: WorldCoords,
    direct_tx: &mpsc::UnboundedSender<Vec<u8>>,
) -> bool {
    if let Some(region) = state.find_region_at(pos).await
        && region.owner_id != user_id
    {
        let _ = direct_tx.send(encode(&ServerMsg::Denied {
            pos,
            reason: format!("Can't edit protected region: {}", region.label),
        }));
        return false;
    }
    true
}

async fn apply_set_cell_batch(
    state: &AppState,
    user_id: u32,
    cells: Vec<CellWrite>,
    commit: bool,
    direct_tx: &mpsc::UnboundedSender<Vec<u8>>,
) -> bool {
    if let Err(e) = state.check_cell_rate(user_id, cells.len() as u64) {
        let positions: Vec<_> = cells.iter().map(|c| c.pos.clamped()).collect();
        let _ = direct_tx.send(encode(&ServerMsg::DeniedBatch {
            positions,
            reason: e.to_string(),
        }));
        return true;
    }
    let mut updates = Vec::with_capacity(cells.len());
    for c in &cells {
        let pos = c.pos.clamped();
        if !check_region_access(state, user_id, pos, direct_tx).await {
            continue;
        }
        let ts = AppState::epoch_secs() as i64;
        let (_, local) = pos.to_chunk();
        let new_cell = c.ch.map(|ch| StoredChunkCell {
            local,
            ch,
            color: c.color,
            author_id: user_id,
            ts,
        });
        updates.push((pos, new_cell));
    }
    let author = state.username_of(user_id).await;

    if commit {
        let action = state.apply_action_batch(user_id, updates).await;
        queue_action_for_broadcast(state, &action, &author);
    } else {
        broadcast_transient_updates(state, user_id, updates, author);
    }
    false
}

async fn apply_restore_cell_batch(
    state: &AppState,
    user_id: u32,
    cells: Vec<RestoreCoord>,
    direct_tx: &mpsc::UnboundedSender<Vec<u8>>,
) {
    let mut updates = Vec::with_capacity(cells.len());
    for c in &cells {
        let pos = c.pos.clamped();
        if !check_region_access(state, user_id, pos, direct_tx).await {
            continue;
        }
        let last_change = {
            let hot = state.inner.hot_buffer.lock().unwrap();
            hot.iter()
                .rev()
                .flat_map(|a| a.changes.iter())
                .find(|ch| ch.pos == pos)
                .cloned()
        };
        if let Some(change) = last_change {
            updates.push((pos, change.old.clone()));
        }
    }
    if !updates.is_empty() {
        let action = state.apply_action_batch(user_id, updates).await;
        for change in &action.changes {
            let entry = change.to_update_entry(&action);
            let author = state.username_of(entry.author_id).await;
            state.queue_cell_update(entry, author);
        }
    }
}

fn queue_action_for_broadcast(state: &AppState, action: &crate::state::Action, author: &str) {
    for change in &action.changes {
        state.queue_cell_update(change.to_update_entry(action), author.to_string());
    }
}

pub fn broadcast_transient_updates(
    state: &AppState,
    author_id: u32,
    updates: Vec<(WorldCoords, Option<StoredChunkCell>)>,
    author: String,
) {
    let ts = AppState::epoch_secs() as i64;

    for (pos, new_cell) in updates {
        let entry = CellUpdateEntry {
            pos,
            ch: new_cell.as_ref().map(|c| c.ch),
            color: new_cell.as_ref().map(|c| c.color).unwrap_or(Rgb(0, 0, 0)),
            ts,
            author_id,
        };

        state.queue_cell_update(entry, author.clone());
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/admin/reset-password", post(admin_reset_password))
        .route("/og/preview", get(crate::preview::og_preview_handler))
        .route("/", get(crate::preview::index_html_handler))
        .fallback_service(ServeDir::new("static"))
        .with_state(state)
}

#[derive(serde::Deserialize)]
struct ResetPasswordPayload {
    username: String,
}

async fn admin_reset_password(
    State(state): State<AppState>,
    Json(payload): Json<ResetPasswordPayload>,
) -> impl IntoResponse {
    match state.reset_password(&payload.username).await {
        Ok(()) => (StatusCode::OK, "password reset").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

pub fn extract_ip(headers: &HeaderMap) -> String {
    headers
        .get("cf-connecting-ip")
        .or_else(|| headers.get("x-real-ip"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

#[derive(serde::Deserialize)]
struct WsQuery {
    token: Option<String>,
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let ip = extract_ip(&headers);
    ws.on_upgrade(move |socket| handle_socket(socket, state, ip, query.token))
}

async fn handle_socket(
    socket: WebSocket,
    state: AppState,
    ip: String,
    session_token: Option<String>,
) {
    state.inner.connected.fetch_add(1, Ordering::Relaxed);
    let conn_id = state.inner.next_conn_id.fetch_add(1, Ordering::Relaxed);
    tracing::info!("+ {} (conn {})", ip, conn_id);

    let (mut sink, mut stream) = socket.split();
    let mut bcast = state.inner.tx.subscribe();
    let (direct_tx, mut direct_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (kick_tx, kick_rx) = tokio::sync::oneshot::channel::<()>();

    let mut session_data = None;
    if let Some(ref t) = session_token
        && let Some(uid) = state.get_session(t).await
    {
        session_data = Some((t.clone(), uid));
    }

    let (initial_name, initial_uid, anon_name, resolved_token, initial_is_anon) = match session_data
    {
        Some((tok, uid)) => {
            let acc = state.get_user_by_id(uid).await;
            let name = acc
                .as_ref()
                .map(|a| a.username.clone())
                .unwrap_or_else(|| format!("anonymous#{}", uid));
            let anon = acc
                .as_ref()
                .map(|a| a.original_anon_name.clone())
                .unwrap_or_else(|| name.clone());
            let is_anon = acc.map(|a| a.is_anonymous).unwrap_or(true);
            (name, uid, anon, tok, is_anon)
        }
        None => {
            let (name, uid, tok) = state.create_anonymous_session().await;
            (name.clone(), uid, name, tok, true)
        }
    };

    state.inner.connections.lock().unwrap().insert(
        conn_id,
        ConnInfo {
            direct_tx: direct_tx.clone(),
            kick_tx,
            viewport: None,
            cursor: None,
            color: walloftext_shared::Rgb(0, 255, 65),
            user_id: initial_uid,
            name: initial_name.clone(),
        },
    );

    let displaced_ids: Vec<u64> = {
        let conns = state.inner.connections.lock().unwrap();
        conns
            .iter()
            .filter(|&(&id, info)| id != conn_id && info.user_id == initial_uid)
            .map(|(&id, _)| id)
            .collect()
    };
    if !displaced_ids.is_empty() {
        let displaced: Vec<ConnInfo> = {
            let mut conns = state.inner.connections.lock().unwrap();
            displaced_ids
                .iter()
                .filter_map(|id| conns.remove(id))
                .collect()
        };
        for info in &displaced {
            if let Some(cursor_pos) = info.cursor {
                let msg = encode(&ServerMsg::CursorLeft {
                    user_id: info.user_id,
                });
                let conns = state.inner.connections.lock().unwrap();
                for conn in conns.values() {
                    if conn.viewport.is_none_or(|r| r.contains(cursor_pos)) {
                        let _ = conn.direct_tx.send(msg.clone());
                    }
                }
            }
        }
    }

    let _ = sink
        .send(Message::Binary(
            encode(&ServerMsg::Welcome {
                author_id: initial_uid,
                author: initial_name.clone(),
                anon_name: anon_name.clone(),
                session_token: resolved_token.clone(),
            })
            .into(),
        ))
        .await;

    if !initial_is_anon {
        let cells = state
            .get_user_by_id(initial_uid)
            .await
            .map(|u| u.protected_cells)
            .unwrap_or(0);
        let _ = sink
            .send(Message::Binary(
                encode(&ServerMsg::AuthOk {
                    username: initial_name.clone(),
                    protected_cells: cells,
                })
                .into(),
            ))
            .await;
    }

    let regions_list = state.all_regions().await;
    let mut records = Vec::with_capacity(regions_list.len());
    for r in &regions_list {
        records.push(state.region_to_record(r).await);
    }
    let _ = sink
        .send(Message::Binary(
            encode(&ServerMsg::RegionData(records)).into(),
        ))
        .await;

    let mut send_task = tokio::spawn(async move {
        let mut kick_rx = kick_rx;
        loop {
            tokio::select! {
                _ = &mut kick_rx => {
                    let _ = sink.send(Message::Binary(encode(&ServerMsg::Kicked).into())).await;
                    break;
                }
                msg = bcast.recv() => match msg {
                    Ok(m) => { if sink.send(Message::Binary(m.into())).await.is_err() { break; } }
                    Err(_) => break,
                },
                msg = direct_rx.recv() => match msg {
                    Some(m) => { if sink.send(Message::Binary(m.into())).await.is_err() { break; } }
                    None => break,
                },
            }
        }
    });

    let recv_state = state.clone();
    let mut recv_task = tokio::spawn(async move {
        let mut user_id = initial_uid;
        let mut is_anon = initial_is_anon;
        let current_token = resolved_token;
        let mut home_anon: Option<(String, u32)> = if initial_is_anon {
            Some((anon_name.clone(), initial_uid))
        } else {
            None
        };
        let anon_name = anon_name;

        while let Some(Ok(msg)) = stream.next().await {
            let bytes = match msg {
                Message::Binary(b) => b,
                Message::Text(t) => t.into(),
                Message::Close(_) => break,
                _ => continue,
            };
            let client_msg = match bitcode::decode::<ClientMsg>(&bytes) {
                Ok(m) => m,
                Err(_) => {
                    tracing::warn!("decode fail from {}", ip);
                    continue;
                }
            };

            match client_msg {
                ClientMsg::SetCellBatch { cells, commit } => {
                    if apply_set_cell_batch(&recv_state, user_id, cells, commit, &direct_tx).await {
                        continue;
                    }
                }

                ClientMsg::RestoreCellBatch { cells } => {
                    apply_restore_cell_batch(&recv_state, user_id, cells, &direct_tx).await;
                }

                ClientMsg::FetchChunkBatch { chunks } => {
                    let mut all_chunks = Vec::with_capacity(chunks.len());
                    let mut all_authors = std::collections::HashMap::new();
                    for chunk in chunks {
                        let (cells, authors) = recv_state.get_chunk_with_authors(chunk).await;
                        all_authors.extend(authors);
                        all_chunks.push((chunk, cells));
                    }
                    let _ = direct_tx.send(encode(&ServerMsg::ChunkDataBatch {
                        chunks: all_chunks,
                        authors: all_authors,
                    }));
                }

                ClientMsg::FetchLodBatch { entries } => {
                    let mut all_entries = Vec::with_capacity(entries.len());
                    for (chunk, step) in entries {
                        let cells = recv_state.get_lod_colors(chunk, step).await;
                        all_entries.push((chunk, step, cells));
                    }
                    let _ = direct_tx.send(encode(&ServerMsg::LodDataBatch {
                        entries: all_entries,
                    }));
                }

                ClientMsg::FetchStats => {
                    let total = recv_state.inner.total_cells.load(Ordering::Relaxed);
                    let online = {
                        let conns = recv_state.inner.connections.lock().unwrap();
                        conns
                            .values()
                            .map(|c| c.user_id)
                            .collect::<std::collections::HashSet<_>>()
                            .len() as u16
                    };
                    let top = recv_state.top_authors(10).await;
                    recv_state.broadcast(&ServerMsg::Stats {
                        total_cells: total,
                        online,
                        top_authors: top,
                    });
                }

                ClientMsg::AddRegion { bounds, label } => {
                    if is_anon {
                        let _ = direct_tx.send(encode(&ServerMsg::AuthFail {
                            reason: "login required".into(),
                        }));
                        continue;
                    }
                    let r = crate::state::StoredRegion {
                        id: 0,
                        bounds,
                        label: label.chars().take(32).collect(),
                        owner_id: user_id,
                    };
                    if let Ok(id) = recv_state.add_region(r).await {
                        let saved = recv_state.inner.regions.read().await.get(&id).cloned();
                        if let Some(saved) = saved {
                            recv_state.broadcast(&ServerMsg::RegionAdded(
                                recv_state.region_to_record(&saved).await,
                            ));
                        }
                    }
                }

                ClientMsg::RemoveRegion { id } => {
                    if let Ok(true) = recv_state.remove_region(id, user_id).await {
                        recv_state.broadcast(&ServerMsg::RegionRemoved { id });
                    }
                }

                ClientMsg::Login {
                    username: uname,
                    password,
                } => {
                    let uname = uname.trim().to_string();

                    let validation_err = if uname.len() < 2 || uname.len() > 24 {
                        Some("username: 2\u{2013}24 chars".to_string())
                    } else if password.len() < 4 {
                        Some("password: min 4 chars".to_string())
                    } else if let Err(e) = recv_state.check_rate_limit(&ip) {
                        Some(e.to_string())
                    } else {
                        None
                    };

                    if let Some(reason) = validation_err {
                        let _ = direct_tx.send(encode(&ServerMsg::AuthFail { reason }));
                        continue;
                    }

                    let mut was_signup = false;
                    let auth_result = match recv_state.get_user_by_name(&uname).await {
                        Some(acc) => {
                            if recv_state.verify_user(&uname, &password).await {
                                recv_state
                                    .inner
                                    .sessions
                                    .write()
                                    .await
                                    .insert(current_token.clone(), acc.user_id);

                                tracing::info!("login {} (uid {})", uname, acc.user_id);
                                Ok(acc)
                            } else {
                                Err("invalid credentials".to_string())
                            }
                        }
                        None => {
                            if is_anon {
                                was_signup = true;
                                recv_state
                                    .upgrade_to_named(&anon_name, &uname, &password, &current_token)
                                    .await
                                    .inspect(|acc| {
                                        tracing::info!(
                                            "registered {} (uid {})",
                                            uname,
                                            acc.user_id
                                        );
                                    })
                                    .map_err(|e| e.to_string())
                            } else {
                                Err("Invalid login attempt".to_string())
                            }
                        }
                    };

                    match auth_result {
                        Ok(acc) => {
                            recv_state.clear_login_throttle(&ip);
                            user_id = acc.user_id;
                            is_anon = false;
                            if was_signup {
                                home_anon = None;
                            }
                            if let Some(info) = recv_state
                                .inner
                                .connections
                                .lock()
                                .unwrap()
                                .get_mut(&conn_id)
                            {
                                info.user_id = user_id;
                                info.name = uname.clone();
                            }
                            let _ = direct_tx.send(encode(&ServerMsg::AuthOk {
                                username: uname,
                                protected_cells: acc.protected_cells,
                            }));
                        }
                        Err(reason) => {
                            recv_state.record_login_failure(&ip);
                            let _ = direct_tx.send(encode(&ServerMsg::AuthFail { reason }));
                        }
                    }
                }

                ClientMsg::Logout => {
                    if !is_anon {
                        let (name, uid) = match home_anon.clone() {
                            Some(home) => home,
                            None => {
                                let (name, uid, _) = recv_state.create_anonymous_session().await;
                                (name, uid)
                            }
                        };

                        recv_state
                            .inner
                            .sessions
                            .write()
                            .await
                            .insert(current_token.clone(), uid);

                        user_id = uid;
                        is_anon = true;
                        home_anon = Some((name.clone(), uid));

                        if let Some(info) = recv_state
                            .inner
                            .connections
                            .lock()
                            .unwrap()
                            .get_mut(&conn_id)
                        {
                            info.user_id = user_id;
                            info.name = name.clone();
                        }

                        let _ = direct_tx.send(encode(&ServerMsg::Welcome {
                            author_id: user_id,
                            author: name.clone(),
                            anon_name: name,
                            session_token: current_token.clone(),
                        }));
                    }
                }

                ClientMsg::UpdateViewport(rect) => {
                    {
                        let mut conns = recv_state.inner.connections.lock().unwrap();
                        if let Some(info) = conns.get_mut(&conn_id) {
                            info.viewport = Some(rect);
                        }
                    }
                    let online_list: Vec<OnlineUser> = {
                        let conns = recv_state.inner.connections.lock().unwrap();
                        conns
                            .iter()
                            .filter(|&(&id, _)| id != conn_id)
                            .map(|(_, c)| OnlineUser {
                                user_id: c.user_id,
                                name: c.name.clone(),
                                color: c.color,
                                pos: c.cursor,
                            })
                            .collect()
                    };
                    let _ = direct_tx.send(encode(&ServerMsg::OnlineList { users: online_list }));
                }

                ClientMsg::UpdateCursor { pos, color } => {
                    let old_pos = {
                        let mut conns = recv_state.inner.connections.lock().unwrap();
                        if let Some(info) = conns.get_mut(&conn_id) {
                            let old = info.cursor;
                            info.cursor = Some(pos);
                            info.color = color;
                            old
                        } else {
                            None
                        }
                    };
                    let my_name = recv_state.username_of(user_id).await;
                    let update_msg = encode(&ServerMsg::CursorUpdate {
                        user_id,
                        name: my_name.clone(),
                        pos,
                        color,
                    });
                    let left_msg = encode(&ServerMsg::CursorLeft { user_id });
                    let conns = recv_state.inner.connections.lock().unwrap();
                    for (&id, conn) in conns.iter() {
                        if id == conn_id {
                            continue;
                        }
                        let now_visible = conn.viewport.is_none_or(|r| r.contains(pos));
                        let was_visible =
                            old_pos.is_some_and(|p| conn.viewport.is_none_or(|r| r.contains(p)));
                        if now_visible {
                            let _ = conn.direct_tx.send(update_msg.clone());
                        } else if was_visible {
                            let _ = conn.direct_tx.send(left_msg.clone());
                        }
                    }
                }

                _ => {}
            }
        }
    });

    tokio::select! {
        _ = &mut send_task => recv_task.abort(),
        _ = &mut recv_task => send_task.abort(),
    }

    let departed = state.inner.connections.lock().unwrap().remove(&conn_id);
    if let Some(info) = departed {
        let msg = encode(&ServerMsg::CursorLeft {
            user_id: info.user_id,
        });
        let conns = state.inner.connections.lock().unwrap();
        for conn in conns.values() {
            let _ = conn.direct_tx.send(msg.clone());
        }
    }

    state.inner.connected.fetch_sub(1, Ordering::Relaxed);
}
