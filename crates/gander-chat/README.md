# gander-chat

A [Leptos](https://leptos.dev) CSR (client-side rendered) chat UI for gander.

Builds to a static WASM bundle (`dist/`) that the gander binary loads in a
per-tab [wry](https://github.com/tauri-apps/wry) webview.

The tab strip, header, and drawers remain in iced (COSMIC); only the tab
**body** is this Leptos client, loaded from a `data:` URL or embedded
asset.

---

## Building

Prerequisites:

```
rustup target add wasm32-unknown-unknown
cargo install trunk
```

**Release build** (produces `dist/`):

```bash
cd crates/gander-chat
trunk build --release
```

**Development server** (live-reloads in the browser at http://localhost:8080):

```bash
cd crates/gander-chat
trunk serve
```

The development server uses the mock `window.gander` defined in
`index.html` — no live gander instance needed.

---

## Bridge API (`window.gander`)

The host (gander binary) must inject a `window.gander` object **before** the
WASM module is instantiated.  The object must expose exactly two methods:

```ts
interface GanderBridge {
  /**
   * Queue `text` as the user's message and begin streaming the assistant
   * reply.  Events are delivered to the callback registered via subscribe().
   *
   * Only one in-flight request is active at a time.  Calling send() while
   * a stream is active is undefined behaviour; the UI prevents it by
   * disabling the Send button.
   */
  send(text: string): void;

  /**
   * Register a callback that receives streaming events.
   * Replaces any previously registered callback.
   * Called exactly once during WASM initialisation.
   */
  subscribe(callback: (event: BridgeEvent) => void): void;
}
```

### Event types

All events are plain JS objects with a `type` discriminant.

#### `agent_text`

Appends a text chunk to the current in-flight assistant message. Used for
both live streaming and history replay.

```ts
{ type: "agent_text"; content: string }
```

`content` is a raw text fragment (not HTML).  Chunks are appended in order.
The client renders the accumulated text as Markdown at each update.

#### `user_text`

Appends a completed user message. Used during history replay only.

```ts
{ type: "user_text"; content: string }
```

#### `tool_use`

Signals the agent invoked a tool.

```ts
{ type: "tool_use"; name: string; input: string }
```

#### `tool_result`

Signals a tool returned output.

```ts
{ type: "tool_result"; name: string; output: string }
```

#### `session_load_start`

Clears the message list and enters replay mode (input disabled).

```ts
{ type: "session_load_start" }
```

#### `session_load_end`

Exits replay mode (input re-enabled).

```ts
{ type: "session_load_end" }
```

#### `done`

Signals that streaming for the current message is complete.

```ts
{ type: "done" }
```

#### `error`

Signals that the stream ended abnormally.  The in-flight message is marked
with the error text.

```ts
{ type: "error"; message: string }
```

### Sequence diagram

```
User types, clicks Send
  │
  ▼
client calls window.gander.send(text)
  │
  ▼
host starts streaming
  │
  ├─▶  {type:"agent_text", content:"Hello"}
  ├─▶  {type:"agent_text", content:" world"}
  │    … (many chunks at ~60/s)
  └─▶  {type:"done"}
```

History replay (clicking a session in the sidebar):

```
client calls window.gander.post({type:"session_select", id:"…"})
  │
  ├─▶  {type:"session_load_start"}
  ├─▶  {type:"user_text", content:"User said this"}
  ├─▶  {type:"agent_text", content:"Agent replied"}
  ├─▶  {type:"done"}
  └─▶  {type:"session_load_end"}
```

### Extending the protocol

Unknown `type` values are silently ignored, so the host can add new event
types without breaking older client builds.  New required fields on existing
types are breaking changes.

---

## Architecture

```
src/
  lib.rs       — Leptos app, components (App, MessageList, MessageView)
  bridge.rs    — window.gander JS bindings via wasm-bindgen + js-sys
  markdown.rs  — Markdown → HTML via pulldown-cmark
index.html     — Trunk entry point + development mock bridge
Trunk.toml     — Trunk build config
```

### Fine-grained reactivity

Each `ChatMessage` holds its `content` as a `RwSignal<String>`.  Appending a
event only re-evaluates the single `inner_html` binding on that message's
`<div>`, not the whole list.  At 60 chunks/second this avoids per-chunk
virtual DOM diffing.

### Portability

The bridge API is the only coupling to gander.  Any host that implements
`window.gander.send` / `subscribe` can load this client.  For use outside
gander, implement a shim that speaks to your own ACP-compatible agent.

---

## Workspace integration

`gander-chat` is a member of the root workspace (`Cargo.toml`), but it is
**excluded from `default-members`** so that `cargo build` at the workspace
root builds only the native gander binary.  CI clippy/test/doc steps are
scoped to `-p gander` for the same reason.

To build or check gander-chat explicitly:

```bash
# native check (limited — some wasm-bindgen APIs require the wasm32 target)
cargo check -p gander-chat --target wasm32-unknown-unknown

# full WASM build via trunk
cd crates/gander-chat && trunk build --release
```

---

## Out of scope (this crate)

- gander-side webview integration (separate issue)
- ACP plumbing (tracked in phlax/gander#14 and phlax/gander#15)
- Authentication, history persistence, multi-conversation
- Syntax highlighting beyond CSS classes (future: syntect compiled to WASM,
  or Prism.js via JS interop)
- Embedding `dist/` in the gander binary (future: `include_dir!`)
