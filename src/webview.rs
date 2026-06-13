// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-tab WebView management for gander.
//!
//! This module is **Linux-only** and uses [wry] with its WebKitGTK backend.
//!
//! ## Architecture
//!
//! Each open profile tab gets its own `wry::WebView` instance, giving strict
//! per-tab isolation (separate browsing context, cookies, storage). The WebView
//! is created when the tab is first opened and destroyed when the tab is closed.
//!
//! ## Integration shape
//!
//! Creating a wry WebView requires a `HasWindowHandle` reference to the parent
//! window. The iced/winit runtime provides this through
//! `iced::window::run_with_handle`, which runs a closure synchronously on the
//! GUI thread with the live `WindowHandle`.
//!
//! Because `wry::WebView` is `!Send` (GTK objects are thread-local), we cannot
//! return it directly from the `run_with_handle` closure (whose return type
//! must be `Send`). Instead the closure stores the newly-created `WebView` in
//! the thread-local [`PENDING`] map, then returns a `Message::WebviewReady`
//! sentinel. The subsequent `update(WebviewReady)` call retrieves the WebView
//! via [`claim_pending`] and moves it into `WebviewStore`.
//!
//! ## Bounds tracking
//!
//! The WebView is positioned by giving wry an X11 child window position +
//! size at creation time, and then updating that position+size via
//! `WebView::set_bounds` whenever the tab body's on-screen rectangle
//! changes. The active tab body widget is wrapped in libcosmic's
//! `cosmic::widget::rectangle_tracker`, which reports `(x, y, w, h)` on every
//! draw. See `app.rs::Message::TabBodyRect`.
//!
//! ## wry 0.55 X11 `set_bounds` move-is-a-no-op bug
//!
//! On X11, `wry::WebView::set_bounds` calls `gtk::Window::move_` on a
//! foreign-window-wrapping `gtk::Window`. That `move_` is a silent no-op
//! because there is no window manager managing the child window — the only
//! authoritative position is the one passed to `XCreateSimpleWindow` at
//! construction time. The resize half of `set_bounds` works (it goes through
//! `XResizeWindow`); only the move is broken.
//!
//! Net effect: the WebView ends up wherever it was first placed and stays
//! there. To work around this we wrap *all* pages (not just `Page::Tab`) in
//! the rectangle tracker, so the very first iced draw — which happens before
//! the user has had a chance to open a tab — already populates
//! `tab_body_bounds`. By the time `create_child_webview` runs we have the
//! true rectangle to give to wry up front.
//!
//! See: <https://github.com/tauri-apps/wry/blob/wry-v0.55.1/src/webkitgtk/mod.rs#L853>
//!
//! ## Known limitations
//!
//! - **X11 only**: `wry::WebViewBuilder::build_as_child` on Linux (the
//!   WebKitGTK path) only supports `RawWindowHandle::Xlib`. On a Wayland
//!   session `run_with_handle` returns a `WaylandWindowHandle` and
//!   `build_as_child` immediately returns
//!   `Err(wry::Error::UnsupportedWindowHandle)`. In that case we log a warning
//!   and leave the tab body as-is (no webview, just the iced placeholder).
//!   See `docs/webview-spike.md` for the full analysis.
//!
//! - **GTK event loop**: wry embeds a `webkit2gtk::WebView` (a GTK widget)
//!   inside an X11 container window. GTK widgets require their event loop
//!   (`gtk::main_iteration`) to be pumped regularly for painting, input, and
//!   animation. The app drives this via a 60 fps subscription that sends a
//!   `Message::PumpGtk` on every tick; `update` calls `gtk::main_iteration_do`
//!   synchronously on the GUI thread. Without this the webview surface renders
//!   once and then freezes.
//!
//! - **Repositioning after creation**: thanks to the wry bug above, `y`
//!   changes (e.g. opening the profile-config context drawer if/when that
//!   ever resizes the tab body vertically) will *resize* but not *move* the
//!   webview. Width changes work. Tracked as a follow-up.
//!
//! [wry]: https://github.com/tauri-apps/wry

use std::{borrow::Cow, cell::RefCell, collections::HashMap};

use cosmic::widget::segmented_button;
use slotmap::Key as _;
use tokio::sync::mpsc;
use wry::{
    Rect, WebView, WebViewBuilder,
    dpi::{LogicalPosition, LogicalSize},
};

use crate::acp::AcpCommand;

/// Embedded assets from `crates/gander-chat/dist/`.
///
/// Built by `cargo xtask build-chat [--release]` and embedded at compile time
/// via `rust-embed`. The `gander://chat/<path>` custom protocol handler serves
/// these bytes to the per-tab WebView.
#[derive(rust_embed::RustEmbed)]
#[folder = "crates/gander-chat/dist/"]
struct ChatAssets;

/// Initialization script injected into every tab WebView before the WASM
/// module instantiates.
///
/// Satisfies the bridge contract documented in `crates/gander-chat/src/bridge.rs`.
/// `send` posts a JSON-encoded `{type:"prompt",text:...}` message to the
/// native IPC handler. `post` posts an arbitrary JSON message. `subscribe`
/// wires up the subscriber list. `_publish` is called by gander (via
/// `evaluate_script`) to fan events out to all registered subscribers.
const BRIDGE_SCRIPT: &str = r#"
window.gander = (function () {
    const subscribers = [];
    return {
        send: function (text) {
            window.ipc.postMessage(JSON.stringify({ type: 'prompt', text: text }));
        },
        post: function (message) {
            window.ipc.postMessage(JSON.stringify(message));
        },
        subscribe: function (cb) {
            subscribers.push(cb);
            return function () {
                const i = subscribers.indexOf(cb);
                if (i >= 0) subscribers.splice(i, 1);
            };
        },
        _publish: function (event) {
            for (const cb of subscribers) {
                try { cb(event); } catch (e) { console.error('[gander] subscriber error', e); }
            }
        },
    };
})();
"#;

/// Coarse pixel height of the COSMIC header + tab strip stack, used **only**
/// as a first-frame fallback before the rectangle tracker has populated
/// `tab_body_bounds`.
///
/// On the COSMIC build we target this is ~80px (header ≈ 48 + tab strip ≈
/// 32). The value is deliberately approximate: any time the rectangle
/// tracker has fired at least once, that value supersedes this constant.
///
/// The reason this matters more than it sounds: see the
/// "wry 0.55 X11 `set_bounds` move-is-a-no-op" bug documented at the top of
/// this file. The webview's initial position is the *only* position it will
/// ever have. If we create the WebView using this fallback and then the
/// tracker reports the real value, the webview will still be sitting where
/// the fallback put it. So this constant needs to be close to the real
/// header height, not just "good enough for a frame".
pub const TAB_STRIP_HEIGHT: f64 = 80.0;

/// Ratio between iced's logical pixels and the device pixels that gtk's
/// X11-child-window backend expects from `set_bounds` / `with_bounds`.
///
/// libcosmic / iced report widget rectangles in *logical* pixels (1024x768
/// on a 1.5x-scaled 1536x1152 monitor). `wry::WebView::set_bounds`
/// forwards the size to `gtk::Window::resize` on the per-webview child
/// window. Toplevel gtk windows compensate for the GDK monitor scale;
/// child windows do not -- gtk treats the number as device pixels. So
/// asking wry for 1015 x 609 produces a 1015 device-pixel surface, which
/// is only 67% of the iced body on a 1.5x display.
///
/// We multiply position and size by this value on the way to wry so the
/// child window lands at the right device-pixel rectangle.
///
/// **Hack.** Hardcoded 1.5 because that is the cosmic build we currently
/// target. Will be wrong on 1.0 / 1.25 / 2.0 scales. TODO: query
/// `gtk::Display::default().unwrap().monitor(0).unwrap().scale_factor()`
/// after `gtk::init()`. Until then override with `GANDER_WEBVIEW_DPR=<n>`.
pub fn display_scale() -> f64 {
    std::env::var("GANDER_WEBVIEW_DPR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.5)
}

// ---------------------------------------------------------------------------
// Pending-webview thread-local
// ---------------------------------------------------------------------------

// `wry::WebView` is `!Send` because it wraps GTK GObjects. We can't return it
// from `iced::window::run_with_handle`'s closure (which requires `T: Send`).
// The workaround: store the WebView here from inside the closure and retrieve
// it via `claim_pending` in the next `update()` call.
//
// Both the closure and `update()` run on the iced GUI thread (the main
// thread), so they share the same thread-local instance.
thread_local! {
    static PENDING: RefCell<HashMap<segmented_button::Entity, WebView>> =
        RefCell::new(HashMap::new());
}

/// Store a newly-created `WebView` in the thread-local pending map.
///
/// Called from inside `iced::window::run_with_handle` closures; retrieved by
/// [`claim_pending`] in the `update()` call triggered by `Message::WebviewReady`.
pub fn store_pending(entity: segmented_button::Entity, view: WebView) {
    PENDING.with(|p| p.borrow_mut().insert(entity, view));
}

/// Take a previously stored pending `WebView` out of the thread-local map.
///
/// Returns `None` if no pending WebView exists for `entity` (e.g., if
/// `build_as_child` failed on Wayland).
pub fn claim_pending(entity: segmented_button::Entity) -> Option<WebView> {
    PENDING.with(|p| p.borrow_mut().remove(&entity))
}

// ---------------------------------------------------------------------------
// Chat URL helper
// ---------------------------------------------------------------------------

/// Build the `gander://chat/index.html?profile=…&tab_id=…` URL loaded by each
/// tab's WebView.
///
/// `tab_id` is the entity index; the chat UI stamps every outbound IPC message
/// with it so gander can route replies to the correct `AcpConnection`.
///
/// The profile name is percent-encoded so that special characters (`&`, `?`,
/// `#`, spaces, …) cannot break the URL or leak into adjacent query params.
pub fn build_chat_url(profile: &str, tab_id: u64) -> String {
    let encoded = percent_encode(profile);
    format!("gander://chat/index.html?profile={encoded}&tab_id={tab_id}")
}

/// Percent-encode a string for use as a URL query-parameter value.
///
/// Encodes every byte that is not in the unreserved set (`A-Z a-z 0-9 - _ . ~`)
/// as `%XX` using uppercase hex digits, matching RFC 3986 §2.3.
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b => {
                let hi = b >> 4;
                let lo = b & 0xf;
                out.push('%');
                out.push(char::from(if hi < 10 { b'0' + hi } else { b'A' + hi - 10 }));
                out.push(char::from(if lo < 10 { b'0' + lo } else { b'A' + lo - 10 }));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Custom-protocol handler
// ---------------------------------------------------------------------------

/// Build the `gander://chat/<path>` custom-protocol handler.
///
/// Looks up `path` in the embedded `ChatAssets` (`crates/gander-chat/dist/`)
/// and returns the bytes with an appropriate `Content-Type` header.  Returns
/// 404 if the asset is not found.
fn make_protocol_handler(
    _id: &str,
    request: wry::http::Request<Vec<u8>>,
) -> wry::http::Response<Cow<'static, [u8]>> {
    // Strip the scheme + host prefix to get the bare path.
    // Request URLs look like "gander://chat/index.html?profile=default".
    let uri = request.uri().to_string();
    let path = uri
        .strip_prefix("gander://chat/")
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("");

    // Map empty / root to the entry point.
    let asset_path = if path.is_empty() || path == "/" {
        "index.html"
    } else {
        path
    };

    match ChatAssets::get(asset_path) {
        Some(asset) => {
            let mime = mime_for(asset_path);
            wry::http::Response::builder()
                .status(200)
                .header("Content-Type", mime)
                .body(Cow::Owned(asset.data.into_owned()))
                .expect("response builder failed")
        }
        None => wry::http::Response::builder()
            .status(404)
            .body(Cow::Borrowed(b"" as &[u8]))
            .expect("response builder failed"),
    }
}

/// Return a `Content-Type` string for a file path based on its extension.
fn mime_for(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "html" => "text/html",
        "js" | "mjs" => "application/javascript",
        "wasm" => "application/wasm",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------------------
// WebviewStore
// ---------------------------------------------------------------------------

/// Owns all active per-tab `WebView` instances.
///
/// One `WebviewStore` lives in `AppModel` (Linux-only). Entries are created
/// via `claim_pending` after `run_with_handle` fires, and removed when tabs
/// are closed.
pub struct WebviewStore {
    views: HashMap<segmented_button::Entity, WebView>,
}

impl WebviewStore {
    pub fn new() -> Self {
        Self {
            views: HashMap::new(),
        }
    }

    /// Consume the pending WebView stored by the `run_with_handle` closure and
    /// move it into the live store.
    ///
    /// The WebView is initially hidden; call `show_only` to make it visible.
    pub fn claim_pending(&mut self, entity: segmented_button::Entity) {
        if let Some(view) = claim_pending(entity) {
            if let Err(err) = view.set_visible(false) {
                tracing::warn!(?entity, %err, "set_visible(false) on claim failed");
            }
            self.views.insert(entity, view);
        }
    }

    /// Drop the WebView for `entity`, freeing its resources.
    ///
    /// Called when a tab is closed. `WebView`'s `Drop` impl destroys the
    /// underlying GTK widget and X11 window.
    pub fn destroy(&mut self, entity: segmented_button::Entity) {
        self.views.remove(&entity);
    }

    /// Show only the WebView for `entity`; hide every other WebView.
    ///
    /// Used when a tab is activated.
    pub fn show_only(&mut self, entity: segmented_button::Entity) {
        for (e, view) in &self.views {
            let visible = *e == entity;
            if let Err(err) = view.set_visible(visible) {
                tracing::warn!(entity = ?e, %err, "set_visible failed");
            }
        }
    }

    /// Hide all WebViews.
    ///
    /// Used when the picker page or empty page is shown.
    pub fn hide_all(&mut self) {
        for view in self.views.values() {
            if let Err(err) = view.set_visible(false) {
                tracing::warn!(%err, "set_visible(false) failed");
            }
        }
    }

    /// Update the bounds of the WebView for `entity`.
    ///
    /// `x`, `y`, `width`, `height` are logical (pre-DPI-scale) pixels.
    ///
    /// Note: due to the wry 0.55 X11 bug described at the top of this file,
    /// only the *size* half of this call is honoured after creation. The
    /// position is whatever `create_child_webview` passed in originally.
    pub fn set_bounds(
        &self,
        entity: segmented_button::Entity,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) {
        if let Some(view) = self.views.get(&entity) {
            Self::apply_bounds(view, x, y, width, height);
        }
    }

    /// Update the bounds of **all** WebViews to the same rectangle.
    ///
    /// Called on window resize so the resize half of `set_bounds` keeps the
    /// (currently hidden) webviews sized to match the visible content area.
    pub fn set_bounds_all(&self, x: f64, y: f64, width: f64, height: f64) {
        for view in self.views.values() {
            Self::apply_bounds(view, x, y, width, height);
        }
    }

    fn apply_bounds(view: &WebView, x: f64, y: f64, width: f64, height: f64) {
        let s = display_scale();
        if let Err(err) = view.set_bounds(Rect {
            position: LogicalPosition::new(x * s, y * s).into(),
            size: LogicalSize::new((width * s).max(1.0), (height * s).max(1.0)).into(),
        }) {
            tracing::warn!(%err, "set_bounds failed");
        }
    }

    /// Evaluate `js` in the WebView for `entity`.
    ///
    /// Used by the GTK pump to push [`AcpEvent`](crate::acp::AcpEvent) payloads
    /// into the chat UI via `window.gander._publish(...)`.
    pub fn evaluate_script(&self, entity: segmented_button::Entity, js: &str) {
        if let Some(view) = self.views.get(&entity) {
            if let Err(err) = view.evaluate_script(js) {
                tracing::warn!(?entity, %err, "evaluate_script failed");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WebView construction helper — called from inside run_with_handle closures
// ---------------------------------------------------------------------------

/// Attempt to create a wry `WebView` as a child of the window identified by
/// `handle`.
///
/// On success the WebView is stored in [`PENDING`] under `entity` and `true`
/// is returned. On failure (typically `UnsupportedWindowHandle` on a Wayland
/// session) a warning is logged and `false` is returned.
///
/// Note on bounds: because of the wry 0.55 X11 move-is-a-no-op bug, the
/// position passed in here is the *final* position of the webview for its
/// entire lifetime. Callers should make sure `initial_bounds` matches the
/// real on-screen tab body rectangle.
///
/// `cmd_tx` is wired into the WebView's IPC handler so that messages from
/// `window.gander.send(text)` are forwarded to the per-tab `AcpConnection`.
///
/// # Panics
///
/// Panics if `gtk::init()` was not called before this function (required by
/// wry's WebKitGTK backend).
pub fn create_child_webview(
    entity: segmented_button::Entity,
    profile: &str,
    handle: &impl raw_window_handle::HasWindowHandle,
    initial_bounds: Rect,
    cmd_tx: mpsc::Sender<AcpCommand>,
) -> bool {
    let tab_id = entity.data().as_ffi();
    let url = build_chat_url(profile, tab_id);

    match WebViewBuilder::new()
        .with_initialization_script(BRIDGE_SCRIPT)
        .with_custom_protocol("gander".into(), make_protocol_handler)
        .with_url(&url)
        .with_bounds(initial_bounds)
        .with_devtools(std::env::var("GANDER_DEVTOOLS").is_ok())
        .with_ipc_handler(move |request: wry::http::Request<String>| {
            let body = request.body();
            match serde_json::from_str::<serde_json::Value>(body) {
                Ok(json) => match json.get("type").and_then(|v| v.as_str()) {
                    Some("prompt") => {
                        if let Some(text) = json.get("text").and_then(|v| v.as_str()) {
                            if let Err(err) = cmd_tx.try_send(AcpCommand::Prompt(text.to_owned())) {
                                tracing::warn!(%err, "ipc handler: failed to send prompt");
                            }
                        }
                    }
                    Some("session_select") => {
                        if let Some(id) = json.get("id").and_then(|v| v.as_str()) {
                            if let Err(err) =
                                cmd_tx.try_send(AcpCommand::SessionSelect(id.to_owned()))
                            {
                                tracing::warn!(%err, "ipc handler: failed to send session_select");
                            }
                        }
                    }
                    Some("session_new") => {
                        if let Err(err) = cmd_tx.try_send(AcpCommand::SessionNew) {
                            tracing::warn!(%err, "ipc handler: failed to send session_new");
                        }
                    }
                    Some("ready") => {
                        if let Err(err) = cmd_tx.try_send(AcpCommand::RequestSessionInfo) {
                            tracing::warn!(%err, "ipc handler: failed to send ready");
                        }
                    }
                    _ => {}
                },
                Err(err) => {
                    tracing::warn!(%err, "ipc handler: invalid JSON from webview");
                }
            }
        })
        .build_as_child(handle)
    {
        Ok(view) => {
            // Pin webkit's zoom to 1.0 so CSS pixels render 1:1 against
            // the device-pixel surface we set up via `set_bounds`. wry
            // already pre-divides our `LogicalSize` by the monitor scale
            // factor (see `display_scale`), so any extra zoom here would
            // double-apply HiDPI scaling to the rendered page.
            if let Err(err) = view.zoom(1.0) {
                tracing::warn!(%err, "webview.zoom(1.0) failed");
            }
            store_pending(entity, view);
            true
        }
        Err(err) => {
            tracing::warn!(
                profile,
                %err,
                "wry build_as_child failed — webview will not be shown for this tab; \
                 on Wayland this is expected (only XlibWindowHandle is supported by the \
                 WebKitGTK backend, see docs/webview-spike.md)"
            );
            false
        }
    }
}
