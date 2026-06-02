use js_sys::Date;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{HtmlCanvasElement, KeyboardEvent, MouseEvent, WheelEvent};

use walloftext_shared::{BOARD_HALF, ClientMsg, WorldCoords};

use crate::actions::{Action, dispatch};
use crate::dom::{Listener, debug_log, document, set_interval, window};
use crate::render::is_glyph_mode;
use crate::state::{HoveredLink, read_board, read_net, read_ui, read_vp, send_msg};

pub fn active_is_text_input() -> bool {
    document()
        .active_element()
        .map(|ae| {
            let tag = ae.tag_name().to_uppercase();
            if tag == "INPUT" || tag == "TEXTAREA" {
                return true;
            }
            ae.get_attribute("contenteditable")
                .map(|v| v != "false")
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn js_key(e: &KeyboardEvent) -> Option<String> {
    let v = js_sys::Reflect::get(e.as_ref(), &"key".into()).ok()?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    v.as_string()
}

pub fn detect_link(pos: WorldCoords) -> Option<HoveredLink> {
    read_board(|b| {
        let cell = |x: i16| b.chunk_manager.get_cell(WorldCoords { x, y: pos.y });
        let start = ((pos.x - 64)..pos.x)
            .rev()
            .take_while(|&x| cell(x).is_some())
            .last()
            .unwrap_or(pos.x);
        let end = (pos.x + 1..=pos.x + 64)
            .take_while(|&x| cell(x).is_some())
            .last()
            .unwrap_or(pos.x);
        let word: String = (start..=end)
            .filter_map(|x| cell(x).map(|c| c.ch))
            .collect();
        if word.is_empty() {
            return None;
        }
        if let Some(rest) = word.strip_prefix('#')
            && let Some(ci) = rest.find(',')
            && let (Ok(lx), Ok(ly)) = (rest[..ci].parse::<i64>(), rest[ci + 1..].parse::<i64>())
            && (-BOARD_HALF..BOARD_HALF).contains(&lx)
            && (-BOARD_HALF..BOARD_HALF).contains(&ly)
        {
            return Some(HoveredLink::Coord(WorldCoords::from_i64(lx, ly)));
        }
        if word.starts_with("http://") || word.starts_with("https://") {
            let all_system = (start..=end).all(|x| cell(x).is_some_and(|c| c.author_id == 0));
            if all_system {
                return Some(HoveredLink::Url(word));
            }
        }
        None
    })
}

pub fn goto(pos: WorldCoords) {
    debug_log!("goto ({},{})", pos.x, pos.y);
    dispatch(Action::SnapTo { pos });
}

pub fn on_keydown(e: KeyboardEvent) {
    if active_is_text_input() {
        return;
    }
    let key = match js_key(&e) {
        Some(k) => k,
        None => return,
    };
    let (code, ctrl, shift) = (e.code(), e.ctrl_key(), e.shift_key());

    if key == "Escape" {
        dispatch(Action::SelectClear);
        return;
    }

    if ctrl && !e.meta_key() {
        e.prevent_default();
        let (w, h) = read_vp(|vp| (vp.log_w() / 2.0, vp.log_h() / 2.0));
        match code.as_str() {
            "Equal" | "NumpadAdd" => dispatch(Action::ZoomStep {
                dir: 1,
                px: w,
                py: h,
            }),
            "Minus" | "NumpadSubtract" => dispatch(Action::ZoomStep {
                dir: -1,
                px: w,
                py: h,
            }),
            "Digit0" | "Numpad0" => dispatch(Action::ZoomReset),
            "KeyC" => dispatch(Action::CopySelection),
            "KeyZ" => dispatch(Action::Undo),
            "KeyV" => {
                let cb = Closure::once(move |val: wasm_bindgen::JsValue| {
                    if let Some(text) = val.as_string() {
                        dispatch(Action::PasteText(text));
                    }
                });
                let _ = js_sys::eval("navigator.clipboard.readText()")
                    .ok()
                    .and_then(|v| v.dyn_into::<js_sys::Promise>().ok())
                    .map(|p| {
                        let _ = p.then(&cb);
                        cb.forget();
                    });
            }
            "KeyR" | "KeyP" => dispatch(Action::TogglePanel("panel-regions")),
            "KeyS" => dispatch(Action::TogglePanel("panel-stats")),
            "KeyL" => dispatch(Action::TogglePanel("panel-log")),
            "KeyU" => dispatch(Action::TogglePanel("panel-auth")),
            "KeyG" => dispatch(Action::TogglePanel("panel-snap")),
            "KeyH" => dispatch(Action::TogglePanel("panel-help")),
            _ => {}
        }
        return;
    }
    if e.meta_key() || e.alt_key() {
        return;
    }

    if !is_glyph_mode() {
        return;
    }

    e.prevent_default();
    match key.as_str() {
        "ArrowRight" => {
            if shift {
                dispatch(Action::SelectExtendBy { dx: 1, dy: 0 })
            } else {
                dispatch(Action::MoveCursor { dx: 1, dy: 0 })
            }
        }
        "ArrowLeft" => {
            if shift {
                dispatch(Action::SelectExtendBy { dx: -1, dy: 0 })
            } else {
                dispatch(Action::MoveCursor { dx: -1, dy: 0 })
            }
        }
        "ArrowDown" => {
            if shift {
                dispatch(Action::SelectExtendBy { dx: 0, dy: 1 })
            } else {
                dispatch(Action::MoveCursor { dx: 0, dy: 1 })
            }
        }
        "ArrowUp" => {
            if shift {
                dispatch(Action::SelectExtendBy { dx: 0, dy: -1 })
            } else {
                dispatch(Action::MoveCursor { dx: 0, dy: -1 })
            }
        }
        "PageDown" => dispatch(Action::MoveCursorPageDown),
        "PageUp" => dispatch(Action::MoveCursorPageUp),
        "Home" => dispatch(Action::MoveCursorLineStart),
        "Enter" => dispatch(Action::MoveCursorNewline),
        "Tab" => dispatch(Action::MoveCursor { dx: 4, dy: 0 }),
        "Backspace" => dispatch(Action::DeleteBackward),
        "Delete" => dispatch(Action::DeleteForward),
        k if k.chars().count() == 1 => {
            if let Some(ch) = k.chars().next().filter(|c| !c.is_control()) {
                dispatch(Action::TypeChar(ch));
            }
        }
        _ => {}
    }
}

pub fn on_mousedown(e: MouseEvent, canvas: &HtmlCanvasElement) {
    let _ = canvas.focus();
    let (sx, sy, btn, alt, shift) = (
        e.client_x() as f64,
        e.client_y() as f64,
        e.button(),
        e.alt_key(),
        e.shift_key(),
    );
    if btn == 1 || (btn == 0 && alt) {
        e.prevent_default();
        debug_log!("mousedown: pan start (button={} alt={})", btn, alt);
        dispatch(Action::PanStart { sx, sy });
        canvas.style().set_property("cursor", "grabbing").unwrap();
        return;
    }
    if btn != 0 {
        return;
    }
    if !is_glyph_mode() {
        dispatch(Action::PanStart { sx, sy });
        canvas.style().set_property("cursor", "grabbing").unwrap();
        return;
    }
    let pos = read_vp(|vp| vp.screen_to_cell(sx, sy));
    if shift {
        debug_log!("mousedown: selection extend to ({},{})", pos.x, pos.y);
        dispatch(Action::SelectExtend { pos });
    } else {
        match read_vp(|vp| vp.hovered_link.clone()) {
            Some(HoveredLink::Coord(coord_pos)) => {
                debug_log!(
                    "mousedown: coord-link click -> ({},{})",
                    coord_pos.x,
                    coord_pos.y
                );
                dispatch(Action::SnapTo { pos: coord_pos });
            }
            Some(HoveredLink::Url(url)) => {
                debug_log!("mousedown: url-link click -> {}", url);
                let _ = window().open_with_url_and_target(&url, "_blank");
            }
            None => {
                debug_log!("mousedown: cursor moved to ({},{})", pos.x, pos.y);
                dispatch(Action::JumpCursor { pos });
            }
        }
    }
}

pub fn on_mousemove(e: MouseEvent) {
    let (sx, sy) = (e.client_x() as f64, e.client_y() as f64);

    if read_vp(|vp| vp.is_pan) {
        dispatch(Action::PanMove { sx, sy });
        return;
    }

    if read_ui(|ui| ui.cursor.sel.drag) {
        let pos = read_vp(|vp| vp.screen_to_cell(sx, sy));
        dispatch(Action::SelectDrag { pos });
        return;
    }

    let pos = read_vp(|vp| vp.screen_to_cell(sx, sy));
    let prev_hover = read_vp(|vp| vp.hover);

    if prev_hover != Some(pos) {
        let link = detect_link(pos);
        dispatch(Action::UpdateHover { sx, sy, pos, link });
    } else {
        dispatch(Action::UpdateHoverPos { sx, sy });
    }
}

pub fn on_mouseup(e: MouseEvent, canvas: &HtmlCanvasElement) {
    let was_pan = read_vp(|vp| vp.is_pan);
    dispatch(Action::PanEnd);
    canvas.style().set_property("cursor", "").unwrap();
    if was_pan {
        return;
    }
    let pos = read_vp(|vp| vp.screen_to_cell(e.client_x() as f64, e.client_y() as f64));
    if read_ui(|ui| ui.cursor.sel.active) {
        debug_log!("selection end at ({},{})", pos.x, pos.y);
    }
    dispatch(Action::SelectEnd { pos });
}

pub fn on_mouseleave(canvas: &HtmlCanvasElement) {
    dispatch(Action::MouseLeave);
    canvas.style().set_property("cursor", "").unwrap();
}

pub fn on_wheel(e: WheelEvent) {
    e.prevent_default();
    let (sx, sy, ctrl, shift) = (
        e.client_x() as f64,
        e.client_y() as f64,
        e.ctrl_key(),
        e.shift_key(),
    );
    if ctrl {
        let dir = if e.delta_y() < 0.0 { 1 } else { -1 };
        dispatch(Action::ZoomStep {
            dir,
            px: sx,
            py: sy,
        });
    } else {
        let (dx, dy) = (
            if shift { e.delta_y() } else { e.delta_x() },
            if shift { 0.0 } else { e.delta_y() },
        );
        dispatch(Action::ScrollViewport { dx, dy });
    }
}

fn touch_dist(t0: &web_sys::Touch, t1: &web_sys::Touch) -> f64 {
    let dx = t0.client_x() as f64 - t1.client_x() as f64;
    let dy = t0.client_y() as f64 - t1.client_y() as f64;
    (dx * dx + dy * dy).sqrt()
}

fn touch_mid(t0: &web_sys::Touch, t1: &web_sys::Touch) -> (f64, f64) {
    (
        (t0.client_x() as f64 + t1.client_x() as f64) / 2.0,
        (t0.client_y() as f64 + t1.client_y() as f64) / 2.0,
    )
}

pub fn on_touchstart(e: web_sys::TouchEvent) {
    e.prevent_default();
    if let Some(inp) = document()
        .get_element_by_id("mobile-input")
        .and_then(|e| e.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
    {
        let _ = inp.blur();
    }
    let touches = e.touches();
    let n = touches.length();
    debug_log!("touchstart: {} touch(es)", n);
    let t = Date::now();
    if n == 1 {
        let touch = touches.get(0).unwrap();
        dispatch(Action::TouchStart1 {
            sx: touch.client_x() as f64,
            sy: touch.client_y() as f64,
            t,
        });
    } else if n == 2 {
        let t0 = touches.get(0).unwrap();
        let t1 = touches.get(1).unwrap();
        let dist = touch_dist(&t0, &t1);
        let (mx, my) = touch_mid(&t0, &t1);
        dispatch(Action::TouchStart2 { dist, mx, my });
    }
}

pub fn on_touchmove(e: web_sys::TouchEvent) {
    e.prevent_default();
    let touches = e.touches();
    let n = touches.length();
    let prev_count = read_vp(|vp| vp.touch_count);
    let t = Date::now();
    if n == 1 && prev_count == 1 {
        let touch = touches.get(0).unwrap();
        dispatch(Action::TouchMove1 {
            sx: touch.client_x() as f64,
            sy: touch.client_y() as f64,
            t,
        });
    } else if n == 2 && prev_count == 2 {
        let t0 = touches.get(0).unwrap();
        let t1 = touches.get(1).unwrap();
        let dist = touch_dist(&t0, &t1);
        let (mx, my) = touch_mid(&t0, &t1);
        dispatch(Action::TouchMove2 { dist, mx, my });
    }
}

pub fn on_touchend(e: web_sys::TouchEvent) {
    e.prevent_default();
    let remaining = e.touches();
    let remaining_count = remaining.length();
    let prev_count = read_vp(|vp| vp.touch_count);

    if prev_count == 2 && remaining_count == 1 {
        let touch = remaining.get(0).unwrap();
        let t = Date::now();
        dispatch(Action::TouchTransition2to1 {
            sx: touch.client_x() as f64,
            sy: touch.client_y() as f64,
            t,
        });
        return;
    }

    let (moved, sx, sy, t_start) = read_vp(|vp| {
        (
            vp.touch_moved,
            vp.touch_start_x,
            vp.touch_start_y,
            vp.touch_start_t,
        )
    });
    crate::state::with_vp(|vp| vp.touch_count = remaining_count);
    dispatch(Action::TouchEnd {
        sx,
        sy,
        t_start,
        moved,
    });
}

pub fn init_mobile_input() {
    let Some(inp_el) = document().get_element_by_id("mobile-input") else {
        return;
    };
    let inp: web_sys::HtmlTextAreaElement = inp_el.clone().dyn_into().unwrap();

    inp_el.on("input", move |_: web_sys::Event| {
        let val = inp.value();
        inp.set_value("");
        if val.is_empty() || !is_glyph_mode() {
            return;
        }
        for ch in val.chars() {
            match ch {
                '\n' | '\r' => dispatch(Action::MobileNewline),
                '\x08' => dispatch(Action::MobileBackspace),
                c if !c.is_control() => dispatch(Action::MobileChar(c)),
                _ => {}
            }
        }
    });

    inp_el.on("keydown", |e: KeyboardEvent| {
        if !is_glyph_mode() {
            return;
        }
        let key = e.key();
        if e.ctrl_key() {
            if key == "z" || key == "Z" {
                e.prevent_default();
                dispatch(Action::Undo);
            }
            return;
        }
        match key.as_str() {
            "ArrowLeft" => {
                e.prevent_default();
                dispatch(Action::MoveCursor { dx: -1, dy: 0 });
            }
            "ArrowRight" => {
                e.prevent_default();
                dispatch(Action::MoveCursor { dx: 1, dy: 0 });
            }
            "ArrowUp" => {
                e.prevent_default();
                dispatch(Action::MoveCursor { dx: 0, dy: -1 });
            }
            "ArrowDown" => {
                e.prevent_default();
                dispatch(Action::MoveCursor { dx: 0, dy: 1 });
            }
            "Backspace" => {
                e.prevent_default();
                dispatch(Action::MobileBackspace);
            }
            "Enter" => {
                e.prevent_default();
                dispatch(Action::MobileNewline);
            }
            _ => {}
        }
    });
}

pub fn init_stats_poll() {
    set_interval(15_000, || {
        if read_net(|n| n.is_conn) {
            send_msg(&ClientMsg::FetchStats);
        }
    });
}
