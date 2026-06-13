# WebKitGTK / wry spike notes

**Branch:** `webview` (replaces the `iced_webview_v2` attempt)  
**Goal:** Embed a real production webview in a gander profile tab using `wry` directly against WebKitGTK on Linux, showing a styled `data:text/html` page so we can confirm HTML + CSS rendering.

---

## What was built

Each open profile tab gets its own `wry::WebView` instance created as a child of the iced application window. The webview loads a `data:text/html;base64,…` URL containing:

```html
<h1>goose: <profile-name></h1>
<p>WebKitGTK webview — spike POC</p>
```

…with a blue border, `sans-serif` font, and a light-blue background so CSS application is visually obvious. When a tab is activated its webview is shown; when hidden (other tab, picker) it is hidden via `WebView::set_visible(false)`. When the tab is closed the `WebView` is dropped (its `Drop` impl destroys the underlying GTK widget and X11 window).

The integration lives in:

| File | Role |
|------|------|
| `src/webview.rs` | All wry/GTK code: `WebviewStore`, thread-local `PENDING`, `build_data_url`, `create_child_webview` |
| `src/app.rs` | Wires lifecycle: create on `OpenTab`/`CreateProfile`/`GotMainWindowId`, show/hide on `ActivateTab`/`ShowNewTabPicker`/`HideNewTabPicker`, destroy on `CloseTab`, resize on `WindowResized`, GTK pump on `PumpGtk` |
| `src/main.rs` | Calls `gtk::init()` before `cosmic::app::run()` |
| `Cargo.toml` | `wry = "0.55"`, `gtk = "0.18"`, `raw-window-handle = "0.6"` (all Linux-only target deps) |
| `.github/workflows/ci.yml` | Adds `libwebkit2gtk-4.1-dev` to both `check` and `build` job apt-get steps |

---

## Architecture decisions

### Thread-local workaround for `!Send WebView`

`wry::WebView` is `!Send` because it wraps GTK GObjects which are thread-local. `iced::window::run_with_handle` takes a closure whose return type `T` must be `Send`.

**Workaround:** the `run_with_handle` closure stores the newly-created `WebView` in a `thread_local! { static PENDING: RefCell<HashMap<Entity, WebView>> }` and returns `Message::WebviewReady(entity)` (which is `Send`). The subsequent `update(WebviewReady(entity))` call runs on the same GUI main thread and retrieves the `WebView` via `claim_pending`. Since both the closure and `update()` run on the main thread, the thread-local is shared correctly.

### GTK event loop pump

Without pumping the GTK event loop, the WebKitGTK surface renders once on creation and then freezes — no repainting, no input handling, no animation. A `cosmic::iced::time::every(16ms)` subscription fires `Message::PumpGtk` at ~60 fps; the `update()` handler calls `gtk::main_iteration()` in a loop while `gtk::events_pending()`. This runs on the GUI main thread where GTK is safe to call.

### Bounds approximation

The webview must be positioned to avoid painting under the tab strip. The tab strip height is approximated as `TAB_STRIP_HEIGHT = 40.0` logical pixels (32px button height + a few pixels top/bottom spacing). Accurate per-widget bounds would require hooking into iced's layout phase, which is not exposed by the current `Element` API. This is left as a follow-up.

Window size is tracked by subscribing to `iced::window::resize_events()`. The initial size is set to the declared minimum (640×400) and updated on the first `WindowResized` event.

---

## Known unknowns and findings

### X11 only — Wayland is a no-op

`wry::WebViewBuilder::build_as_child` on Linux (the WebKitGTK backend) only supports `RawWindowHandle::Xlib`. On a Wayland session, `iced::window::run_with_handle` returns a `WaylandWindowHandle` and `build_as_child` returns `Err(wry::Error::UnsupportedWindowHandle)` immediately.

The code handles this gracefully: `create_child_webview` logs a `tracing::warn!` and returns `false`; the `WebviewReady` message is still dispatched but `claim_pending` finds nothing and is a no-op. The tab body falls back to the iced placeholder text.

**Why this matters:** gander's libcosmic build links `wayland` as a feature, so in practice COSMIC will be running under Wayland. The webview will only render in an X11 session (e.g., `DISPLAY=:0 WAYLAND_DISPLAY= cargo run` or XWayland with forced X11 window). Under Wayland the tab shows the text placeholder unchanged — no crash, but also no webview.

This is the primary limitation of this POC. Paths forward:

- **wry 0.56+ / tao:** The wry roadmap includes Wayland subsurface support for WebKitGTK via `wl_subsurface`. Once that lands, switching to it should be straightforward.
- **Separate X11 process:** Run the webview in a child process with `DISPLAY` set; embed the X11 window into the Wayland surface via XWayland embedding (complex, not recommended).
- **webkit2gtk directly:** Bypass wry and use `webkit2gtk` + GTK-layer-shell or a GtkPlug/GtkSocket pattern. More work, same Wayland limitation.

### GTK + winit init order

`gtk::init()` is called in `main()` before `cosmic::app::run()`. This works cleanly: libcosmic/winit uses Wayland (via `wayland-client`) directly and does not go through GTK, so GTK initialisation has no side-effects on the iced window stack. It just opens the X11 display connection that GTK and WebKitGTK need internally.

No ordering issues were observed in testing. The concern about GTK + winit conflicts does not apply here because they use separate display connections.

### Multiple webviews on the same parent window

On X11, multiple `build_as_child` calls on the same parent window worked correctly in testing — each tab gets its own child X11 window, shown/hidden independently. The `PENDING` map keyed by `segmented_button::Entity` keeps them separate.

On Wayland this is moot (all webviews fall back to no-op as described above).

### `WebView::set_visible(false)` and Z-order

When a tab is deactivated, `set_visible(false)` calls the underlying GTK `gtk_widget_hide()`, which unmaps the X11 window. This is reliable and doesn't leave the webview floating over other content. On activation, `set_visible(true)` remaps it. No Z-order issues were observed.

### `window::oldest()` returning `None`

`cosmic::iced::window::oldest()` returns `Task<Option<Id>>`. In the current libcosmic, this reliably returns the main window's ID during `init()`. The `None` arm falls back to `Id::RESERVED` (value 1, the reserved "first window" slot). In practice this branch is never taken.

---

## System requirements

```
# Debian / Ubuntu
sudo apt-get install libwebkit2gtk-4.1-dev

# Fedora
sudo dnf install webkit2gtk4.1-devel

# Arch
sudo pacman -S webkit2gtk-4.1
```

The package transitively brings in `libgtk-3-dev`, `libsoup-3.0-dev`, and `libjavascriptcoregtk-4.1-dev`.

---

## What's out of scope (follow-ups)

- Pointing the webview at a real goosed HTTP server (follows from phlax/gander#5)
- `window.electron.*` polyfill shim for the goose React UI
- Wayland subsurface support (upstream wry work needed)
- Accurate tab-content bounds from iced's layout phase
- X11 tab bar height measured dynamically rather than approximated
