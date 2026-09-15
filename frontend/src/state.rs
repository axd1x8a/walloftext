use crate::FoldHashMap;
use std::cell::{Cell, RefCell};

use js_sys::Date;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{HtmlCanvasElement, WebSocket};

use walloftext_shared::{
    AuthorStat, BOARD_HALF, CHUNK_H, CHUNK_W, CellWrite, ChunkCoords, ClientMsg, RegionRecord,
    RestoreCoord, Rgb, WorldCoords, WorldRect, clamp_coord,
};

use crate::chunks::ChunkManager;
use crate::dom::{debug_log, random_vivid_color};
use crate::render::LOD_GLYPH_THRESHOLD;

pub const CELL_W: f64 = 8.0;
pub const CELL_H: f64 = 16.0;

pub mod dirty {
    pub const STATUSBAR: u32 = 1 << 0;
    pub const SEL_INFO: u32 = 1 << 1;
    pub const POPUP: u32 = 1 << 2;
    pub const STATS: u32 = 1 << 3;
    pub const REGIONS: u32 = 1 << 4;
    pub const AUTH: u32 = 1 << 5;
    pub const LOG: u32 = 1 << 6;
    pub const SNAP: u32 = 1 << 7;
    pub const COLOR: u32 = 1 << 8;
    pub const ONLINE: u32 = 1 << 9;
    pub const ALL: u32 = !0u32;
}

#[derive(Clone)]
pub enum HoveredLink {
    Coord(WorldCoords),
    Url(String),
}

#[derive(Clone)]
pub enum AuthState {
    Anonymous,
    LoggedIn {
        username: String,
        protected_cells: u32,
    },
}

#[derive(Clone)]
pub struct CanvasCell {
    pub ch: char,
    pub color: Rgb,
    pub author_id: u32,
    pub ts: i64,
}

impl CanvasCell {
    pub fn from_update(ch: char, color: Rgb, author_id: u32, ts: i64) -> Self {
        Self {
            ch,
            color,
            author_id,
            ts,
        }
    }

    pub fn typed(ch: char) -> Option<Self> {
        if ch == ' ' {
            return None;
        }
        let (color, author_id) = read_ui(|ui| (ui.color, ui.my_id));
        Some(Self {
            ch,
            color,
            author_id,
            ts: (Date::now() / 1000.0) as i64,
        })
    }
}

pub struct ViewportState {
    pub canvas: Option<HtmlCanvasElement>,
    pub log_w: f64,
    pub log_h: f64,
    pub dpr: f64,
    pub zoom: f64,
    pub cw: f64,
    pub ch: f64,
    pub vx: f64,
    pub vy: f64,

    pub is_pan: bool,
    pub pan_sx: f64,
    pub pan_sy: f64,
    pub pan_ox: f64,
    pub pan_oy: f64,

    pub touch_count: u32,
    pub touch_start_x: f64,
    pub touch_start_y: f64,
    pub touch_start_vx: f64,
    pub touch_start_vy: f64,
    pub touch_start_t: f64,
    pub touch_moved: bool,
    pub pinch_start_dist: f64,
    pub pinch_start_zoom: f64,
    pub pinch_start_vx: f64,
    pub pinch_start_vy: f64,
    pub touch_vel_x: f64,
    pub touch_vel_y: f64,
    pub touch_last_x: f64,
    pub touch_last_y: f64,
    pub touch_last_t: f64,

    pub hover: Option<WorldCoords>,
    pub hover_sx: f64,
    pub hover_sy: f64,
    pub hover_above: bool,
    pub hovered_link: Option<HoveredLink>,
}

impl Default for ViewportState {
    fn default() -> Self {
        Self {
            canvas: None,
            log_w: 800.0,
            log_h: 600.0,
            dpr: 1.0,
            zoom: 1.0,
            cw: CELL_W,
            ch: CELL_H,
            vx: 0.0,
            vy: 0.0,
            is_pan: false,
            pan_sx: 0.0,
            pan_sy: 0.0,
            pan_ox: 0.0,
            pan_oy: 0.0,
            touch_count: 0,
            touch_start_x: 0.0,
            touch_start_y: 0.0,
            touch_start_vx: 0.0,
            touch_start_vy: 0.0,
            touch_start_t: 0.0,
            touch_moved: false,
            pinch_start_dist: 0.0,
            pinch_start_zoom: 1.0,
            pinch_start_vx: 0.0,
            pinch_start_vy: 0.0,
            touch_vel_x: 0.0,
            touch_vel_y: 0.0,
            touch_last_x: 0.0,
            touch_last_y: 0.0,
            touch_last_t: 0.0,
            hover: None,
            hover_sx: 0.0,
            hover_sy: 0.0,
            hover_above: false,
            hovered_link: None,
        }
    }
}

impl ViewportState {
    pub fn log_w(&self) -> f64 {
        self.log_w
    }

    pub fn log_h(&self) -> f64 {
        self.log_h
    }

    pub fn visible_world_bounds(&self) -> WorldRect {
        const MIN_B: i64 = -BOARD_HALF;
        const MAX_B: i64 = BOARD_HALF - 1;
        WorldRect::new(
            WorldCoords::from_i64(
                ((self.vx / self.cw).floor() as i64).clamp(MIN_B, MAX_B),
                ((self.vy / self.ch).floor() as i64).clamp(MIN_B, MAX_B),
            ),
            WorldCoords::from_i64(
                (((self.vx + self.log_w) / self.cw).ceil() as i64).clamp(MIN_B, MAX_B),
                (((self.vy + self.log_h) / self.ch).ceil() as i64).clamp(MIN_B, MAX_B),
            ),
        )
    }

    pub fn screen_to_cell(&self, sx: f64, sy: f64) -> WorldCoords {
        WorldCoords {
            x: clamp_coord(((sx + self.vx) / self.cw).floor() as i64),
            y: clamp_coord(((sy + self.vy) / self.ch).floor() as i64),
        }
    }

    pub fn clamp_viewport(&mut self) {
        const VISIBLE: f64 = 0.2;
        let (w, h) = (self.log_w(), self.log_h());
        let bx0 = -BOARD_HALF as f64 * self.cw;
        let bx1 = BOARD_HALF as f64 * self.cw;
        let by0 = -BOARD_HALF as f64 * self.ch;
        let by1 = BOARD_HALF as f64 * self.ch;

        let lo_x = bx0 - (1.0 - VISIBLE) * w;
        let hi_x = bx1 - VISIBLE * w;
        self.vx = if lo_x <= hi_x {
            self.vx.clamp(lo_x, hi_x)
        } else {
            (bx0 + bx1 - w) / 2.0
        };

        let lo_y = by0 - (1.0 - VISIBLE) * h;
        let hi_y = by1 - VISIBLE * h;
        self.vy = if lo_y <= hi_y {
            self.vy.clamp(lo_y, hi_y)
        } else {
            (by0 + by1 - h) / 2.0
        };
    }

    pub fn apply_zoom_inner(&mut self, z: f64, px: f64, py: f64) {
        let (wpx, wpy) = ((px + self.vx) / self.cw, (py + self.vy) / self.ch);
        self.zoom = z.clamp(0.03125, 8.0);
        self.cw = CELL_W * self.zoom;
        self.ch = CELL_H * self.zoom;
        self.vx = wpx * self.cw - px;
        self.vy = wpy * self.ch - py;
        self.clamp_viewport();
    }

    pub fn zoom_step_inner(&mut self, dir: i32, px: f64, py: f64) {
        const S: &[f64] = &[
            0.03125, 0.0625, 0.125, 0.25, 0.33, 0.5, 0.67, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0, 8.0,
        ];
        let cur = S
            .iter()
            .copied()
            .min_by(|a, b| {
                (a - self.zoom)
                    .abs()
                    .partial_cmp(&(b - self.zoom).abs())
                    .unwrap()
            })
            .unwrap_or(1.0);
        let i = S.iter().position(|&s| s == cur).unwrap_or(5) as i32;
        self.apply_zoom_inner(S[(i + dir).max(0).min(S.len() as i32 - 1) as usize], px, py);
    }
}

pub struct BoardState {
    pub chunk_manager: ChunkManager,
    pub regions: FoldHashMap<u32, RegionRecord>,
    pub author_names: FoldHashMap<u32, String>,
    pub last_stats: Option<(u32, u16, Vec<AuthorStat>)>,
    pub prev_cells: FoldHashMap<WorldCoords, Option<CanvasCell>>,
    pub flash_deny: FoldHashMap<WorldCoords, f64>,
    pub online_users: FoldHashMap<u32, (String, walloftext_shared::Rgb)>,
    pub remote_cursors: FoldHashMap<u32, (WorldCoords, walloftext_shared::Rgb)>,
}

impl Default for BoardState {
    fn default() -> Self {
        Self {
            chunk_manager: ChunkManager::new(),
            regions: FoldHashMap::default(),
            author_names: FoldHashMap::default(),
            last_stats: None,
            prev_cells: FoldHashMap::default(),
            flash_deny: FoldHashMap::default(),
            online_users: FoldHashMap::default(),
            remote_cursors: FoldHashMap::default(),
        }
    }
}

impl BoardState {
    pub fn author_name(&self, id: u32) -> String {
        self.author_names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("anon#{}", id))
    }
}

pub struct Selection {
    pub active: bool,
    pub drag: bool,
    pub span: WorldRect,
}

impl Default for Selection {
    fn default() -> Self {
        let zero = WorldCoords { x: 0, y: 0 };
        Self {
            active: false,
            drag: false,
            span: WorldRect::new(zero, zero),
        }
    }
}

impl Selection {
    pub fn clear(&mut self) {
        self.active = false;
        self.drag = false;
    }

    pub fn extend(&mut self, anchor: WorldCoords, end: WorldCoords) {
        let anchor = if self.active { self.span.a } else { anchor };
        self.active = true;
        self.span = WorldRect::new(anchor, end);
    }

    pub fn drag_to(&mut self, end: WorldCoords) {
        self.span.b = end;
    }

    pub fn end_drag(&mut self, end: WorldCoords) {
        self.drag = false;
        self.span.b = end;
    }

    pub fn rect(&self) -> WorldRect {
        WorldRect::from_corners(self.span.a, self.span.b)
    }
}

pub struct CursorState {
    pub pos: WorldCoords,
    pub sel: Selection,
    pub type_start_x: i16,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            pos: WorldCoords { x: 0, y: 0 },
            sel: Selection::default(),
            type_start_x: 0,
        }
    }
}

impl CursorState {
    pub fn move_to(&mut self, pos: WorldCoords, color: Rgb) {
        self.pos = pos;
        self.type_start_x = pos.x;
        self.sel.clear();
        send_cursor_if_changed(pos, color);
    }

    pub fn type_move_to(&mut self, pos: WorldCoords, color: Rgb) {
        self.pos = pos;
        self.sel.clear();
        send_cursor_if_changed(pos, color);
    }

    pub fn select_to(&mut self, pos: WorldCoords, color: Rgb) {
        let anchor = self.pos;
        self.sel.extend(anchor, pos);
        self.pos = pos;
        self.type_start_x = pos.x;
        send_cursor_if_changed(pos, color);
    }

    pub fn drag_sel_to(&mut self, pos: WorldCoords, color: Rgb) {
        self.sel.drag_to(pos);
        self.pos = pos;
        self.type_start_x = pos.x;
        send_cursor_if_changed(pos, color);
    }
}

fn send_cursor_if_changed(pos: WorldCoords, color: Rgb) {
    let changed = NET.with(|n| n.borrow().last_cursor_sent != Some(pos));
    if changed {
        NET.with(|n| n.borrow_mut().last_cursor_sent = Some(pos));
        let msg = ClientMsg::UpdateCursor { pos, color };
        NET.with(|n| n.borrow().send(&msg));
    }
}

pub type UndoEntry = (WorldCoords, Option<CanvasCell>);

pub struct UIState {
    pub cursor: CursorState,
    pub color: Rgb,

    pub my_id: u32,
    pub my_author: String,
    pub my_anon_name: String,
    pub auth: AuthState,

    pub undo_stack: Vec<Vec<UndoEntry>>,
    pub undo_batch: Option<Vec<UndoEntry>>,

    pub bookmarks: Vec<(String, i64, i64)>,
}

impl Default for UIState {
    fn default() -> Self {
        Self {
            cursor: CursorState::default(),
            color: random_vivid_color(),
            my_id: 0,
            my_author: "anon".into(),
            my_anon_name: "anon".into(),
            auth: AuthState::Anonymous,
            undo_stack: Vec::new(),
            undo_batch: None,
            bookmarks: vec![],
        }
    }
}

impl UIState {
    const UNDO_LIMIT: usize = 500;

    pub fn push_undo(&mut self, pos: WorldCoords, prev: Option<CanvasCell>) {
        if let Some(batch) = self.undo_batch.as_mut() {
            batch.push((pos, prev));
            return;
        }
        self.undo_stack.push(vec![(pos, prev)]);
        if self.undo_stack.len() > Self::UNDO_LIMIT {
            self.undo_stack.remove(0);
        }
    }

    pub fn begin_undo_batch(&mut self) {
        self.undo_batch = Some(Vec::new());
    }

    pub fn end_undo_batch(&mut self) {
        if let Some(batch) = self.undo_batch.take()
            && !batch.is_empty()
        {
            self.undo_stack.push(batch);
            if self.undo_stack.len() > Self::UNDO_LIMIT {
                self.undo_stack.remove(0);
            }
        }
    }
}

pub struct NetState {
    pub ws: Option<WebSocket>,
    pub worker: Option<web_sys::Worker>,
    pub is_conn: bool,
    pub kicked: bool,
    pub pending_sets: Vec<CellWrite>,
    set_flush_timer: i32,
    viewport_flush_timer: i32,
    pub last_cursor_sent: Option<WorldCoords>,
    anchor_flush_timer: i32,
}

impl Default for NetState {
    fn default() -> Self {
        Self {
            ws: None,
            worker: None,
            is_conn: false,
            kicked: false,
            pending_sets: Vec::new(),
            set_flush_timer: -1,
            viewport_flush_timer: -1,
            last_cursor_sent: None,
            anchor_flush_timer: -1,
        }
    }
}

impl NetState {
    const SET_FLUSH_PERIOD_MS: i32 = 100;

    pub fn send(&self, msg: &ClientMsg) {
        if let Some(ws) = &self.ws
            && ws.ready_state() == WebSocket::OPEN
        {
            let _ = ws.send_with_u8_array(&bitcode::encode(msg));
        }
    }

    pub fn queue_set_cell(&mut self, pos: WorldCoords, cell: Option<CanvasCell>) {
        self.pending_sets.push(CellWrite {
            pos,
            ch: cell.as_ref().map(|c| c.ch),
            color: cell.as_ref().map_or(Rgb(0, 0, 0), |c| c.color),
        });
        if self.set_flush_timer == -1 {
            let win = crate::dom::window();
            let cb = wasm_bindgen::closure::Closure::once(flush_pending_sets);
            self.set_flush_timer = win
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    cb.as_ref().unchecked_ref(),
                    Self::SET_FLUSH_PERIOD_MS,
                )
                .unwrap_or(-1);
            cb.forget();
        }
    }

    pub fn flush(&mut self) {
        self.set_flush_timer = -1;
        if self.pending_sets.is_empty() {
            return;
        }
        let cells = std::mem::take(&mut self.pending_sets);
        debug_log!("flush_pending_sets: sending {} cell write(s)", cells.len());
        self.send(&ClientMsg::SetCellBatch {
            cells,
            commit: true,
        });
    }
}

thread_local! {
    pub static VIEWPORT:     RefCell<ViewportState> = RefCell::new(ViewportState::default());
    pub static BOARD:        RefCell<BoardState>    = RefCell::new(BoardState::default());
    pub static UI:           RefCell<UIState>       = RefCell::new(UIState::default());
    pub static NET:          RefCell<NetState>      = RefCell::new(NetState::default());
    pub static DIRTY_BITS:   Cell<u32>              = const { Cell::new(!0u32) };
    pub static CANVAS_DIRTY: Cell<bool>             = const { Cell::new(true) };
}

pub fn mark_dirty(bits: u32) {
    DIRTY_BITS.with(|d| d.set(d.get() | bits));
}

pub fn mark_canvas_dirty() {
    CANVAS_DIRTY.with(|d| d.set(true));
}

pub fn with_vp<R>(f: impl FnOnce(&mut ViewportState) -> R) -> R {
    VIEWPORT.with(|s| f(&mut s.borrow_mut()))
}

pub fn read_vp<R>(f: impl FnOnce(&ViewportState) -> R) -> R {
    VIEWPORT.with(|s| f(&s.borrow()))
}

pub fn with_board<R>(f: impl FnOnce(&mut BoardState) -> R) -> R {
    BOARD.with(|s| f(&mut s.borrow_mut()))
}

pub fn read_board<R>(f: impl FnOnce(&BoardState) -> R) -> R {
    BOARD.with(|s| f(&s.borrow()))
}

pub fn with_ui<R>(f: impl FnOnce(&mut UIState) -> R) -> R {
    UI.with(|s| f(&mut s.borrow_mut()))
}

pub fn read_ui<R>(f: impl FnOnce(&UIState) -> R) -> R {
    UI.with(|s| f(&s.borrow()))
}

pub fn with_net<R>(f: impl FnOnce(&mut NetState) -> R) -> R {
    NET.with(|s| f(&mut s.borrow_mut()))
}

pub fn read_net<R>(f: impl FnOnce(&NetState) -> R) -> R {
    NET.with(|s| f(&s.borrow()))
}

pub fn send_msg(msg: &ClientMsg) {
    read_net(|n| n.send(msg));
}

pub fn flush_pending_sets() {
    with_net(|n| n.flush());
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DevicePt {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DeviceSize {
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Copy)]
pub struct DeviceRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl DeviceRect {
    #[inline]
    pub fn from_pt_size(pt: DevicePt, size: DeviceSize) -> Self {
        Self {
            x: pt.x,
            y: pt.y,
            w: size.w,
            h: size.h,
        }
    }

    #[inline]
    pub fn intersects(&self, other: &DeviceRect) -> bool {
        self.x < other.x + other.w
            && self.x + self.w > other.x
            && self.y < other.y + other.h
            && self.y + self.h > other.y
    }
}

pub fn world_viewport_rect() -> WorldRect {
    read_vp(|vp| vp.visible_world_bounds())
}

pub fn flush_viewport_update() {
    let bounds = world_viewport_rect();
    read_net(|n| n.send(&ClientMsg::UpdateViewport(bounds)));
    with_net(|n| n.viewport_flush_timer = -1);
    queue_anchor_update();
}

pub fn queue_viewport_update() {
    with_net(|n| {
        if n.viewport_flush_timer == -1 {
            let win = crate::dom::window();
            let cb = wasm_bindgen::closure::Closure::once(flush_viewport_update);
            n.viewport_flush_timer = win
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    cb.as_ref().unchecked_ref(),
                    150,
                )
                .unwrap_or(-1);
            cb.forget();
        }
    });
}

pub fn flush_anchor_update() {
    with_net(|n| n.anchor_flush_timer = -1);
    let (cx, cy, zoom) = read_vp(|vp| {
        let cx = ((vp.vx + vp.log_w / 2.0) / vp.cw).round() as i32;
        let cy = ((vp.vy + vp.log_h / 2.0) / vp.ch).round() as i32;
        (cx, cy, vp.zoom)
    });
    let zoom_pct = (zoom * 100.0).round() as i32;
    let hash = format!("?at={},{},{}", cx, cy, zoom_pct);
    let _ = crate::dom::window().history().ok().and_then(|h| {
        h.replace_state_with_url(&JsValue::NULL, "", Some(&hash))
            .ok()
    });
}

pub fn queue_anchor_update() {
    with_net(|n| {
        if n.anchor_flush_timer == -1 {
            let win = crate::dom::window();
            let cb = wasm_bindgen::closure::Closure::once(flush_anchor_update);
            n.anchor_flush_timer = win
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    cb.as_ref().unchecked_ref(),
                    500,
                )
                .unwrap_or(-1);
            cb.forget();
        }
    });
}

pub fn apply_anchor_viewport() {
    let search = crate::dom::window().location().search().unwrap_or_default();
    let raw = search
        .trim_start_matches('?')
        .split('&')
        .find_map(|kv| kv.strip_prefix("at=").map(str::to_owned))
        .unwrap_or_default();
    if raw.is_empty() {
        return;
    }
    let parts: Vec<&str> = raw.split(',').collect();
    if parts.len() < 2 {
        return;
    }
    let (Ok(x), Ok(y)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) else {
        return;
    };
    let zoom = parts
        .get(2)
        .and_then(|s| s.parse::<f64>().ok())
        .map(|pct| pct / 100.0)
        .unwrap_or(1.0)
        .clamp(0.03125, 8.0);
    with_vp(|vp| {
        vp.zoom = zoom;
        vp.cw = CELL_W * zoom;
        vp.ch = CELL_H * zoom;
        vp.vx = x as f64 * vp.cw - vp.log_w / 2.0;
        vp.vy = y as f64 * vp.ch - vp.log_h / 2.0;
        vp.clamp_viewport();
    });
}

pub fn resend_cursor() {
    let pos = read_ui(|ui| ui.cursor.pos);
    let color = read_ui(|ui| ui.color);
    read_net(|n| n.send(&ClientMsg::UpdateCursor { pos, color }));
}

pub fn write_cell(pos: WorldCoords, cell: Option<CanvasCell>) {
    let prev = read_board(|b| b.chunk_manager.get_cell(pos));
    with_board(|b| b.prev_cells.insert(pos, prev.clone()));
    with_ui(|ui| ui.push_undo(pos, prev));
    with_net(|n| n.queue_set_cell(pos, cell.clone()));
    with_board(|b| b.chunk_manager.set_cell(pos, cell));
}

pub fn undo() {
    let batch = with_ui(|ui| ui.undo_stack.pop());
    let Some(batch) = batch else {
        debug_log!("undo: nothing to undo");
        return;
    };
    debug_log!("undo: reverting {} cell(s)", batch.len());
    let mut coords: Vec<WorldCoords> = Vec::with_capacity(batch.len());
    for (pos, prev) in batch.into_iter().rev() {
        with_board(|b| {
            let cur = b.chunk_manager.get_cell(pos);
            b.prev_cells.insert(pos, cur);
            b.chunk_manager.set_cell(pos, prev);
        });
        coords.push(pos);
    }
    if !coords.is_empty() {
        send_msg(&ClientMsg::RestoreCellBatch {
            cells: coords.iter().map(|&pos| RestoreCoord { pos }).collect(),
        });
    }
    let mut dirty_chunks = crate::FoldHashSet::default();
    for &pos in &coords {
        dirty_chunks.insert(ChunkCoords::from_world(pos));
    }
    for chunk in dirty_chunks {
        crate::render::build_chunk_buffer(chunk);
    }
    scroll_to_cursor();
}

pub fn scroll_to_cursor() {
    let cursor = read_ui(|ui| ui.cursor.pos);
    with_vp(|vp| {
        let (w, h, m) = (vp.log_w(), vp.log_h(), 4.0);
        let cpx = cursor.x as f64 * vp.cw;
        let cpy = cursor.y as f64 * vp.ch;
        if cpx - vp.vx < m * vp.cw {
            vp.vx = cpx - m * vp.cw;
        }
        if cpx - vp.vx > w - m * vp.cw - vp.cw {
            vp.vx = cpx - w + m * vp.cw + vp.cw;
        }
        if cpy - vp.vy < m * vp.ch {
            vp.vy = cpy - m * vp.ch;
        }
        if cpy - vp.vy > h - m * vp.ch - vp.ch {
            vp.vy = cpy - h + m * vp.ch + vp.ch;
        }
        vp.clamp_viewport();
    });
    with_board(|b| b.chunk_manager.schedule_flush_debounced(0));
}

pub fn snap_to(pos: WorldCoords) {
    debug_log!("snap_to ({},{})", pos.x, pos.y);
    with_ui(|ui| {
        let color = ui.color;
        ui.cursor.move_to(pos, color);
    });
    with_vp(|vp| {
        let (hw, hh) = (vp.log_w() / 2.0, vp.log_h() / 2.0);
        vp.vx = pos.x as f64 * vp.cw - hw;
        vp.vy = pos.y as f64 * vp.ch - hh;
        vp.clamp_viewport();
    });
    with_board(|b| b.chunk_manager.schedule_flush_debounced(0));
    queue_anchor_update();
}

pub fn apply_zoom(z: f64, px: f64, py: f64) {
    with_vp(|vp| {
        debug_log!("apply_zoom {:.4} -> {:.4}", vp.zoom, z.clamp(0.03125, 8.0));
        vp.apply_zoom_inner(z, px, py);
    });
    with_board(|b| b.chunk_manager.schedule_flush_debounced(150));
    queue_viewport_update();
}

pub fn zoom_step(dir: i32, px: f64, py: f64) {
    with_vp(|vp| vp.zoom_step_inner(dir, px, py));
    with_board(|b| b.chunk_manager.schedule_flush_debounced(150));
    queue_viewport_update();
}

pub fn visible_chunk_range() -> (
    std::ops::RangeInclusive<i64>,
    std::ops::RangeInclusive<i64>,
    u8,
    bool,
) {
    read_vp(|vp| {
        let raw_step = (1.0_f64 / vp.zoom).max(1.0) as u32;
        let step = raw_step.next_power_of_two() as u8;
        let bounds = vp.visible_world_bounds();
        let is_glyph = vp.cw >= LOD_GLYPH_THRESHOLD;
        (
            bounds.visible_chunk_columns(),
            bounds.visible_chunk_rows(),
            step,
            is_glyph,
        )
    })
}

pub fn viewport_chunk_center() -> (f64, f64) {
    read_vp(|vp| {
        let (w, h) = (vp.log_w(), vp.log_h());
        let ccx = (vp.vx + w / 2.0) / vp.cw / CHUNK_W as f64;
        let ccy = (vp.vy + h / 2.0) / vp.ch / CHUNK_H as f64;
        (ccx, ccy)
    })
}

pub trait RgbExt {
    fn to_f32_alpha(&self, alpha: f32) -> [f32; 4];
    fn to_f32_array(&self) -> [f32; 3];
}

impl RgbExt for walloftext_shared::Rgb {
    #[inline]
    fn to_f32_alpha(&self, alpha: f32) -> [f32; 4] {
        [
            self.0 as f32 / 255.0,
            self.1 as f32 / 255.0,
            self.2 as f32 / 255.0,
            alpha,
        ]
    }
    #[inline]
    fn to_f32_array(&self) -> [f32; 3] {
        [
            self.0 as f32 / 255.0,
            self.1 as f32 / 255.0,
            self.2 as f32 / 255.0,
        ]
    }
}
