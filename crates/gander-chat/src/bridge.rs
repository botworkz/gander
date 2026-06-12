// SPDX-License-Identifier: GPL-3.0-or-later

//! JS bridge bindings to `window.gander`.
//!
//! The host (gander binary) must expose a `window.gander` object with two
//! methods before the WASM module is instantiated:
//!
//! ```js
//! window.gander = {
//!   send(text)          // queue a user message; starts streaming events
//!   subscribe(callback) // register one event-receiver callback
//! }
//! ```
//!
//! Events are plain JS objects delivered to the registered callback.
//! See `README.md` for the full event schema.

use js_sys::Function;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;

/// Return the `window.gander` object, or `None` if it is not present.
///
/// Absence is normal in test environments; callers should degrade
/// gracefully rather than panicking.
fn get_gander() -> Option<JsValue> {
    let window = web_sys::window()?;
    let gander = js_sys::Reflect::get(&window, &JsValue::from_str("gander")).ok()?;
    if gander.is_undefined() || gander.is_null() {
        None
    } else {
        Some(gander)
    }
}

/// Returns `true` if `window.gander` is available.
pub fn is_available() -> bool {
    get_gander().is_some()
}

/// Call `window.gander.send(text)`.
///
/// Queues `text` as the user's message and signals the host to start
/// streaming the assistant reply.  Events are delivered to the callback
/// previously registered with [`subscribe`].
///
/// No-ops (with a console warning) if `window.gander` is not present.
pub fn send(text: &str) {
    let Some(gander) = get_gander() else {
        web_sys::console::warn_1(&JsValue::from_str(
            "gander-chat: window.gander is not available; send() is a no-op",
        ));
        return;
    };

    let send_fn: Function = js_sys::Reflect::get(&gander, &JsValue::from_str("send"))
        .ok()
        .and_then(|v| v.dyn_into::<Function>().ok())
        .expect("window.gander.send must be a function; ensure the host initializes the bridge before WASM loads");

    let _ = send_fn.call1(&gander, &JsValue::from_str(text));
}

/// Call `window.gander.subscribe(callback)`.
///
/// Registers `callback` as the receiver of streaming events.  Only one
/// callback is active at a time; a second call replaces the first.
///
/// No-ops (with a console warning) if `window.gander` is not present.
pub fn subscribe(callback: &Function) {
    let Some(gander) = get_gander() else {
        web_sys::console::warn_1(&JsValue::from_str(
            "gander-chat: window.gander is not available; subscribe() is a no-op",
        ));
        return;
    };

    let subscribe_fn: Function = js_sys::Reflect::get(&gander, &JsValue::from_str("subscribe"))
        .ok()
        .and_then(|v| v.dyn_into::<Function>().ok())
        .expect("window.gander.subscribe must be a function; ensure the host initializes the bridge before WASM loads");

    let _ = subscribe_fn.call1(&gander, callback);
}
