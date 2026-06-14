# gander design

`gander` is a tabbed COSMIC client for goose. It depends on `geese` for profile
management (lib-level, not shell-out) and talks to one goose agent per tab over
the Agent Client Protocol (ACP v1).

## Goal

A single COSMIC window with a tab per goose conversation. The tab strip and
chrome are native iced/libcosmic; the tab body hosts a Leptos chat client in a
per-tab wry webview that talks to a per-tab goose process over ACP.

## Why this shape

Goose's May 2026 roadmap (aaif-goose/goose#9173) commits to ACP+ as the
standard protocol between goose clients and the harness, with multi-client
explicitly the design. This unblocks "build a native COSMIC client for goose"
as a real path. We're not embedding anyone else's UI; we're a first-class ACP
client.

The hybrid (iced shell + Leptos chat in webview) is chosen because:

- iced gives us COSMIC-native chrome (tab bar, header, drawers, theming)
- Leptos gives us fine-grained reactivity for streaming-token updates
  (the hot path in any chat UI) and access to web-ecosystem rendering
  (markdown, code highlighting, virtualised lists)
- The Leptos crate is built as a static WASM bundle and is theoretically
  reusable as a standalone web client against any ACP-speaking agent

## Known constraint: Wayland + wry

`wry::WebViewBuilder::build_as_child` does not support Wayland window handles
on Linux. It only supports `XlibWindowHandle`. Gander therefore requires
running under XWayland as v0. This is documented in the README. We track wry
upstream for native Wayland subsurface support; when it lands we revisit.

If it never lands, the fallback is to invert the shell (wry-owns-window,
Leptos renders the chrome too) — a "be Tauri" rewrite that we don't want to
do but is the escape hatch.

## Architecture

- `crates/gander` — iced/libcosmic app shell
  - `app.rs` — top-level AppModel, tab strip, picker, drawers (existing)
  - `tab.rs` — tab body, currently placeholder, becomes wry webview host
  - `supervisor.rs` — per-profile goose process lifecycle (landed in #5)
  - `acp/` — pure-ACP worker: session management, streaming, tool-call merging.
    Must not reference goose or geesed directly — enforced by the CI grep guard.
  - `transport/` — transport abstraction
    - `mod.rs` — `Transport` trait (`async fn connect(self: Box<Self>) -> Result<UnixStream, TransportError>`)
      and `TransportError`; callers get an abstraction point without generics
    - `geesed.rs` — `GeesedTransport` implements `Transport`; geesed-specific socket
      handshake, path helpers (`acp_socket_path`, `runtime_dir`), and `GeesedError`
      live here, keeping `acp/` free of geesed-private details
  - `ext/` — extension handler abstraction
    - `mod.rs` — `ExtHandler` trait (`async fn on_tool_call_completed`) plus `ExtEvent`
      and `ExtRequest` enums; the structural boundary between pure-ACP and goose-specific code
    - `goose.rs` — `GooseExtHandler` implements `ExtHandler`; inspects
      `_meta.goose.toolCall.extensionName` and emits `ExtRequest::ReadResource` for completed
      tool calls that carry a `rawOutput.resourceUri`.  `process_pending_fetches` and
      `extract_html_from_read_resource_response` live here (the only place
      `_goose/unstable/resources/read` is referenced).
  - `webview.rs` — per-tab wry webview loading the gander-chat bundle (issue not yet open)
- `crates/gander-chat` — Leptos chat UI (issue #16)
  - Talks to host via `window.gander.send()` / `subscribe()` bridge
  - Builds standalone via `trunk` so it can be developed in a browser
  - `acp_core/` — pure-ACP components and event handling (no goose-private surfaces):
    types, session sidebar, message list, tool-call cards, input row, footer
  - `goose_ext/` — goose-specific extensions: Concertina (Extensions + Settings drawer),
    MCP App iframe, and the `tool_resource` event handler that hydrates it

## Transport + ExtHandler boundary

`src/acp/` is kept strictly goose-free.  Two trait boundaries enforce this:

**`Transport`** (`src/transport/mod.rs`) — abstracts the connection phase.
`AcpConnection::connect_with_rx` takes `Box<dyn Transport>`.  Today only
`GeesedTransport` exists; `app.rs` constructs it and boxes it.  Error
downcasting (`downcast_ref::<GeesedError>()`) happens at the `app.rs` call
site — the trait itself is unaware of geesed.

**`ExtHandler`** (`src/ext/mod.rs`) — abstracts goose-specific side-effects.
`run_worker` takes `Box<dyn ExtHandler>` (promoted to `Arc` internally for
`Send` sharing).  After `drain_session_updates` the worker drains an internal
`ExtEvent` channel: `ReadResource` entries go onto `pending_fetches` for the
`process_pending_fetches` RPC; `SessionInfo` and `ToolResource` events go
straight to the UI channel.  The handler is fire-and-forget — a panic or
error logs at `warn!` and the ACP path continues uninterrupted.

### Adding a new goose extension method

1. Add a variant to `ExtRequest` in `src/ext/mod.rs` (e.g. `ListTools { ... }`).
2. Add a variant to `ExtEvent` if the response needs to reach the UI.
3. In `GooseExtHandler::on_tool_call_completed` (or a new `on_*` method if
   warranted), detect the condition and emit the new `ExtRequest` variant.
4. In `run_worker`'s handler-event drain loop (`src/acp/mod.rs`), add a match arm
   that processes the new variant — either queuing a fetch or forwarding to `ext_ui_tx`.
5. If the event reaches the UI, add a match arm in `ext_event_to_js` (`src/app.rs`)
   and a handler in the relevant `gander-chat` bridge listener.

When `_goose/unstable/resources/read` graduates to the ACP spec: drop
`src/ext/goose/fetch.rs` (or the fetch section of `goose.rs`), add
`read_resource` to `src/acp/`, and update `GooseExtHandler::on_tool_call_completed`
to call it.  Nothing else changes.

## Storage

(unchanged — `$XDG_DATA_HOME/gander/state.toml` for tab order; profile data
lives under `$GEESE_ROOT`, owned by geese)

## Active work

- #14 spike: prove ACP over stdio (PR #17) — answers "does goose speak ACP today"
- #15 acp client module (PR #18) — depends on #14
- #16 gander-chat Leptos crate (PR #19) — independent, parallel
- _follow-up_: webview-in-tab-body integration — opens after #14, #15, #16 land
