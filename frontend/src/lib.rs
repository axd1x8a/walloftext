mod actions;
mod chunks;
mod dom;
mod input;
mod network;
mod render;
mod state;
mod widgets;
mod worker;

pub(crate) type FoldHashMap<K, V> = foldhash::HashMap<K, V>;
pub(crate) type FoldHashSet<T> = foldhash::HashSet<T>;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{HtmlCanvasElement, KeyboardEvent, MouseEvent};

use actions::{Action, dispatch};
use dom::{Listener, debug_log, document, set_interval, window};
use input::{
    active_is_text_input, on_keydown, on_mousedown, on_mouseleave, on_mousemove, on_mouseup,
    on_touchend, on_touchmove, on_touchstart, on_wheel,
};
use render::{WGPU_STATE, is_glyph_mode, resize_canvas, schedule_render};
use state::with_vp;
use widgets::{MiniTUI, render_all};

use crate::state::with_net;

fn show_fatal_error(msg: &str) {
    dom::El::from_id("overlay-msg").map(|el| el.text(msg));
    dom::El::from_id("overlay-reload").map(|el| el.css("display", "block"));
    dom::El::from_id("overlay").map(|el| el.css("display", "flex"));
}

#[wasm_bindgen]
pub fn main(worker: web_sys::Worker) {
    std::panic::set_hook(Box::new(|info| {
        show_fatal_error("An unexpected error occurred. Please reload the page.");
        console_error_panic_hook::hook(info);
    }));
    debug_log!("frontend init starting");

    let doc = document();
    let canvas: HtmlCanvasElement = doc.get_element_by_id("cv-gl").unwrap().dyn_into().unwrap();
    with_vp(|vp| {
        vp.canvas = Some(canvas.clone());
    });

    wasm_bindgen_futures::spawn_local(async {
        let state = render::init_wgpu().await;
        let ok = state.is_some();
        WGPU_STATE.with(|ws| *ws.borrow_mut() = state);
        if ok {
            debug_log!("wgpu init ok");
        } else {
            web_sys::console::error_1(&"wgpu init failed".into());
            show_fatal_error(
                "Failed to initialize the renderer. \
                 WebGPU and WebGL2 are both unavailable. \
                 Try enabling hardware acceleration in your browser settings, \
                 or use Chrome 113+, Firefox 100+, or Safari 17+.",
            );
        }
        resize_canvas();
    });

    MiniTUI::init_drag();
    MiniTUI::init_close_buttons();
    MiniTUI::init_panel_toggle_buttons();

    document().on("click", |e: MouseEvent| {
        let target = match e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        {
            Some(t) => t,
            None => return,
        };
        if let Some(raw) = target.get_attribute("data-goto")
            && let Some((xs, ys)) = raw.split_once(',')
            && let (Ok(x), Ok(y)) = (xs.parse::<i64>(), ys.parse::<i64>())
        {
            dispatch(Action::SnapTo {
                pos: walloftext_shared::WorldCoords::from_i64(x, y),
            });
        }
    });

    set_interval(250, schedule_render);

    window().on("resize", |_: web_sys::Event| MiniTUI::clamp_all());

    window().on("popstate", |_: web_sys::Event| {
        state::apply_anchor_viewport();
        state::with_board(|b| b.chunk_manager.schedule_flush_debounced(0));
        state::queue_viewport_update();
        schedule_render();
    });

    if let Some(btn) = dom::El::from_id("kicked-reconnect") {
        btn.on("click", |_: web_sys::Event| {
            with_net(|n| n.kicked = false);
            dom::El::from_id("overlay").map(|el| el.css("display", "none"));
            network::connect();
        });
    }

    for w in widgets::widgets() {
        w.init_events();
    }

    resize_canvas();
    state::apply_anchor_viewport();
    window().on("resize", |_: web_sys::Event| resize_canvas());

    doc.on("keydown", move |e: KeyboardEvent| on_keydown(e));

    doc.on("keypress", move |e: KeyboardEvent| {
        if active_is_text_input() || !is_glyph_mode() {
            return;
        }
        let key = e.key();
        let ch = match key.chars().next() {
            Some(c) if key.chars().count() == 1 && !c.is_control() => c,
            _ => return,
        };
        dispatch(Action::TypeChar(ch));
    });

    {
        let cv = canvas.clone();
        canvas.on("mousedown", move |e: MouseEvent| on_mousedown(e, &cv));
    }
    canvas.on("mousemove", on_mousemove);
    {
        let cv = canvas.clone();
        canvas.on("mouseup", move |e: MouseEvent| on_mouseup(e, &cv));
    }
    {
        let cv = canvas.clone();
        canvas.on("mouseleave", move |_: MouseEvent| on_mouseleave(&cv));
    }
    canvas.on_prevent("wheel", on_wheel);
    canvas.on_prevent("touchstart", on_touchstart);
    canvas.on_prevent("touchmove", on_touchmove);
    {
        canvas.on_prevent("touchend", move |e: web_sys::TouchEvent| on_touchend(e));
    }

    set_interval(30_000, || {
        let now = dom::perf();
        let cutoff = now - 120_000.0;
        let (cols, rows, _, _) = state::visible_chunk_range();
        state::with_board(|b| {
            chunks::evict_unseen(&mut b.chunk_manager.chunks, cutoff, cols, rows);
        });
        render::drop_stale_chunk_bufs();
    });

    input::init_mobile_input();
    input::init_stats_poll();
    MiniTUI::load_layout();
    render_all();
    network::init_worker_bridge(worker);
    network::connect();
    let _ = canvas.focus();

    window().on("beforeunload", |_: web_sys::Event| MiniTUI::save_layout());
}
