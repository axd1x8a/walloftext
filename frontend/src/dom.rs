use std::cell::Cell;

use js_sys::Date;
use wasm_bindgen::JsCast;
use wasm_bindgen::convert::FromWasmAbi;
use wasm_bindgen::prelude::*;
use web_sys::{HtmlElement, HtmlInputElement};

use walloftext_shared::Rgb;

macro_rules! debug_log {
    ($($arg:tt)*) => {{
        #[cfg(debug_assertions)]
        {
            web_sys::console::log_1(&format!($($arg)*).into());
        }
    }};
}
pub(crate) use debug_log;

pub fn window() -> web_sys::Window {
    web_sys::window().unwrap()
}

pub fn document() -> web_sys::Document {
    window().document().unwrap()
}

pub fn perf() -> f64 {
    window().performance().unwrap().now()
}

pub fn random_vivid_color() -> Rgb {
    let hue = js_sys::Math::random() * 360.0;
    let c = 0.8_f64;
    let x = c * (1.0 - ((hue / 60.0 % 2.0) - 1.0).abs());
    let m = 0.2_f64;
    let (r, g, b) = match hue as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Rgb(
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    )
}

pub trait Listener: AsRef<web_sys::EventTarget> {
    fn on<E, F>(&self, event: &str, f: F)
    where
        E: FromWasmAbi + 'static,
        F: FnMut(E) + 'static,
    {
        let cb = Closure::<dyn FnMut(E)>::new(f);
        AsRef::<web_sys::EventTarget>::as_ref(self)
            .add_event_listener_with_callback(event, cb.as_ref().unchecked_ref())
            .unwrap();
        cb.forget();
    }

    fn on_capture<E, F>(&self, event: &str, f: F)
    where
        E: FromWasmAbi + 'static,
        F: FnMut(E) + 'static,
    {
        let cb = Closure::<dyn FnMut(E)>::new(f);
        AsRef::<web_sys::EventTarget>::as_ref(self)
            .add_event_listener_with_callback_and_bool(event, cb.as_ref().unchecked_ref(), true)
            .unwrap();
        cb.forget();
    }

    fn on_prevent<E, F>(&self, event: &str, f: F)
    where
        E: FromWasmAbi + 'static,
        F: FnMut(E) + 'static,
    {
        let cb = Closure::<dyn FnMut(E)>::new(f);
        let ael = web_sys::AddEventListenerOptions::new();
        ael.set_passive(false);
        AsRef::<web_sys::EventTarget>::as_ref(self)
            .add_event_listener_with_callback_and_add_event_listener_options(
                event,
                cb.as_ref().unchecked_ref(),
                ael.as_ref(),
            )
            .unwrap();
        cb.forget();
    }
}

impl<T: AsRef<web_sys::EventTarget>> Listener for T {}

pub struct Timer(Cell<i32>);

impl Timer {
    pub const fn new() -> Self {
        Self(Cell::new(-1))
    }

    pub fn start<F: FnOnce() + 'static>(&self, ms: i32, f: F) {
        self.cancel();
        self.0.set(set_timeout(ms, f));
    }

    pub fn cancel(&self) {
        let id = self.0.get();
        if id >= 0 {
            window().clear_timeout_with_handle(id);
            self.0.set(-1);
        }
    }
}

pub fn set_timeout<F: FnOnce() + 'static>(ms: i32, f: F) -> i32 {
    let cb = Closure::once_into_js(f);
    window()
        .set_timeout_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), ms)
        .unwrap()
}

pub fn set_interval<F: FnMut() + 'static>(ms: i32, f: F) {
    let cb = Closure::<dyn FnMut()>::new(f);
    window()
        .set_interval_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), ms)
        .unwrap();
    cb.forget();
}

#[derive(Clone)]
pub struct El {
    pub node: web_sys::Element,
}

impl AsRef<web_sys::EventTarget> for El {
    fn as_ref(&self) -> &web_sys::EventTarget {
        self.node.as_ref()
    }
}

impl El {
    pub fn new(tag: &str) -> Self {
        Self {
            node: document().create_element(tag).unwrap(),
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        document().get_element_by_id(id).map(|node| Self { node })
    }

    pub fn add_class(self, cls: &str) -> Self {
        let _ = self.node.class_list().add_1(cls);
        self
    }

    pub fn remove_class(self, cls: &str) -> Self {
        let _ = self.node.class_list().remove_1(cls);
        self
    }

    pub fn has_class(&self, cls: &str) -> bool {
        self.node.class_list().contains(cls)
    }

    pub fn get_value(&self) -> String {
        self.node
            .clone()
            .dyn_into::<HtmlInputElement>()
            .map(|i| i.value())
            .unwrap_or_default()
    }

    pub fn set_value(self, val: &str) -> Self {
        if let Ok(i) = self.node.clone().dyn_into::<HtmlInputElement>() {
            i.set_value(val);
        }
        self
    }

    pub fn set_inner_html(self, html: &str) -> Self {
        self.node.set_inner_html(html);
        self
    }

    pub fn css(self, prop: &str, val: &str) -> Self {
        if let Ok(he) = self.node.clone().dyn_into::<HtmlElement>() {
            let _ = he.style().set_property(prop, val);
        }
        self
    }

    pub fn remove_css(self, prop: &str) -> Self {
        if let Ok(he) = self.node.clone().dyn_into::<HtmlElement>() {
            let _ = he.style().remove_property(prop);
        }
        self
    }

    pub fn dataset_get(&self, key: &str) -> Option<String> {
        self.node
            .clone()
            .dyn_into::<HtmlElement>()
            .ok()
            .and_then(|he| he.dataset().get(key))
    }

    pub fn dataset_set(self, key: &str, val: &str) -> Self {
        if let Ok(he) = self.node.clone().dyn_into::<HtmlElement>() {
            let _ = he.dataset().set(key, val);
        }
        self
    }

    pub fn rect(&self) -> web_sys::DomRect {
        self.node.get_bounding_client_rect()
    }

    pub fn offset_height(&self) -> f64 {
        self.node
            .clone()
            .dyn_into::<HtmlElement>()
            .map(|he| he.offset_height() as f64)
            .unwrap_or(0.0)
    }

    pub fn div() -> Self {
        Self::new("div")
    }
    pub fn span() -> Self {
        Self::new("span")
    }

    pub fn class(self, name: &str) -> Self {
        self.node.set_class_name(name);
        self
    }

    pub fn attr(self, key: &str, val: &str) -> Self {
        self.node.set_attribute(key, val).unwrap();
        self
    }

    pub fn text(self, content: &str) -> Self {
        self.node.set_text_content(Some(content));
        self
    }

    pub fn style(self, st: &str) -> Self {
        self.node.set_attribute("style", st).unwrap();
        self
    }

    pub fn add(self, child: El) -> Self {
        self.node.append_child(&child.node).unwrap();
        self
    }
}

pub fn coord_span(pos: walloftext_shared::WorldCoords) -> El {
    El::span()
        .css("cursor", "pointer")
        .css("color", "var(--accent)")
        .attr("title", "go to coords")
        .attr("data-goto", &format!("{},{}", pos.x, pos.y))
        .text(&format!("({},{})", pos.x, pos.y))
}

fn log_line(msg_span: El) {
    if let Some(inner_el) = El::from_id("log-inner") {
        let d = Date::new_0();
        let time = format!(
            "{:02}:{:02}:{:02}",
            d.get_hours(),
            d.get_minutes(),
            d.get_seconds()
        );
        let line = El::div()
            .class("log-line")
            .add(El::span().class("lt").text(&time))
            .add(msg_span);
        let inner_node = inner_el.node.clone();
        let _ = inner_el.add(line);
        while inner_node.child_element_count() > 60 {
            if let Some(f) = inner_node.first_element_child() {
                let _ = inner_node.remove_child(&f);
            }
        }
        inner_node
            .dyn_into::<HtmlElement>()
            .unwrap()
            .set_scroll_top(9999);
    }
}

pub fn log_el(content: El, cls: &str) {
    log_line(El::span().class(&format!("lm {}", cls)).add(content));
}

pub fn log(msg: &str, cls: &str) {
    log_line(El::span().class(&format!("lm {}", cls)).text(msg));
}

pub fn sb_msg(msg: &str) {
    if let Some(el) = El::from_id("sb-msg") {
        let _ = el.text(&format!(" {}", msg));
    }
    set_timeout(4500, || {
        if let Some(el) = El::from_id("sb-msg") {
            let _ = el.text("");
        }
    });
}

pub fn hide_tooltips() {
    for id in ["pxinfo", "rginfo", "linkinfo"] {
        El::from_id(id).map(|e| e.css("opacity", "0"));
    }
}
