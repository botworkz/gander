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
//! - **Bounds tracking**: The tab-content area position is approximated as
//!   `y = TAB_STRIP_HEIGHT`. Accurate per-widget bounds would require hooking
//!   into iced's layout phase, which is not exposed by the current `Element`
//!   API. Improving this is left as a follow-up.
//!
//! [wry]: https://github.com/tauri-apps/wry

use std::{cell::RefCell, collections::HashMap};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use cosmic::widget::segmented_button;
use wry::{
    Rect, WebView, WebViewBuilder,
    dpi::{LogicalPosition, LogicalSize},
};

/// Approximate pixel height of the COSMIC tab strip.
///
/// This is used to offset the webview so it doesn't paint under the tab bar.
/// A button height of 32 plus top/bottom spacing rounds to this value; exact
/// bounds require iced layout integration (tracked as a follow-up).
pub const TAB_STRIP_HEIGHT: f64 = 40.0;

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
// HTML / data-URL helper
// ---------------------------------------------------------------------------

/// Build the `data:text/html;base64,…` URL that each tab's WebView loads.
///
/// The page contains a styled `<h1>goose: {profile}</h1>` so we can visually
/// confirm that HTML *and* CSS are being applied — not just a blank surface.
pub fn build_data_url(profile: &str) -> String {
    let html = format!(
        "<!DOCTYPE html>\n\
         <html>\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <style>\n\
         body {{\n\
           font-family: sans-serif;\n\
           margin: 32px;\n\
           border: 3px solid #4a90d9;\n\
           border-radius: 8px;\n\
           padding: 24px;\n\
           background: #f0f7ff;\n\
         }}\n\
         h1 {{ color: #2c5282; margin: 0; }}\n\
         p  {{ color: #666; margin-top: 8px; }}\n\
         </style>\n\
         </head>\n\
         <body>\n\
           <h1>goose: {profile}</h1>\n\
           <p>WebKitGTK webview — spike POC</p>\n\
         </body>\n\
         </html>"
    );
    let encoded = BASE64.encode(html.as_bytes());
    format!("data:text/html;base64,{encoded}")
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
    /// The WebView is initially hidden; call [`show_only`] to make it visible.
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
    /// Called on window resize so that any tab's webview is correctly sized
    /// when it becomes active again.
    pub fn set_bounds_all(&self, x: f64, y: f64, width: f64, height: f64) {
        for view in self.views.values() {
            Self::apply_bounds(view, x, y, width, height);
        }
    }

    fn apply_bounds(view: &WebView, x: f64, y: f64, width: f64, height: f64) {
        if let Err(err) = view.set_bounds(Rect {
            position: LogicalPosition::new(x, y).into(),
            size: LogicalSize::new(width, height).into(),
        }) {
            tracing::warn!(%err, "set_bounds failed");
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
/// # Panics
///
/// Panics if `gtk::init()` was not called before this function (required by
/// wry's WebKitGTK backend).
pub fn create_child_webview(
    entity: segmented_button::Entity,
    profile: &str,
    handle: &impl raw_window_handle::HasWindowHandle,
    initial_bounds: Rect,
) -> bool {
    let url = build_data_url(profile);
    match WebViewBuilder::new()
        .with_url(&url)
        .with_bounds(initial_bounds)
        .build_as_child(handle)
    {
        Ok(view) => {
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
