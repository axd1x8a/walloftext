use js_sys::Date;
use wasm_bindgen::JsCast;

use walloftext_shared::{BOARD_HALF, WorldCoords};

use crate::dom::{Timer, debug_log, document, hide_tooltips, sb_msg, window};
use crate::render::{LOD_GLYPH_THRESHOLD, is_glyph_mode, schedule_render, start_momentum};
use crate::state::{
    CELL_H, CELL_W, CanvasCell, HoveredLink, apply_zoom, read_board, read_ui, read_vp,
    scroll_to_cursor, snap_to, undo, with_board, with_ui, with_vp, write_cell, zoom_step,
};
use crate::widgets::{MiniTUI, render_all, render_popups};

thread_local! {
    pub static LONG_PRESS: Timer = const { Timer::new() };
}

pub enum Action {
    MoveCursor {
        dx: i64,
        dy: i64,
    },
    MoveCursorPageDown,
    MoveCursorPageUp,
    MoveCursorLineStart,
    MoveCursorNewline,
    JumpCursor {
        pos: WorldCoords,
    },
    TypeChar(char),
    DeleteBackward,
    DeleteForward,
    ZoomStep {
        dir: i32,
        px: f64,
        py: f64,
    },
    ZoomReset,
    PanStart {
        sx: f64,
        sy: f64,
    },
    PanMove {
        sx: f64,
        sy: f64,
    },
    PanEnd,
    MouseLeave,
    ScrollViewport {
        dx: f64,
        dy: f64,
    },
    UpdateHover {
        sx: f64,
        sy: f64,
        pos: WorldCoords,
        link: Option<HoveredLink>,
    },
    UpdateHoverPos {
        sx: f64,
        sy: f64,
    },
    SnapTo {
        pos: WorldCoords,
    },
    SelectExtend {
        pos: WorldCoords,
    },
    SelectExtendBy {
        dx: i64,
        dy: i64,
    },
    SelectDrag {
        pos: WorldCoords,
    },
    SelectEnd {
        pos: WorldCoords,
    },
    SelectClear,
    Undo,
    CopySelection,
    PasteText(String),
    TogglePanel(&'static str),
    FollowLink(HoveredLink),
    TouchStart1 {
        sx: f64,
        sy: f64,
        t: f64,
    },
    TouchStart2 {
        dist: f64,
        mx: f64,
        my: f64,
    },
    TouchMove1 {
        sx: f64,
        sy: f64,
        t: f64,
    },
    TouchMove2 {
        dist: f64,
        mx: f64,
        my: f64,
    },
    TouchEnd {
        sx: f64,
        sy: f64,
        t_start: f64,
        moved: bool,
    },
    TouchTransition2to1 {
        sx: f64,
        sy: f64,
        t: f64,
    },
    ShowTouchInfo {
        sx: f64,
        sy: f64,
    },
    MobileChar(char),
    MobileNewline,
    MobileBackspace,
}

pub fn dispatch(action: Action) {
    use Action::*;
    match action {
        SelectClear => {
            with_ui(|ui| ui.cursor.sel.clear());
            render_all();
        }

        ZoomStep { dir, px, py } => {
            zoom_step(dir, px, py);
            render_all();
            schedule_render();
        }

        ZoomReset => {
            let (w, h) = read_vp(|vp| (vp.log_w() / 2.0, vp.log_h() / 2.0));
            apply_zoom(1.0, w, h);
            with_vp(|vp| {
                vp.vx = 0.0;
                vp.vy = 0.0;
            });
            with_ui(|ui| {
                let color = ui.color;
                ui.cursor.move_to(WorldCoords { x: 0, y: 0 }, color);
            });
            render_all();
            schedule_render();
        }

        Undo => {
            undo();
            render_all();
            schedule_render();
        }

        PasteText(text) => {
            paste_text_impl(text);
        }

        CopySelection => {
            copy_selection_impl();
        }

        TogglePanel(id) => {
            MiniTUI::toggle(id);
        }

        MoveCursor { dx, dy } => {
            with_ui(|ui| {
                let color = ui.color;
                let new_pos = ui.cursor.pos.offset(dx, dy);
                ui.cursor.move_to(new_pos, color);
            });
            scroll_to_cursor();
            render_all();
            schedule_render();
        }

        MoveCursorPageDown => {
            let rows = read_vp(|vp| (vp.log_h() / vp.ch).floor() as i64);
            with_ui(|ui| {
                let color = ui.color;
                let new_pos = ui.cursor.pos.offset(0, rows);
                ui.cursor.move_to(new_pos, color);
            });
            scroll_to_cursor();
            render_all();
            schedule_render();
        }

        MoveCursorPageUp => {
            let rows = read_vp(|vp| (vp.log_h() / vp.ch).floor() as i64);
            with_ui(|ui| {
                let color = ui.color;
                let new_pos = ui.cursor.pos.offset(0, -rows);
                ui.cursor.move_to(new_pos, color);
            });
            scroll_to_cursor();
            render_all();
            schedule_render();
        }

        MoveCursorLineStart => {
            with_ui(|ui| {
                let color = ui.color;
                let new_pos = WorldCoords {
                    x: 0,
                    y: ui.cursor.pos.y,
                };
                ui.cursor.move_to(new_pos, color);
            });
            scroll_to_cursor();
            render_all();
            schedule_render();
        }

        MoveCursorNewline => {
            with_ui(|ui| {
                let color = ui.color;
                let new_pos = WorldCoords {
                    x: 0,
                    y: ui.cursor.pos.offset(0, 1).y,
                };
                ui.cursor.move_to(new_pos, color);
            });
            scroll_to_cursor();
            render_all();
            schedule_render();
        }

        TypeChar(ch) => {
            let pos = read_ui(|ui| ui.cursor.pos);
            write_cell(pos, CanvasCell::typed(ch));
            crate::render::build_chunk_buffer(pos.to_chunk().0);
            with_ui(|ui| {
                let color = ui.color;
                let new_pos = ui.cursor.pos.offset(1, 0);
                ui.cursor.move_to(new_pos, color);
            });
            scroll_to_cursor();
            render_all();
            schedule_render();
        }

        DeleteBackward => {
            let pos = with_ui(|ui| {
                let color = ui.color;
                let new_pos = ui.cursor.pos.offset(-1, 0);
                ui.cursor.move_to(new_pos, color);
                ui.cursor.pos
            });
            write_cell(pos, None);
            crate::render::build_chunk_buffer(pos.to_chunk().0);
            scroll_to_cursor();
            render_all();
            schedule_render();
        }

        DeleteForward => {
            let pos = read_ui(|ui| ui.cursor.pos);
            write_cell(pos, None);
            crate::render::build_chunk_buffer(pos.to_chunk().0);
            with_ui(|ui| {
                let color = ui.color;
                let new_pos = ui.cursor.pos.offset(1, 0);
                ui.cursor.move_to(new_pos, color);
            });
            scroll_to_cursor();
            render_all();
            schedule_render();
        }

        JumpCursor { pos } => {
            with_ui(|ui| {
                let color = ui.color;
                ui.cursor.move_to(pos, color);
            });
            render_all();
            schedule_render();
        }

        PanStart { sx, sy } => {
            with_vp(|vp| {
                vp.is_pan = true;
                vp.pan_sx = sx;
                vp.pan_sy = sy;
                vp.pan_ox = vp.vx;
                vp.pan_oy = vp.vy;
            });
        }

        PanMove { sx, sy } => {
            with_vp(|vp| {
                vp.vx = vp.pan_ox - (sx - vp.pan_sx);
                vp.vy = vp.pan_oy - (sy - vp.pan_sy);
                vp.clamp_viewport();
            });
            with_board(|b| b.chunk_manager.schedule_flush_debounced(0));
            crate::state::queue_viewport_update();
            if read_vp(|vp| vp.hover.is_some()) {
                with_vp(|vp| {
                    vp.hover = None;
                    vp.hovered_link = None;
                });
                hide_tooltips();
            }
            schedule_render();
        }

        PanEnd => {
            with_vp(|vp| vp.is_pan = false);
            with_ui(|ui| ui.cursor.sel.drag = false);
            MiniTUI::save_layout();
            schedule_render();
        }

        MouseLeave => {
            with_vp(|vp| {
                vp.is_pan = false;
                vp.hover = None;
                vp.hovered_link = None;
            });
            with_ui(|ui| ui.cursor.sel.drag = false);
            hide_tooltips();
        }

        ScrollViewport { dx, dy } => {
            with_vp(|vp| {
                vp.vx += dx;
                vp.vy += dy;
                vp.clamp_viewport();
            });
            with_board(|b| b.chunk_manager.schedule_flush_debounced(0));
            crate::state::queue_viewport_update();
            schedule_render();
        }

        UpdateHover { sx, sy, pos, link } => {
            with_vp(|vp| {
                vp.hover = Some(pos);
                vp.hover_sx = sx;
                vp.hover_sy = sy + 8.0;
                vp.hover_above = false;
                vp.hovered_link = link.clone();
            });
            if let Some(canvas) = read_vp(|vp| vp.canvas.clone()) {
                let _ = canvas
                    .style()
                    .set_property("cursor", if link.is_some() { "pointer" } else { "" });
            }
            render_all();
        }

        UpdateHoverPos { sx, sy } => {
            with_vp(|vp| {
                vp.hover_sx = sx;
                vp.hover_sy = sy + 8.0;
            });
            render_popups();
        }

        SelectExtend { pos } => {
            with_ui(|ui| {
                let color = ui.color;
                ui.cursor.select_to(pos, color);
                ui.cursor.sel.drag = true;
            });
            scroll_to_cursor();
            render_all();
            schedule_render();
        }

        SelectExtendBy { dx, dy } => {
            with_ui(|ui| {
                let color = ui.color;
                let end = ui.cursor.pos.offset(dx, dy);
                ui.cursor.select_to(end, color);
            });
            scroll_to_cursor();
            render_all();
            schedule_render();
        }

        SelectDrag { pos } => {
            with_ui(|ui| {
                let color = ui.color;
                ui.cursor.drag_sel_to(pos, color);
            });
            render_all();
            schedule_render();
        }

        SelectEnd { pos } => {
            with_ui(|ui| {
                ui.cursor.sel.end_drag(pos);
                ui.cursor.pos = pos;
            });
            render_all();
            schedule_render();
        }

        SnapTo { pos } => {
            snap_to(pos);
            render_all();
            schedule_render();
        }

        FollowLink(link) => match link {
            HoveredLink::Coord(pos) => {
                snap_to(pos);
                render_all();
                schedule_render();
            }
            HoveredLink::Url(url) => {
                let _ = window().open_with_url_and_target(&url, "_blank");
            }
        },

        TouchStart1 { sx, sy, t } => {
            LONG_PRESS.with(|lp| lp.cancel());
            with_vp(|vp| {
                if vp.hover.is_some() {
                    vp.hover = None;
                    vp.hovered_link = None;
                }
            });
            hide_tooltips();
            let cw = read_vp(|vp| vp.cw);
            with_vp(|vp| {
                vp.touch_count = 1;
                vp.touch_vel_x = 0.0;
                vp.touch_vel_y = 0.0;
                vp.touch_start_x = sx;
                vp.touch_start_y = sy;
                vp.touch_start_vx = vp.vx;
                vp.touch_start_vy = vp.vy;
                vp.touch_start_t = t;
                vp.touch_last_x = sx;
                vp.touch_last_y = sy;
                vp.touch_last_t = t;
                vp.touch_moved = false;
            });
            if cw >= LOD_GLYPH_THRESHOLD {
                LONG_PRESS.with(|timer| {
                    timer.start(500, move || dispatch(Action::ShowTouchInfo { sx, sy }));
                });
            }
        }

        TouchStart2 { dist, mx, my } => {
            LONG_PRESS.with(|lp| lp.cancel());
            with_vp(|vp| {
                vp.touch_count = 2;
                vp.touch_vel_x = 0.0;
                vp.touch_vel_y = 0.0;
                vp.pinch_start_dist = dist.max(20.0);
                vp.pinch_start_zoom = vp.zoom;
                vp.pinch_start_vx = vp.vx;
                vp.pinch_start_vy = vp.vy;
                vp.touch_start_x = mx;
                vp.touch_start_y = my;
            });
        }

        TouchMove1 { sx, sy, t } => {
            with_vp(|vp| {
                let dx = sx - vp.touch_start_x;
                let dy = sy - vp.touch_start_y;
                if dx.abs() > 4.0 || dy.abs() > 4.0 {
                    vp.touch_moved = true;
                    LONG_PRESS.with(|lp| lp.cancel());
                }
                let dt = t - vp.touch_last_t;
                if dt > 0.0 && dt < 100.0 {
                    vp.touch_vel_x = (sx - vp.touch_last_x) / dt;
                    vp.touch_vel_y = (sy - vp.touch_last_y) / dt;
                }
                vp.touch_last_x = sx;
                vp.touch_last_y = sy;
                vp.touch_last_t = t;
                vp.vx = vp.touch_start_vx - dx;
                vp.vy = vp.touch_start_vy - dy;
                vp.clamp_viewport();
            });
            with_board(|b| b.chunk_manager.schedule_flush_debounced(0));
            crate::state::queue_viewport_update();
            schedule_render();
        }

        TouchMove2 { dist, mx, my } => {
            with_vp(|vp| {
                if dist < 20.0 {
                    vp.pinch_start_dist = 20.0;
                    vp.pinch_start_zoom = vp.zoom;
                    vp.touch_start_x = mx;
                    vp.touch_start_y = my;
                    vp.pinch_start_vx = vp.vx;
                    vp.pinch_start_vy = vp.vy;
                }
                if vp.pinch_start_dist > 0.0 {
                    let new_zoom =
                        (vp.pinch_start_zoom * dist / vp.pinch_start_dist).clamp(0.03125, 8.0);
                    let new_cw = CELL_W * new_zoom;
                    let new_ch = CELL_H * new_zoom;
                    let wx =
                        (vp.touch_start_x + vp.pinch_start_vx) / (CELL_W * vp.pinch_start_zoom);
                    let wy =
                        (vp.touch_start_y + vp.pinch_start_vy) / (CELL_H * vp.pinch_start_zoom);
                    vp.zoom = new_zoom;
                    vp.cw = new_cw;
                    vp.ch = new_ch;
                    vp.vx = wx * new_cw - mx;
                    vp.vy = wy * new_ch - my;
                    vp.clamp_viewport();
                }
                vp.touch_moved = true;
            });
            with_board(|b| b.chunk_manager.schedule_flush_debounced(0));
            crate::state::queue_viewport_update();
            schedule_render();
        }

        TouchEnd {
            sx,
            sy,
            t_start,
            moved,
        } => {
            let vel = if moved {
                read_vp(|vp| (vp.touch_vel_x, vp.touch_vel_y))
            } else {
                (0.0, 0.0)
            };
            if moved && (vel.0.abs() > 0.15 || vel.1.abs() > 0.15) {
                debug_log!(
                    "touchend: starting momentum scroll ({:.3},{:.3})",
                    vel.0,
                    vel.1
                );
                start_momentum();
            }
            if !moved && Date::now() - t_start < 400.0 && is_glyph_mode() {
                let pos = read_vp(|vp| vp.screen_to_cell(sx, sy));
                match crate::input::detect_link(pos) {
                    Some(link) => dispatch(Action::FollowLink(link)),
                    None => {
                        with_ui(|ui| {
                            let color = ui.color;
                            ui.cursor.move_to(pos, color);
                        });
                        scroll_to_cursor();
                        render_all();
                        schedule_render();
                        if let Some(inp) = document()
                            .get_element_by_id("mobile-input")
                            .and_then(|e| e.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
                        {
                            let _ = inp.focus();
                        }
                    }
                }
            }
        }

        TouchTransition2to1 { sx, sy, t } => {
            with_vp(|vp| {
                vp.touch_count = 1;
                vp.touch_start_x = sx;
                vp.touch_start_y = sy;
                vp.touch_start_vx = vp.vx;
                vp.touch_start_vy = vp.vy;
                vp.touch_start_t = t;
                vp.touch_last_x = sx;
                vp.touch_last_y = sy;
                vp.touch_last_t = t;
                vp.touch_moved = true;
                vp.touch_vel_x = 0.0;
                vp.touch_vel_y = 0.0;
            });
        }

        ShowTouchInfo { sx, sy } => {
            LONG_PRESS.with(|lp| lp.cancel());
            let pos = read_vp(|vp| vp.screen_to_cell(sx, sy));
            let link = crate::input::detect_link(pos);
            with_vp(|vp| {
                vp.hover = Some(pos);
                vp.hover_sx = sx;
                vp.hover_sy = sy;
                vp.hover_above = true;
                vp.hovered_link = link;
            });
            render_all();
        }

        MobileChar(ch) => {
            let pos = read_ui(|ui| ui.cursor.pos);
            write_cell(pos, CanvasCell::typed(ch));
            crate::render::build_chunk_buffer(pos.to_chunk().0);
            with_ui(|ui| {
                let color = ui.color;
                let new_pos = ui.cursor.pos.offset(1, 0);
                ui.cursor.move_to(new_pos, color);
            });
            scroll_to_cursor();
            render_all();
            schedule_render();
        }

        MobileNewline => {
            with_ui(|ui| {
                let color = ui.color;
                let new_pos = WorldCoords {
                    x: 0,
                    y: ui.cursor.pos.offset(0, 1).y,
                };
                ui.cursor.move_to(new_pos, color);
            });
            scroll_to_cursor();
            render_all();
            schedule_render();
        }

        MobileBackspace => {
            let pos = with_ui(|ui| {
                let color = ui.color;
                let new_pos = ui.cursor.pos.offset(-1, 0);
                ui.cursor.move_to(new_pos, color);
                ui.cursor.pos
            });
            write_cell(pos, None);
            crate::render::build_chunk_buffer(pos.to_chunk().0);
            scroll_to_cursor();
            render_all();
            schedule_render();
        }
    }
}

fn copy_selection_impl() {
    let (active, sel) = read_ui(|ui| (ui.cursor.sel.active, ui.cursor.sel.rect()));
    if !active {
        return;
    }
    debug_log!(
        "copy_selection: ({},{}) to ({},{})",
        sel.a.x,
        sel.a.y,
        sel.b.x,
        sel.b.y
    );
    let text: String = (sel.a.y..=sel.b.y)
        .map(|y| {
            (sel.a.x..=sel.b.x)
                .map(|x| {
                    read_board(|b| {
                        b.chunk_manager
                            .get_cell(WorldCoords { x, y })
                            .map(|c| c.ch)
                            .unwrap_or(' ')
                    })
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let escaped = text
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace("${", "\\${");
    let _ = js_sys::eval(&format!(
        "navigator.clipboard&&navigator.clipboard.writeText(`{}`)",
        escaped
    ));
    sb_msg("copied");
}

fn paste_text_impl(text: String) {
    debug_log!(
        "paste_text: {} line(s), {} byte(s)",
        text.lines().count(),
        text.len()
    );
    const MAX_COLS: i64 = 2048;
    const MAX_ROWS: i64 = 2048;
    let origin = read_ui(|ui| ui.cursor.pos);
    with_ui(|ui| ui.begin_undo_batch());
    let mut dirty_chunks = crate::FoldHashSet::default();
    for (row, line) in text.lines().take(MAX_ROWS as usize).enumerate() {
        for (col, ch) in line.chars().take(MAX_COLS as usize).enumerate() {
            let wx = origin.x as i64 + col as i64;
            let wy = origin.y as i64 + row as i64;
            if !(-BOARD_HALF..BOARD_HALF).contains(&wx) || !(-BOARD_HALF..BOARD_HALF).contains(&wy)
            {
                continue;
            }
            let pos = WorldCoords::from_i64(wx, wy);
            write_cell(pos, CanvasCell::typed(ch));
            dirty_chunks.insert(pos.to_chunk().0);
        }
    }
    let row = text.lines().take(MAX_ROWS as usize).count() as i64;
    with_ui(|ui| {
        ui.end_undo_batch();
        let color = ui.color;
        let new_pos = WorldCoords::from_i64(origin.x as i64, origin.y as i64 + row.max(1) - 1);
        ui.cursor.move_to(new_pos, color);
    });
    for chunk in dirty_chunks {
        crate::render::build_chunk_buffer(chunk);
    }
    scroll_to_cursor();
    render_all();
    schedule_render();
}
