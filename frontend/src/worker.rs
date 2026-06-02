use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{DedicatedWorkerGlobalScope, MessageEvent};

use crate::network::zstd_decompress;

#[wasm_bindgen]
pub fn worker_entry() {
    let global = js_sys::global().unchecked_into::<DedicatedWorkerGlobalScope>();
    let global2 = global.clone();

    let on_message = Closure::<dyn FnMut(_)>::new(move |e: MessageEvent| {
        let buf: js_sys::ArrayBuffer = match e.data().dyn_into() {
            Ok(b) => b,
            Err(_) => return,
        };
        let compressed = js_sys::Uint8Array::new(&buf).to_vec();
        let decompressed = zstd_decompress(&compressed);
        let out = js_sys::Uint8Array::from(decompressed.as_slice());
        let transfer = js_sys::Array::of1(&out.buffer());
        let _ = global2.post_message_with_transfer(&out, &transfer);
    });

    global.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();
}
