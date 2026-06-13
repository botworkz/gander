// SPDX-License-Identifier: GPL-3.0-or-later

//! Leptos chat client for gander.
//!
//! # Architecture
//!
//! ```text
//! App
//! ├── Sidebar       (session list, "+ New session" button)
//! └── ChatPane
//!     ├── MessageList   (scrollable, one MessageView per message)
//!     ├── input row     (textarea + Send button)
//!     └── Footer        (cwd, attach, tokens, model, mode, tools, settings)
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
//! # Entry point
//!
//! [`main`] is called by the Trunk-generated JS loader when the WASM
//! module is instantiated.

use leptos::prelude::*;
use leptos_icons::Icon;
use wasm_bindgen::prelude::*;

pub mod bridge;
pub mod markdown;

/// Fallback label used when a session has no title.
///
/// Must match `ListedSession`'s fallback in `src/acp.rs` (different crate —
/// keep both in sync if this string ever changes).
const DEFAULT_SESSION_LABEL: &str = "Session";

// ─── Data model ──────────────────────────────────────────────────────────────

/// Whether a message was written by the user or the assistant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    /// A tool invocation or result (gray, monospace-feel).
    Tool,
}

/// A single chat message.
///
/// All mutable fields are [`RwSignal`]s so that Leptos can surgically update
/// the DOM when tokens arrive, without diffing the whole message list.
///
/// The struct is [`Copy`] because every field is either a primitive or an
/// `RwSignal<T>` (which is itself `Copy` — it's just a typed arena ID).
#[derive(Clone, Copy, Debug)]
pub struct ChatMessage {
    /// Stable, unique ID used as the `key` in `<For>` to avoid re-mounts.
    pub id: u32,
    pub role: Role,
    /// Accumulated text.  Grows token-by-token for in-flight messages.
    pub content: RwSignal<String>,
    /// `true` while the host is still streaming tokens for this message.
    pub streaming: RwSignal<bool>,
    /// Non-`None` when the stream ended with an error.
    pub error: RwSignal<Option<String>>,
}

impl ChatMessage {
    fn new_user(id: u32, text: &str) -> Self {
        Self {
            id,
            role: Role::User,
            content: RwSignal::new(text.to_string()),
            streaming: RwSignal::new(false),
            error: RwSignal::new(None),
        }
    }

    fn new_assistant(id: u32) -> Self {
        Self {
            id,
            role: Role::Assistant,
            content: RwSignal::new(String::new()),
            streaming: RwSignal::new(true),
            error: RwSignal::new(None),
        }
    }

    fn new_tool(id: u32, text: String) -> Self {
        Self {
            id,
            role: Role::Tool,
            content: RwSignal::new(text),
            streaming: RwSignal::new(false),
            error: RwSignal::new(None),
        }
    }
}

/// A session entry shown in the sidebar.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionEntry {
    pub id: String,
    pub label: String,
    /// ISO 8601 `updated_at` from the host, if available.
    pub last_active: Option<String>,
}

// ─── Event handling ───────────────────────────────────────────────────────────

/// Dispatch a raw JS bridge event to the appropriate signal update.
///
/// Called from the `Closure` registered with `bridge::subscribe`.
/// Runs outside any reactive context, so signal reads use `get_untracked`.
#[allow(clippy::too_many_arguments)]
fn handle_bridge_event(
    event: JsValue,
    in_flight: RwSignal<Option<ChatMessage>>,
    sending: RwSignal<bool>,
    replaying: RwSignal<bool>,
    messages: RwSignal<Vec<ChatMessage>>,
    next_id: RwSignal<u32>,
    sessions: RwSignal<Vec<SessionEntry>>,
    active_session_id: RwSignal<Option<String>>,
    footer_cwd: RwSignal<Option<String>>,
    footer_model: RwSignal<Option<String>>,
    footer_tool_count: RwSignal<Option<u32>>,
) {
    let event_type = js_sys::Reflect::get(&event, &JsValue::from_str("type"))
        .ok()
        .and_then(|v| v.as_string());

    /// Consume and return the next message ID.
    fn take_id(next_id: RwSignal<u32>) -> u32 {
        let id = next_id.get_untracked();
        next_id.set(id + 1);
        id
    }

    match event_type.as_deref() {
        // ── agent text chunk (live or history replay) ──────────────────────
        Some("agent_text") => {
            let chunk = js_sys::Reflect::get(&event, &JsValue::from_str("content"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();

            // Re-use the existing in-flight bubble or start a new one.
            let msg = match in_flight.get_untracked() {
                Some(m) => m,
                None => {
                    let m = ChatMessage::new_assistant(take_id(next_id));
                    messages.update(|v| v.push(m));
                    in_flight.set(Some(m));
                    m
                }
            };
            msg.content.update(|c| c.push_str(&chunk));
        }

        // ── user text chunk (history replay only) ──────────────────────────
        Some("user_text") => {
            let text = js_sys::Reflect::get(&event, &JsValue::from_str("content"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();

            // A user message from history is always complete. If there is an
            // in-flight agent bubble, close it first to keep ordering correct.
            if let Some(msg) = in_flight.get_untracked() {
                msg.streaming.set(false);
                in_flight.set(None);
            }

            let m = ChatMessage::new_user(take_id(next_id), &text);
            messages.update(|v| v.push(m));
        }

        // ── tool call ──────────────────────────────────────────────────────
        Some("tool_use") => {
            let name = js_sys::Reflect::get(&event, &JsValue::from_str("name"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "(tool)".to_string());
            let input = js_sys::Reflect::get(&event, &JsValue::from_str("input"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();

            let text = format!("called **{}**\n```json\n{}\n```", name, input);
            let m = ChatMessage::new_tool(take_id(next_id), text);
            messages.update(|v| v.push(m));
        }

        // ── tool result ────────────────────────────────────────────────────
        Some("tool_result") => {
            let name = js_sys::Reflect::get(&event, &JsValue::from_str("name"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "(tool)".to_string());
            let output = js_sys::Reflect::get(&event, &JsValue::from_str("output"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();

            let text = if output.is_empty() {
                format!("**{}** returned (no output)", name)
            } else {
                format!("**{}** returned\n```\n{}\n```", name, output)
            };
            let m = ChatMessage::new_tool(take_id(next_id), text);
            messages.update(|v| v.push(m));
        }

        // ── session load start: clear UI, enter replay mode ────────────────
        Some("session_load_start") => {
            messages.set(Vec::new());
            in_flight.set(None);
            sending.set(false);
            replaying.set(true);
        }

        // ── session load end: leave replay mode ────────────────────────────
        Some("session_load_end") => {
            // Close any lingering in-flight bubble from the last replay turn.
            if let Some(msg) = in_flight.get_untracked() {
                msg.streaming.set(false);
                in_flight.set(None);
            }
            replaying.set(false);
        }

        Some("done") => {
            if let Some(msg) = in_flight.get_untracked() {
                msg.streaming.set(false);
            }
            in_flight.set(None);
            sending.set(false);
        }

        Some("error") => {
            let message = js_sys::Reflect::get(&event, &JsValue::from_str("message"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "Unknown error".to_string());

            if let Some(msg) = in_flight.get_untracked() {
                msg.error.set(Some(message));
                msg.streaming.set(false);
            }
            in_flight.set(None);
            sending.set(false);
            replaying.set(false);
        }

        Some("session_list") => {
            let raw_sessions = js_sys::Reflect::get(&event, &JsValue::from_str("sessions"))
                .ok()
                .filter(|v| v.is_array())
                .map(|v| js_sys::Array::from(&v))
                .unwrap_or_default();

            let mut entries: Vec<SessionEntry> = Vec::new();
            for i in 0..raw_sessions.length() {
                let item = raw_sessions.get(i);
                let id = js_sys::Reflect::get(&item, &JsValue::from_str("id"))
                    .ok()
                    .and_then(|v| v.as_string())
                    .unwrap_or_default();
                let label = js_sys::Reflect::get(&item, &JsValue::from_str("label"))
                    .ok()
                    .and_then(|v| v.as_string())
                    .unwrap_or_else(|| DEFAULT_SESSION_LABEL.to_string());
                let last_active = js_sys::Reflect::get(&item, &JsValue::from_str("last_active"))
                    .ok()
                    .and_then(|v| v.as_string());
                if !id.is_empty() {
                    entries.push(SessionEntry {
                        id,
                        label,
                        last_active,
                    });
                }
            }
            sessions.set(entries);
        }

        Some("session_active") => {
            let id = js_sys::Reflect::get(&event, &JsValue::from_str("id"))
                .ok()
                .and_then(|v| v.as_string());
            active_session_id.set(id);
            // Clear chat messages when a new session is created (via
            // `session_new` command). History replay uses `session_load_start`
            // instead and is handled separately above.
            messages.set(Vec::new());
            in_flight.set(None);
            sending.set(false);
        }

        // ── session metadata for the footer bar ───────────────────────────
        Some("session_info") => {
            let cwd = js_sys::Reflect::get(&event, &JsValue::from_str("cwd"))
                .ok()
                .and_then(|v| v.as_string());
            let model = js_sys::Reflect::get(&event, &JsValue::from_str("model"))
                .ok()
                .and_then(|v| v.as_string());
            // tool_count may be null (not yet supported) or a number.
            let tool_count = js_sys::Reflect::get(&event, &JsValue::from_str("tool_count"))
                .ok()
                .and_then(|v| v.as_f64())
                .map(|n| n as u32);

            if let Some(c) = cwd {
                footer_cwd.set(Some(c));
            }
            if let Some(m) = model {
                footer_model.set(Some(m));
            }
            footer_tool_count.set(tool_count);
        }

        _ => {
            // Ignore unrecognised events so future protocol extensions are
            // forwards-compatible.
        }
    }
}

// ─── Time-ago formatting ──────────────────────────────────────────────────────

/// Format an ISO 8601 timestamp as a short "time ago" string.
///
/// Uses `js_sys::Date` to parse the timestamp and compute the elapsed days
/// accurately, including correct handling of variable-length months.
fn time_ago(iso: &str) -> String {
    let then = js_sys::Date::new(&JsValue::from_str(iso));
    // `Date::new` with an unparseable string produces NaN for `getTime()`.
    let then_ms = then.get_time();
    if then_ms.is_nan() {
        return "(unknown)".to_string();
    }

    let now_ms = js_sys::Date::now();
    let diff_ms = now_ms - then_ms;
    if diff_ms < 0.0 {
        return "just now".to_string();
    }

    let diff_mins = (diff_ms / 60_000.0) as u64;
    let diff_hours = diff_mins / 60;
    let diff_days = diff_hours / 24;

    match diff_mins {
        0..=1 => "just now".to_string(),
        2..=59 => format!("{diff_mins}m ago"),
        60..=119 => "1h ago".to_string(),
        _ if diff_hours < 24 => format!("{diff_hours}h ago"),
        _ if diff_days == 1 => "yesterday".to_string(),
        _ if diff_days < 7 => format!("{diff_days}d ago"),
        _ if diff_days < 14 => "1w ago".to_string(),
        _ if diff_days < 30 => format!("{}w ago", diff_days / 7),
        _ if diff_days < 365 => format!("{}mo ago", diff_days / 30),
        _ => format!("{}y ago", diff_days / 365),
    }
}

// ─── Components ───────────────────────────────────────────────────────────────

/// Root application component.
///
/// Owns all chat and session state and wires up the JS bridge subscription.
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

    // Register the event callback once for the lifetime of the app.
    // The Closure is leaked intentionally — it must outlive the app.
    {
        let cb = Closure::wrap(Box::new(move |event: JsValue| {
            handle_bridge_event(
                event,
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
        }) as Box<dyn FnMut(JsValue)>);

        bridge::subscribe(cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // Tell the host that the bridge is ready; it will respond with
    // session_list and session_active so the sidebar populates.
    bridge::post_json(r#"{"type":"ready"}"#);

    // Submit the current input as a user message.
    let submit = move || {
        let text = input_text.get_untracked().trim().to_string();
        if text.is_empty() || sending.get_untracked() || replaying.get_untracked() {
            return;
        }

        let uid = next_id.get_untracked();
        next_id.set(uid + 2); // reserve uid for user msg, uid+1 for assistant reply

        let user_msg = ChatMessage::new_user(uid, &text);
        let assistant_msg = ChatMessage::new_assistant(uid + 1);

        messages.update(|v| v.push(user_msg));
        messages.update(|v| v.push(assistant_msg));
        in_flight.set(Some(assistant_msg));
        sending.set(true);
        input_text.set(String::new());

        bridge::send(&text);
    };

    // Whether input should be disabled.
    let input_disabled = move || sending.get() || replaying.get();

    view! {
        <div class="gander-root">
            <Sidebar sessions active_session_id />
            <div class="gander-chat">
                <MessageList messages />
                <div class="input-row">
                    <textarea
                        class="input-box"
                        rows="3"
                        placeholder=move || {
                            if replaying.get() { "Loading session…" } else { "Message…" }
                        }
                        prop:value=input_text
                        prop:disabled=input_disabled
                        on:input=move |ev| {
                            let el: web_sys::HtmlTextAreaElement =
                                ev.target().unwrap().unchecked_into();
                            input_text.set(el.value());
                        }
                        on:keydown=move |ev: web_sys::KeyboardEvent| {
                            // Enter (without Shift) submits; Shift+Enter inserts a newline.
                            if ev.key() == "Enter" && !ev.shift_key() {
                                ev.prevent_default();
                                submit();
                            }
                        }
                    />
                    <button
                        class="send-btn"
                        disabled=input_disabled
                        on:click=move |_| submit()
                    >
                        "Send"
                    </button>
                </div>
                <Footer cwd=footer_cwd model=footer_model tool_count=footer_tool_count />
            </div>
        </div>
    }
}

/// Left-edge session sidebar.
///
/// Shows up to 5 sessions, a "+ New session" button, and highlights the
/// active session.
#[component]
fn Sidebar(
    sessions: RwSignal<Vec<SessionEntry>>,
    active_session_id: RwSignal<Option<String>>,
) -> impl IntoView {
    let on_new = move |_| {
        bridge::post_json(r#"{"type":"session_new"}"#);
    };

    view! {
        <div class="sidebar">
            <button class="new-session-btn" on:click=on_new>
                "+ New session"
            </button>
            <div class="session-list">
                <For
                    each=move || sessions.get()
                    key=|s| s.id.clone()
                    children=move |entry| {
                        let id = entry.id.clone();
                        let label = entry.label.clone();
                        let ago = entry
                            .last_active
                            .as_deref()
                            .map(time_ago)
                            .unwrap_or_default();
                        let id_for_click = id.clone();
                        let is_active = move || {
                            active_session_id
                                .get()
                                .as_deref()
                                .map(|a| a == id.as_str())
                                .unwrap_or(false)
                        };
                        let on_click = move |_| {
                            // Build the message as a proper JS object to avoid
                            // manual JSON escaping and potential injection.
                            let obj = js_sys::Object::new();
                            let _ = js_sys::Reflect::set(
                                &obj,
                                &JsValue::from_str("type"),
                                &JsValue::from_str("session_select"),
                            );
                            let _ = js_sys::Reflect::set(
                                &obj,
                                &JsValue::from_str("id"),
                                &JsValue::from_str(&id_for_click),
                            );
                            bridge::post_value(obj.into());
                        };
                        view! {
                            <button
                                class=move || {
                                    if is_active() {
                                        "session-item session-item--active"
                                    } else {
                                        "session-item"
                                    }
                                }
                                on:click=on_click
                            >
                                <span class="session-label">{label}</span>
                                <span class="session-ago">{ago}</span>
                            </button>
                        }
                    }
                />
            </div>
        </div>
    }
}

/// Scrollable list of all chat messages.
#[component]
fn MessageList(messages: RwSignal<Vec<ChatMessage>>) -> impl IntoView {
    view! {
        <div class="message-list">
            <For
                each=move || messages.get()
                key=|msg| msg.id
                children=|msg| view! { <MessageView message=msg /> }
            />
        </div>
    }
}

/// A single message bubble (user, assistant, or tool).
///
/// The `inner_html` on `.message-content` lets pulldown-cmark HTML land
/// directly in the DOM.  Content is trusted (local bridge only).
#[component]
fn MessageView(message: ChatMessage) -> impl IntoView {
    let role_class = match message.role {
        Role::User => "message message--user",
        Role::Assistant => "message message--assistant",
        Role::Tool => "message message--tool",
    };

    view! {
        <div class=role_class>
            // Reactive inner HTML: re-evaluates only when `content` changes.
            <div
                class="message-content"
                inner_html=move || markdown::render(&message.content.get())
            />
            // Blinking cursor while the host is still streaming tokens.
            {move || {
                message
                    .streaming
                    .get()
                    .then(|| view! { <span class="streaming-cursor">"▋"</span> })
            }}
            // Error notice, shown only on failure.
            {move || {
                message
                    .error
                    .get()
                    .map(|e| view! { <div class="error-notice">{e}</div> })
            }}
        </div>
    }
}

/// Footer bar showing session metadata below the input row.
///
/// Each field is driven by its own signal so only the changed span re-renders.
/// Fields that have not yet been populated show `—` (em-dash).
///
/// ## Layout
///
/// ```text
/// 📁 /home/…  📎  |  — / —  |  model  |  —  |  N tools  |  ⚙
///   cwd      attach  tokens    model   mode   tools   settings
/// ```
#[component]
fn Footer(
    cwd: RwSignal<Option<String>>,
    model: RwSignal<Option<String>>,
    tool_count: RwSignal<Option<u32>>,
) -> impl IntoView {
    view! {
        <div class="input-footer">
            // ── cwd ────────────────────────────────────────────────────────
            <span class="footer-cwd" title=move || cwd.get().unwrap_or_default()>
                <Icon icon=icondata::LuFolder width="14px" height="14px" />
                {move || cwd.get().unwrap_or_else(|| "—".to_string())}
            </span>

            // ── attach (no-op placeholder) ─────────────────────────────────
            <button
                class="footer-btn"
                title="Attach file (not implemented)"
                on:click=|_| {
                    web_sys::console::log_1(
                        &wasm_bindgen::JsValue::from_str("attach not implemented"),
                    );
                }
            >
                <Icon icon=icondata::LuPaperclip width="14px" height="14px" />
            </button>

            <span class="footer-sep">"|"</span>

            // ── token usage (placeholder) ──────────────────────────────────
            <span class="footer-tokens">"— / —"</span>

            <span class="footer-sep">"|"</span>

            // ── model ──────────────────────────────────────────────────────
            <span class="footer-model">
                {move || model.get().unwrap_or_else(|| "—".to_string())}
            </span>

            <span class="footer-sep">"|"</span>

            // ── mode (placeholder) ─────────────────────────────────────────
            <span class="footer-mode">"—"</span>

            <span class="footer-sep">"|"</span>

            // ── tool count ─────────────────────────────────────────────────
            <span class="footer-tools">
                {move || {
                    tool_count
                        .get()
                        .map(|n| format!("{n} tools"))
                        .unwrap_or_else(|| "—".to_string())
                }}
            </span>

            <span class="footer-sep">"|"</span>

            // ── settings (no-op placeholder) ──────────────────────────────
            <button
                class="footer-btn"
                title="Settings (not implemented)"
                on:click=|_| {
                    web_sys::console::log_1(
                        &wasm_bindgen::JsValue::from_str("settings not implemented"),
                    );
                }
            >
                <Icon icon=icondata::LuSettings2 width="14px" height="14px" />
            </button>
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
