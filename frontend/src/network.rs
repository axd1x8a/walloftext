use std::io::Read;

use ruzstd::decoding::StreamingDecoder;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{MessageEvent, WebSocket};

use walloftext_shared::{CellUpdateEntry, ChunkCoords, ClientMsg, ServerMsg, WorldCoords};

use crate::chunks::LodChunkData;
use crate::chunks::{ChunkData, MAX_FLUSH, refresh_chunks};
use crate::dom::{El, coord_span, debug_log, log, log_el, sb_msg, set_timeout, window};
use crate::render::schedule_render;
use crate::state::{
    AuthState, CanvasCell, dirty, mark_dirty, read_board, read_net, with_board, with_net, with_ui,
};
use crate::widgets::render_all;

pub fn zstd_decompress(data: &[u8]) -> Vec<u8> {
    if let Ok(mut dec) = StreamingDecoder::new(data) {
        let mut out = Vec::new();
        if dec.read_to_end(&mut out).is_ok() {
            return out;
        }
    }
    data.to_vec()
}

fn update_cell_data(
    pos: WorldCoords,
    ch: Option<char>,
    color: walloftext_shared::Rgb,
    ts: i64,
    author_id: u32,
) -> ChunkCoords {
    with_board(|b| {
        b.prev_cells.remove(&pos);
        b.chunk_manager.set_cell(
            pos,
            ch.map(|ch| CanvasCell::from_update(ch, color, author_id, ts)),
        );
    });
    let (chunk, _) = pos.to_chunk();
    chunk
}

fn apply_cell_update(
    pos: WorldCoords,
    ch: Option<char>,
    color: walloftext_shared::Rgb,
    ts: i64,
    author_id: u32,
) {
    let chunk = update_cell_data(pos, ch, color, ts, author_id);
    crate::render::build_chunk_buffer(chunk);
}

fn contiguous_word(group: &[CellUpdateEntry]) -> Option<String> {
    let y = group[0].pos.y;
    let x0 = group[0].pos.x;
    for (k, u) in group.iter().enumerate() {
        if u.pos.y != y || u.pos.x != x0 + k as i16 {
            return None;
        }
    }
    group.iter().map(|u| u.ch).collect::<Option<String>>()
}

fn log_cell_updates(updates: &[CellUpdateEntry]) {
    for group in updates.chunk_by(|a, b| a.author_id == b.author_id) {
        let author_id = group[0].author_id;
        if author_id != 0 {
            let author = read_board(|b| b.author_name(author_id));
            let first = &group[0];
            let msg = if let [u] = group {
                El::span()
                    .add(El::span().text(&format!("{} '{}' ", author, u.ch.unwrap_or(' '))))
                    .add(coord_span(u.pos))
            } else if let Some(word) = contiguous_word(group) {
                El::span()
                    .add(El::span().text(&format!("{} \"{}\" ", author, word)))
                    .add(coord_span(first.pos))
            } else {
                El::span()
                    .add(El::span().text(&format!("{} wrote {} cells ", author, group.len())))
                    .add(coord_span(first.pos))
            };
            log_el(msg, "w");
        }
    }
}

fn build_chunk_data(cells: &[walloftext_shared::ChunkCell]) -> ChunkData {
    let mut data = ChunkData::new_empty();
    for c in cells {
        let idx = c.local.to_index();
        data.chars[idx] = c.ch;
        data.colors[idx] = c.color;
        data.authors[idx] = c.author_id;
        data.timestamps[idx] = c.ts;
        data.set_occupied(idx, true);
    }
    data
}

pub fn handle_server_msg(msg: ServerMsg) {
    match msg {
        ServerMsg::Welcome {
            author_id,
            author,
            anon_name,
            session_token,
        } => {
            debug_log!("recv Welcome: author={} id={}", author, author_id);
            if let Some(storage) = window().local_storage().ok().flatten() {
                let _ = storage.set_item("walloftext_session_token", &session_token);
            }
            let is_registered = author != anon_name;
            with_ui(|ui| {
                ui.my_author = author.clone();
                ui.my_anon_name = anon_name.clone();
                ui.my_id = author_id;
                if is_registered {
                    ui.auth = AuthState::LoggedIn {
                        username: author.clone(),
                        protected_cells: 0,
                    };
                }
            });
            with_board(|b| {
                b.author_names.insert(author_id, author.clone());
            });
            render_all();
            log(&format!("you are {}", author), "w");
        }

        ServerMsg::CellUpdate { entry, author } => {
            debug_log!(
                "recv CellUpdate ({},{}) '{:?}' by {}",
                entry.pos.x,
                entry.pos.y,
                entry.ch,
                author
            );
            with_board(|b| {
                b.author_names.insert(entry.author_id, author);
            });
            apply_cell_update(entry.pos, entry.ch, entry.color, entry.ts, entry.author_id);
            log_cell_updates(std::slice::from_ref(&entry));
            schedule_render();
        }

        ServerMsg::CellUpdateBatch { updates, authors } => {
            debug_log!("recv CellUpdateBatch: {} update(s)", updates.len());
            with_board(|b| {
                for (id, name) in authors {
                    b.author_names.insert(id, name);
                }
            });
            let mut dirty: Vec<ChunkCoords> = Vec::new();
            for u in &updates {
                let chunk = update_cell_data(u.pos, u.ch, u.color, u.ts, u.author_id);
                if !dirty.contains(&chunk) {
                    dirty.push(chunk);
                }
            }
            for chunk in dirty {
                crate::render::build_chunk_buffer(chunk);
            }
            log_cell_updates(&updates);
            schedule_render();
        }

        ServerMsg::ChunkDataBatch { chunks, authors } => {
            debug_log!("recv ChunkDataBatch: {} chunk(s)", chunks.len());
            with_board(|b| {
                b.author_names.extend(authors);
                for (chunk, cells) in &chunks {
                    let mut chunk_data = build_chunk_data(cells);
                    for pending in read_net(|n| n.pending_sets.clone()).iter() {
                        let (c, local) = pending.pos.to_chunk();
                        if c == *chunk {
                            let idx = local.to_index();
                            chunk_data.set_occupied(idx, pending.ch.is_some());
                            if let Some(ch) = pending.ch {
                                chunk_data.chars[idx] = ch;
                                chunk_data.colors[idx] = pending.color;
                            }
                        }
                    }
                    b.chunk_manager.on_chunk_received(*chunk, chunk_data);
                    b.chunk_manager.schedule_flush_debounced(0);
                }
            });
            for (chunk, _) in &chunks {
                crate::render::build_chunk_buffer(*chunk);
            }
            schedule_render();
        }

        ServerMsg::LodDataBatch { entries } => {
            debug_log!("recv LodDataBatch: {} chunk(s)", entries.len());
            with_board(|b| {
                for (chunk, step, cells) in &entries {
                    let mut lod_data = LodChunkData::new_empty();
                    for c in cells {
                        let idx = c.local.to_index();
                        lod_data.colors[idx] = c.color;
                        lod_data.set_occupied(idx, true);
                    }
                    b.chunk_manager.on_lod_received(*chunk, *step, lod_data);
                    b.chunk_manager.schedule_flush_debounced(0);
                }
            });
            schedule_render();
        }

        ServerMsg::Stats {
            total_cells,
            online,
            top_authors,
        } => {
            debug_log!("recv Stats total={} online={}", total_cells, online);
            with_board(|b| b.last_stats = Some((total_cells, online, top_authors)));
            mark_dirty(dirty::STATS | dirty::STATUSBAR);
            schedule_render();
        }

        ServerMsg::RegionData(regions) => {
            debug_log!("recv RegionData count={}", regions.len());
            with_board(|b| {
                b.regions.clear();
                for r in regions {
                    b.regions.insert(r.id, r);
                }
            });
            mark_dirty(dirty::REGIONS);
            schedule_render();
        }

        ServerMsg::RegionAdded(r) => {
            let label = r.label.clone();
            with_board(|b| {
                b.regions.insert(r.id, r);
            });
            log(&format!("region \"{}\" protected", label), "r");
            mark_dirty(dirty::REGIONS);
            schedule_render();
        }

        ServerMsg::RegionRemoved { id } => {
            with_board(|b| {
                b.regions.remove(&id);
            });
            mark_dirty(dirty::REGIONS);
            schedule_render();
        }

        ServerMsg::Denied { pos, reason } => {
            with_board(|b| {
                if let Some(prev) = b.prev_cells.remove(&pos) {
                    b.chunk_manager.set_cell(pos, prev);
                }
                b.flash_deny.insert(pos, crate::dom::perf() + 700.0);
            });
            log(&format!("X {}", reason), "d");
            sb_msg(&format!("X {}", reason));
            schedule_render();
        }

        ServerMsg::DeniedBatch { positions, reason } => {
            let until = crate::dom::perf() + 700.0;
            let mut dirty_chunks = crate::FoldHashSet::default();
            let denied: crate::FoldHashSet<_> = positions.iter().copied().collect();
            with_board(|b| {
                for pos in positions {
                    if let Some(prev) = b.prev_cells.remove(&pos) {
                        b.chunk_manager.set_cell(pos, prev);
                    }
                    b.flash_deny.insert(pos, until);
                    dirty_chunks.insert(pos.to_chunk().0);
                }
            });
            with_ui(|ui| {
                for batch in &mut ui.undo_stack {
                    batch.retain(|(pos, _)| !denied.contains(pos));
                }
                ui.undo_stack.retain(|batch| !batch.is_empty());
            });
            for chunk in dirty_chunks {
                crate::render::build_chunk_buffer(chunk);
            }
            log(&format!("X {}", reason), "d");
            sb_msg(&format!("X {}", reason));
            schedule_render();
        }

        ServerMsg::AuthOk {
            username,
            protected_cells,
        } => {
            let my_id = crate::state::read_ui(|ui| ui.my_id);
            with_ui(|ui| {
                ui.my_author = username.clone();
                ui.auth = AuthState::LoggedIn {
                    username: username.clone(),
                    protected_cells,
                };
            });
            with_board(|b| {
                b.author_names.insert(my_id, username.clone());
            });
            mark_dirty(dirty::AUTH | dirty::STATUSBAR);
            schedule_render();
            log(&format!("logged in as {}", username), "c");
        }

        ServerMsg::AuthFail { reason } => {
            sb_msg(&format!("X {}", reason));
            log(&format!("ERR: {}", reason), "e");
            El::from_id("auth-anon")
                .map(|el| el.text(&format!("X {}", reason)).css("color", "#aa2222"));
        }

        ServerMsg::Error(msg) => {
            log(&format!("err: {}", msg), "e");
        }

        ServerMsg::CursorUpdate {
            user_id,
            name,
            pos,
            color,
        } => {
            with_board(|b| {
                b.author_names.insert(user_id, name.clone());
                b.online_users.insert(user_id, (name, color));
                b.remote_cursors.insert(user_id, (pos, color));
            });
            mark_dirty(dirty::ONLINE);
            schedule_render();
        }

        ServerMsg::CursorLeft { user_id } => {
            with_board(|b| {
                b.online_users.remove(&user_id);
                b.remote_cursors.remove(&user_id);
            });
            mark_dirty(dirty::ONLINE);
            schedule_render();
        }

        ServerMsg::OnlineList { users } => {
            with_board(|b| {
                b.online_users.clear();
                b.remote_cursors.clear();
                for u in users {
                    b.author_names.insert(u.user_id, u.name.clone());
                    b.online_users.insert(u.user_id, (u.name, u.color));
                    if let Some(pos) = u.pos {
                        b.remote_cursors.insert(u.user_id, (pos, u.color));
                    }
                }
            });
            mark_dirty(dirty::ONLINE);
            schedule_render();
        }

        ServerMsg::Kicked => {
            with_net(|n| n.kicked = true);
            El::from_id("overlay-msg").map(|el| el.text("This tab was disconnected because another tab connected with the same account."));
            El::from_id("kicked-reconnect").map(|el| el.css("display", "block"));
            El::from_id("overlay").map(|el| el.css("display", "flex"));
        }
    }
}

pub fn init_worker_bridge(worker: web_sys::Worker) {
    let on_msg = Closure::<dyn FnMut(_)>::new(|e: MessageEvent| {
        if let Ok(arr) = e.data().dyn_into::<js_sys::Uint8Array>() {
            match bitcode::decode::<ServerMsg>(&arr.to_vec()) {
                Ok(msg) => handle_server_msg(msg),
                Err(err) => debug_log!("worker decode error: {}", err),
            }
        }
    });
    worker.set_onmessage(Some(on_msg.as_ref().unchecked_ref()));
    on_msg.forget();
    with_net(|n| n.worker = Some(worker));
}

pub fn connect() {
    let loc = window().location();
    let proto = if loc.protocol().unwrap_or_default() == "https:" {
        "wss"
    } else {
        "ws"
    };
    let host = loc.host().unwrap_or_default();
    let token = window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|s| s.get_item("walloftext_session_token").ok().flatten())
        .unwrap_or_default();
    let url = if token.is_empty() {
        format!("{}://{}/ws", proto, host)
    } else {
        format!("{}://{}/ws?token={}", proto, host, token)
    };
    debug_log!("connecting to {}://{}", proto, host);

    let ws = WebSocket::new(&url).unwrap();
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

    let cb = Closure::<dyn FnMut(_)>::new(|_: web_sys::Event| {
        with_net(|n| {
            n.is_conn = true;
            n.send(&ClientMsg::FetchStats);
        });
        read_net(|n| {
            n.send(&ClientMsg::UpdateViewport(
                crate::state::world_viewport_rect(),
            ))
        });
        let pos = crate::state::read_ui(|ui| ui.cursor.pos);
        let color = crate::state::read_ui(|ui| ui.color);
        with_net(|n| n.last_cursor_sent = Some(pos));
        read_net(|n| n.send(&ClientMsg::UpdateCursor { pos, color }));
        refresh_chunks(MAX_FLUSH);
        log("connected", "c");
        sb_msg("type anywhere | ? for help");
    });
    ws.set_onopen(Some(cb.as_ref().unchecked_ref()));
    cb.forget();

    let cb = Closure::<dyn FnMut(_)>::new(|e: MessageEvent| {
        let buf: js_sys::ArrayBuffer = match e.data().dyn_into() {
            Ok(b) => b,
            Err(_) => return,
        };
        read_net(|n| {
            if let Some(worker) = &n.worker {
                let transfer = js_sys::Array::of1(&buf);
                let _ = worker.post_message_with_transfer(&buf.into(), &transfer);
            }
        });
    });
    ws.set_onmessage(Some(cb.as_ref().unchecked_ref()));
    cb.forget();

    let cb = Closure::<dyn FnMut(_)>::new(|_: web_sys::CloseEvent| {
        let kicked = read_net(|n| n.kicked);
        with_net(|n| {
            n.is_conn = false;
            n.last_cursor_sent = None;
        });
        with_board(|b| b.chunk_manager.on_disconnect());
        if !kicked {
            log("disconnected; retrying...", "e");
            set_timeout(3000, connect);
        }
    });
    ws.set_onclose(Some(cb.as_ref().unchecked_ref()));
    cb.forget();

    with_net(|n| n.ws = Some(ws));
}
