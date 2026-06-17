// SPDX-License-Identifier: GPL-3.0-or-later

//! Leptos chat client for gander.
//!
//! # Architecture
//!
//! ```text
//! App
//! ├── .sidebar (div)
//! │   └── .concertina (shared scroll container)
//! │       ├── Sidebar      (Sessions section + list — acp_core)
//! │       └── Concertina   (Extensions / Settings — goose_ext)
//! └── .gander-chat
//!     │  // exactly one of:
//!     ├── ChatPane              (when pane_view == Chat)
//!     │   ├── MessageList       (scrollable, one MessageView per message)
//!     │   ├── InputRow          (textarea + Send button)
//!     │   └── Footer            (cwd, attach, tokens, model, mode, tools, settings)
//!     └── AllSessions           (when pane_view == AllSessions)
//! ```
//!
//! All chat state lives in [`App`] as [`leptos::RwSignal`]s.  Because
//! Leptos signals are `Copy`, they can be freely captured in closures
//! without reference-counting.
//!
//! ## Streaming
//!
//! Live token streaming:
//!
//! 1. User submits text → `bridge::send(text)` called.
//! 2. A user bubble and an in-flight assistant bubble are appended.
//! 3. The JS bridge fires `{type:"agent_text", content:"…"}` events.
//! 4. Each chunk calls `msg.content.update(|c| c.push_str(chunk))`.
//! 5. Leptos patches only the single changed text node — no vdom diff.
//! 6. A `{type:"done"}` event marks the message complete.
//!
//! ## History replay
//!
//! On session load:
//!
//! 1. `session_load_start` → clear message list, set `replaying=true`.
//! 2. `user_text` → append a completed user bubble.
//! 3. `agent_text` → create (if needed) and append to in-flight agent bubble.
//! 4. `tool_use` / `tool_result` → append a tool bubble.
//! 5. `done` → finalize in-flight agent bubble.
//! 6. `session_load_end` → clear `replaying`, re-enable input.
//!
//! ## Session sidebar
//!
//! On startup the WASM sends `{type:"ready"}` to the host, which responds
//! with `{type:"session_list", sessions:[…]}` and
//! `{type:"session_active", id:"…"}`.  Clicking a session fires
//! `session_select`; clicking "+ New session" fires `session_new`.
//!
//! ## Footer
//!
//! The host fires `{type:"session_info", cwd:"…", model:"…", tool_count:N}`
//! (or `tool_count:null` when unavailable) after the bridge `ready` handshake.
//! Each field drives its own signal so only the changed span re-renders.
//!
//! ## Sidebar layout
//!
//! All three sidebar sections (Sessions, Extensions, Settings) are
//! concertina-style: a header that toggles a body open/closed.  The
//! Sessions section is owned by `acp_core` and the other two by
//! `goose_ext`; they share the `.concertina` scroll wrapper so a long
//! session list doesn't push the goose-side rows off the bottom.
//!
//! ## Right-pane view switching
//!
//! `pane_view: RwSignal<ChatPaneView>` controls what fills the right
//! pane.  Default is `Chat` (message list + input + footer).  The
//! sidebar's "View all sessions →" link flips it to `AllSessions`,
//! and selecting a session anywhere flips it back to `Chat`.  The
//! sidebar itself is unaffected.
//!
//! # Entry point
//!
//! [`main`] is called by the Trunk-generated JS loader when the WASM
//! module is instantiated.

use leptos::prelude::*;
use wasm_bindgen::prelude::*;

pub mod acp_core;
pub mod bridge;
pub mod goose_ext;
pub mod markdown;

use acp_core::components::{AllSessions, Footer, InputRow, MessageList, Sidebar};
use acp_core::types::{ChatMessage, ChatPaneView, SessionEntry};
use goose_ext::components::Concertina;

// Re-export McpAppIframe so that acp_core sub-modules can import it without
// referencing the goose_ext path directly (keeps acp_core/ free of
// extension-layer path references, which the CI guard checks).
pub use goose_ext::components::McpAppIframe;

// ─── App ─────────────────────────────────────────────────────────────────────

/// Root application component.
///
/// Owns all chat and session state and wires up the JS bridge subscription.
/// Composes `<Sidebar/>` and `<Concertina/>` directly inside the `.sidebar`
/// div so the two halves of the sidebar chrome live in their respective
/// modules while the outer wrapper stays here.
#[component]
pub fn App() -> impl IntoView {
    let messages: RwSignal<Vec<ChatMessage>> = RwSignal::new(Vec::new());
    let next_id: RwSignal<u32> = RwSignal::new(0);
    let input_text: RwSignal<String> = RwSignal::new(String::new());
    // True while an assistant reply is being streamed (live prompt).
    let sending: RwSignal<bool> = RwSignal::new(false);
    // True while a session's history is being replayed.
    let replaying: RwSignal<bool> = RwSignal::new(false);
    // The assistant message currently receiving tokens, if any.
    let in_flight: RwSignal<Option<ChatMessage>> = RwSignal::new(None);
    // Session sidebar state.
    let sessions: RwSignal<Vec<SessionEntry>> = RwSignal::new(Vec::new());
    let active_session_id: RwSignal<Option<String>> = RwSignal::new(None);
    // Footer metadata — populated when the host fires `session_info`.
    let footer_cwd: RwSignal<Option<String>> = RwSignal::new(None);
    let footer_model: RwSignal<Option<String>> = RwSignal::new(None);
    let footer_tool_count: RwSignal<Option<u32>> = RwSignal::new(None);
    // Which view fills the right-hand pane.  Default to the chat
    // conversation; the sidebar's "View all sessions" link flips this
    // to AllSessions and selecting a session flips it back to Chat.
    let pane_view: RwSignal<ChatPaneView> = RwSignal::new(ChatPaneView::Chat);

    // Register the event callback once for the lifetime of the app.
    // The Closure is leaked intentionally — it must outlive the app.
    // Both handlers receive every event; they process disjoint event types
    // (acp_core: standard ACP events; goose_ext: tool_resource only).
    // Convention: acp_core first, goose_ext second.
    {
        let cb = Closure::wrap(Box::new(move |event: JsValue| {
            acp_core::events::handle_acp_core_bridge_event(
                &event,
                in_flight,
                sending,
                replaying,
                messages,
                next_id,
                sessions,
                active_session_id,
                footer_cwd,
                footer_model,
                footer_tool_count,
            );
            goose_ext::events::handle_goose_ext_bridge_event(&event, messages);
        }) as Box<dyn FnMut(JsValue)>);

        bridge::subscribe(cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // Tell the host that the bridge is ready; it will respond with
    // session_list and session_active so the sidebar populates.
    bridge::post_json(r#"{"type":"ready"}"#);

    view! {
        <div class="gander-root">
            <div class="sidebar">
                // Single `.concertina` scroll container holding every
                // sidebar section.  Sessions (acp_core) is the primary
                // navigation row; Extensions / Settings (goose_ext)
                // live below it and share the same scroll viewport so
                // a long session list doesn't push them off-screen.
                <div class="concertina">
                    <Sidebar sessions active_session_id pane_view />
                    <Concertina />
                </div>
            </div>
            <div class="gander-chat">
                {move || {
                    match pane_view.get() {
                        ChatPaneView::Chat => view! {
                            <MessageList messages />
                            <InputRow input_text sending replaying next_id messages in_flight />
                            <Footer cwd=footer_cwd model=footer_model tool_count=footer_tool_count />
                        }.into_any(),
                        ChatPaneView::AllSessions => view! {
                            <AllSessions
                                sessions
                                active_session_id
                                view=pane_view
                            />
                        }.into_any(),
                    }
                }}
            </div>
        </div>
    }
}

// ─── Entry point ─────────────────────────────────────────────────────────────

/// Application entry point called by the Trunk-generated JS loader.
///
/// Marked `#[wasm_bindgen(start)]` so that wasm-bindgen exports it as the
/// WASM module start function; the Trunk-generated JS calls it automatically
/// when the module is instantiated.
#[wasm_bindgen(start)]
pub fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
